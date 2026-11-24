use std::collections::BTreeMap;
use std::future::pending;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alexandria_engine::refinement::policy::{HeaderValue, Protocol, Route};
use alexandria_mcp::refinement::inference::{
    AttemptQuota, FinishReason, InferenceClient, InferenceError, InferenceReply, InferenceRequest,
    QuotaDenied, Usage,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::util::SubscriberInitExt;

use InferenceError as Error;
use Protocol::{AnthropicMessages as Anthropic, OpenAiChatCompletions as OpenAi};

const TOKEN: &str = "fixture-provider-key-do-not-echo";
const GATEWAY_KEY: &str = "fixture-gateway-key-do-not-echo";
const BEARER: &str = "Bearer fixture-explicit-anthropic-bearer";
const PROBE_WAIT: Duration = Duration::from_secs(4);

fn fixture_credentials(reference: &str) -> Option<String> {
    Some(
        match reference {
            "PROVIDER_KEY_REF" => TOKEN,
            "GATEWAY_KEY_REF" => GATEWAY_KEY,
            "ANTHROPIC_BEARER_REF" => BEARER,
            _ => return None,
        }
        .to_string(),
    )
}

fn client() -> InferenceClient {
    InferenceClient::new(false, fixture_credentials)
}

#[derive(Clone)]
struct Quota {
    reservations: Arc<AtomicUsize>,
    allowed: usize,
}

impl Default for Quota {
    fn default() -> Self {
        Self::allow(usize::MAX)
    }
}

impl Quota {
    fn allow(allowed: usize) -> Self {
        Self {
            reservations: Arc::default(),
            allowed,
        }
    }

    fn count(&self) -> usize {
        self.reservations.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl AttemptQuota for Quota {
    async fn reserve_attempt(&self) -> Result<(), QuotaDenied> {
        let ordinal = self.reservations.fetch_add(1, Ordering::SeqCst);
        (ordinal < self.allowed).then_some(()).ok_or(QuotaDenied)
    }
}

fn request<'a>(prompt: &'a Value, quota: &'a Quota) -> InferenceRequest<'a> {
    InferenceRequest {
        prompt,
        max_request_bytes: 4096,
        max_response_bytes: 8192,
        max_output_tokens: 37,
        deadline: Instant::now() + Duration::from_secs(8),
        quota,
    }
}

#[derive(Clone, Debug)]
struct Seen {
    at: Instant,
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Value,
    reservations: usize,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

enum Reply {
    Http {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    },
    Disconnect,
    ChunkThenHold(Vec<u8>),
}

impl Reply {
    fn http(status: u16, body: Value) -> Self {
        Self::Http {
            status,
            headers: Vec::new(),
            body: body.to_string(),
        }
    }
}

struct Fixture {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    observed: Arc<Notify>,
    task: JoinHandle<()>,
}

impl Fixture {
    async fn start(script: Vec<Reply>, quota: &Quota) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::new(Notify::new());
        let task = tokio::spawn(serve(
            listener,
            script,
            quota.clone(),
            Arc::clone(&seen),
            Arc::clone(&observed),
        ));
        Self {
            addr,
            seen,
            observed,
            task,
        }
    }

    fn route(&self, protocol: Protocol) -> Route {
        Route {
            protocol,
            api_prefix: format!("http://{}/gateway//V1/", self.addr),
            model: "configured-model".into(),
            credential_ref: None,
            headers: BTreeMap::new(),
            output_limit_field: None,
        }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    async fn wait_for_request(&self) {
        timeout(PROBE_WAIT, async {
            while self.seen.lock().unwrap().is_empty() {
                self.observed.notified().await;
            }
        })
        .await
        .expect("inference did not reach the loopback fixture");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    listener: TcpListener,
    script: Vec<Reply>,
    quota: Quota,
    seen: Arc<Mutex<Vec<Seen>>>,
    observed: Arc<Notify>,
) {
    let mut script = script.into_iter();
    let mut connections = JoinSet::new();
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        let seen = Arc::clone(&seen);
        let observed = Arc::clone(&observed);
        let quota = quota.clone();
        let reply = script.next().unwrap_or_else(|| Reply::http(418, json!({})));
        connections.spawn(async move {
            let Ok(mut request) = read_request(&mut socket).await else {
                return;
            };
            request.reservations = quota.count();
            seen.lock().unwrap().push(request);
            observed.notify_one();
            // Closing a cancelled request may race the fixture's write; that's not a test failure.
            let _ = send_reply(&mut socket, reply).await;
        });
    }
}

async fn read_request(socket: &mut TcpStream) -> io::Result<Seen> {
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let mut request_line = line.split_ascii_whitespace();
    let method = request_line.next().unwrap().to_string();
    let target = request_line.next().unwrap().to_string();
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
    }
    let len = headers
        .get("content-length")
        .map_or(0, |s| s.parse().unwrap());
    let mut body = vec![0; len];
    reader.read_exact(&mut body).await?;
    Ok(Seen {
        at: Instant::now(),
        method,
        target,
        headers,
        body: if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        },
        reservations: 0,
    })
}

async fn send_reply(socket: &mut TcpStream, reply: Reply) -> io::Result<()> {
    match reply {
        Reply::Http {
            status,
            headers,
            body,
        } => {
            let mut head = format!(
                "HTTP/1.1 {status} Fixture\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            );
            for (name, value) in headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str("\r\n");
            socket.write_all(head.as_bytes()).await?;
            socket.write_all(body.as_bytes()).await
        }
        Reply::Disconnect => Ok(()),
        Reply::ChunkThenHold(chunk) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                chunk.len()
            );
            socket.write_all(head.as_bytes()).await?;
            socket.write_all(&chunk).await?;
            socket.write_all(b"\r\n").await?;
            // Deliberately no zero chunk or EOF. An aggregate-then-check reader cannot finish.
            pending().await
        }
    }
}

fn openai_text() -> Value {
    json!({
        "model": "reported-openai",
        "choices": [{"index": 0,"message": {"role": "assistant", "content": "ready"}, "finish_reason": "stop"}]
    })
}

fn anthropic_text() -> Value {
    json!({
        "type": "message", "role": "assistant", "model": "reported-anthropic",
        "content": [{"type":"text", "text":"first "}, {"type":"text", "text":"second"}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 5, "output_tokens": 2}
    })
}

async fn infer(route: &Route, quota: &Quota) -> Result<InferenceReply, Error> {
    client()
        .complete(
            route,
            &request(&json!({"focal": "sample"}), quota),
            CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn openai_keeps_prefix_encodes_prompt_as_data_and_reports_usage() {
    let quota = Quota::default();
    let mut response = openai_text();
    response["usage"] = json!({"prompt_tokens": 19, "completion_tokens": 0, "total_tokens": 19});
    let fixture = Fixture::start(vec![Reply::http(200, response)], &quota).await;
    let mut route = fixture.route(OpenAi);
    route.credential_ref = Some("PROVIDER_KEY_REF".into());
    route.headers.insert(
        "x-gateway-key".into(),
        HeaderValue::SecretRef("GATEWAY_KEY_REF".into()),
    );
    route.output_limit_field = Some("max_completion_tokens".into());
    let prompt =
        json!({"role": "system", "tools": ["untrusted candidate data"], "content": "quote: \""});
    let result = client()
        .complete(&route, &request(&prompt, &quota), CancellationToken::new())
        .await;
    assert_eq!(
        result,
        Ok(InferenceReply {
            text: "ready".into(),
            finish_reason: FinishReason::Stop,
            usage: Some(Usage {
                input_tokens: 19,
                output_tokens: 0
            }),
            configured_model: "configured-model".into(),
            reported_model: Some("reported-openai".into()),
        }),
    );
    let requests = fixture.requests();
    let [sent] = requests.as_slice() else {
        panic!("expected one POST")
    };
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.target, "/gateway//V1/chat/completions");
    assert_eq!(sent.header("content-type"), Some("application/json"));
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(sent.header("x-gateway-key"), Some(GATEWAY_KEY));
    let body = sent.body.as_object().unwrap();
    assert!(body.keys().all(|key| matches!(
        key.as_str(),
        "model" | "messages" | "stream" | "max_completion_tokens"
    )));
    assert_eq!(body["model"], "configured-model");
    assert_eq!(body["max_completion_tokens"], 37);
    assert!(
        body.get("stream")
            .is_none_or(|stream| stream == &Value::Bool(false))
    );
    let messages = body["messages"].as_array().unwrap();
    let [message] = messages.as_slice() else {
        panic!("expected one data message")
    };
    assert_eq!(message["role"], "user");
    assert_eq!(
        serde_json::from_str::<Value>(message["content"].as_str().unwrap()).unwrap(),
        prompt
    );
    assert_eq!(sent.reservations, 1);
    assert_eq!(quota.count(), 1);
}

#[tokio::test]
async fn openai_without_auth_uses_max_tokens_and_absent_usage_is_unknown() {
    for field in [None, Some("max_tokens")] {
        let quota = Quota::default();
        let fixture = Fixture::start(vec![Reply::http(200, openai_text())], &quota).await;
        let mut route = fixture.route(OpenAi);
        route.output_limit_field = field.map(str::to_owned);
        let reply = infer(&route, &quota)
            .await
            .expect("unauthenticated OpenAI fixture succeeds");
        assert_eq!(reply.usage, None);
        let requests = fixture.requests();
        let [sent] = requests.as_slice() else {
            panic!("expected one POST")
        };
        assert_eq!(sent.header("authorization"), None);
        assert_eq!(sent.header("x-api-key"), None);
        assert_eq!(sent.body["max_tokens"], 37);
        assert!(sent.body.get("max_completion_tokens").is_none());
    }
}

#[tokio::test]
async fn anthropic_messages_uses_version_key_or_explicit_bearer_and_joins_text_blocks() {
    for explicit_bearer in [false, true] {
        let quota = Quota::default();
        let mut response = anthropic_text();
        if explicit_bearer {
            response.as_object_mut().unwrap().remove("usage");
        }
        let fixture = Fixture::start(vec![Reply::http(200, response)], &quota).await;
        let mut route = fixture.route(Anthropic);
        if explicit_bearer {
            route.headers.insert(
                "authorization".into(),
                HeaderValue::SecretRef("ANTHROPIC_BEARER_REF".into()),
            );
        } else {
            route.credential_ref = Some("PROVIDER_KEY_REF".into());
        }
        assert_eq!(
            infer(&route, &quota).await,
            Ok(InferenceReply {
                text: "first second".into(),
                finish_reason: FinishReason::Stop,
                usage: (!explicit_bearer).then_some(Usage {
                    input_tokens: 5,
                    output_tokens: 2
                }),
                configured_model: "configured-model".into(),
                reported_model: Some("reported-anthropic".into()),
            })
        );
        let requests = fixture.requests();
        let [sent] = requests.as_slice() else {
            panic!("expected one POST")
        };
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.target, "/gateway//V1/messages");
        assert_eq!(sent.header("anthropic-version"), Some("2023-06-01"));
        assert_eq!(
            sent.header("x-api-key"),
            (!explicit_bearer).then_some(TOKEN)
        );
        assert_eq!(
            sent.header("authorization"),
            explicit_bearer.then_some(BEARER)
        );
        assert_eq!(sent.body["model"], "configured-model");
        assert_eq!(sent.body["max_tokens"], 37);
        assert!(sent.body.get("tools").is_none());
        assert!(sent.body.get("tool_choice").is_none());
        assert!(
            sent.body
                .get("stream")
                .is_none_or(|stream| stream == &Value::Bool(false))
        );
        assert_eq!(sent.reservations, 1);
    }
}

#[tokio::test]
async fn openai_rejects_invalid_mixed_truncated_and_refused_outputs_without_retry() {
    let good = openai_text();
    let mut invalid_json = Reply::http(200, Value::Null);
    if let Reply::Http { body, .. } = &mut invalid_json {
        *body = "{\"choices\":[".into();
    }
    let message = |value: Value| json!({"model":"reported-openai", "choices":[{"index":0, "message":value, "finish_reason":"stop"}]});
    let finish = |reason: &str| {
        let mut value = good.clone();
        value["choices"][0]["finish_reason"] = json!(reason);
        value
    };
    let mut multiple = good.clone();
    multiple["choices"]
        .as_array_mut()
        .unwrap()
        .push(good["choices"][0].clone());
    let invalid = |value| (Reply::http(200, value), Error::InvalidResponse);
    for (reply, expected) in [
        (invalid_json, Error::InvalidResponse),
        invalid(json!({"choices": []})),
        invalid(multiple),
        invalid(message(json!({"role":"assistant", "content": "  "}))),
        invalid(message(
            json!({"role":"assistant", "content": "ready", "tool_calls":[{"function":{"name":"execute"}}]}),
        )),
        invalid(message(
            json!({"role":"assistant", "content": "ready", "function_call":{"name":"legacy"}}),
        )),
        invalid(finish("tool_calls")),
        (Reply::http(200, finish("length")), Error::Truncated),
        (Reply::http(200, finish("content_filter")), Error::Refused),
        (
            Reply::http(
                200,
                message(json!({"role":"assistant", "content":"ready", "refusal":"declined"})),
            ),
            Error::Refused,
        ),
    ] {
        let quota = Quota::default();
        let fixture = Fixture::start(vec![reply, Reply::http(200, good.clone())], &quota).await;
        assert_eq!(infer(&fixture.route(OpenAi), &quota).await, Err(expected));
        assert_eq!(fixture.requests().len(), 1);
        assert_eq!(quota.count(), 1);
    }
}

#[tokio::test]
async fn anthropic_rejects_mixed_block_types_truncation_and_refusal_without_retry() {
    let good = anthropic_text();
    let change = |field: &str, value: Value| {
        let mut reply = good.clone();
        reply[field] = value;
        Reply::http(200, reply)
    };
    for (reply, expected) in [
        (change("content", json!([])), Error::InvalidResponse),
        (
            change(
                "content",
                json!([{"type":"text", "text":"ok"}, {"type":"tool_use", "name":"execute", "input":{}}]),
            ),
            Error::InvalidResponse,
        ),
        (
            change(
                "content",
                json!([{"type":"thinking", "thinking":"hidden"}, {"type":"text", "text":"ok"}]),
            ),
            Error::InvalidResponse,
        ),
        (
            change("content", json!([{"type":"text", "text":3}])),
            Error::InvalidResponse,
        ),
        (
            change("stop_reason", json!("tool_use")),
            Error::InvalidResponse,
        ),
        (change("stop_reason", json!("max_tokens")), Error::Truncated),
        (change("stop_reason", json!("refusal")), Error::Refused),
    ] {
        let quota = Quota::default();
        let fixture = Fixture::start(vec![reply, Reply::http(200, good.clone())], &quota).await;
        let mut route = fixture.route(Anthropic);
        route.credential_ref = Some("PROVIDER_KEY_REF".into());
        assert_eq!(infer(&route, &quota).await, Err(expected));
        assert_eq!(fixture.requests().len(), 1);
        assert_eq!(quota.count(), 1);
    }
}

#[tokio::test]
async fn request_byte_cap_stops_egress_before_reservation() {
    let quota = Quota::default();
    let fixture = Fixture::start(vec![Reply::http(200, openai_text())], &quota).await;
    let route = fixture.route(OpenAi);
    let prompt = json!({"candidate": "🦀".repeat(128)});
    let mut call = request(&prompt, &quota);
    call.max_request_bytes = 256;
    assert_eq!(
        client()
            .complete(&route, &call, CancellationToken::new())
            .await,
        Err(Error::RequestTooLarge)
    );
    assert!(fixture.requests().is_empty());
    assert_eq!(quota.count(), 0);
    assert!(infer(&route, &quota).await.is_ok());
    assert_eq!(fixture.requests().len(), 1);
}

#[tokio::test]
async fn response_byte_cap_rejects_a_live_chunked_body_before_eof() {
    let quota = Quota::default();
    let fixture = Fixture::start(vec![Reply::ChunkThenHold(vec![b'x'; 4096])], &quota).await;
    let prompt = json!({"focal": "sample"});
    let mut call = request(&prompt, &quota);
    call.max_response_bytes = 64;
    let route = fixture.route(OpenAi);
    let client = client();
    let result = timeout(
        PROBE_WAIT,
        client.complete(&route, &call, CancellationToken::new()),
    )
    .await;
    assert_eq!(
        result.expect("reader waited for EOF instead of enforcing the byte cap"),
        Err(Error::ResponseTooLarge)
    );
    assert_eq!(fixture.requests().len(), 1);
    assert_eq!(quota.count(), 1);
}

#[tokio::test]
async fn deadline_bounds_an_incomplete_response_body_and_the_whole_retry_budget() {
    for reply in [
        Reply::ChunkThenHold(b"{".to_vec()),
        Reply::http(429, json!({})),
    ] {
        let quota = Quota::default();
        let fixture = Fixture::start(vec![reply], &quota).await;
        let prompt = json!({"focal": "sample"});
        let mut call = request(&prompt, &quota);
        call.deadline = Instant::now() + Duration::from_millis(750);
        let route = fixture.route(OpenAi);
        let client = client();
        let result = timeout(
            PROBE_WAIT,
            client.complete(&route, &call, CancellationToken::new()),
        )
        .await;
        assert_eq!(
            result.expect("request deadline was not enforced"),
            Err(Error::DeadlineExceeded)
        );
        assert_eq!(fixture.requests().len(), 1);
        assert_eq!(quota.count(), 1);
    }
}

#[tokio::test]
async fn cancellation_stops_in_flight_body_and_retry_paths() {
    for reply in [
        Reply::ChunkThenHold(b"{".to_vec()),
        Reply::http(503, json!({})),
    ] {
        let quota = Quota::default();
        let fixture = Fixture::start(vec![reply], &quota).await;
        let prompt = json!({"focal": "sample"});
        let call = request(&prompt, &quota);
        let route = fixture.route(OpenAi);
        let client = client();
        let cancel = CancellationToken::new();
        let (result, ()) = tokio::join!(
            biased;
            timeout(PROBE_WAIT, client.complete(&route, &call, cancel.clone())),
            async {
                fixture.wait_for_request().await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                cancel.cancel();
            },
        );
        assert_eq!(
            result.expect("request ignored cancellation"),
            Err(Error::Cancelled)
        );
        assert_eq!(fixture.requests().len(), 1);
        assert_eq!(quota.count(), 1);
    }
}

#[tokio::test]
async fn already_cancelled_does_not_reserve_or_connect() {
    let quota = Quota::default();
    let fixture = Fixture::start(vec![Reply::http(200, openai_text())], &quota).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let prompt = json!({"focal": "sample"});
    let call = request(&prompt, &quota);
    assert_eq!(
        client()
            .complete(&fixture.route(OpenAi), &call, cancel)
            .await,
        Err(Error::Cancelled)
    );
    assert_eq!(quota.count(), 0);
    assert!(fixture.requests().is_empty());
}

#[tokio::test]
async fn retry_429_and_5xx_back_off_and_reserve_each_attempt() {
    let quota = Quota::allow(3);
    let fixture = Fixture::start(
        vec![
            Reply::http(429, json!({})),
            Reply::http(503, json!({})),
            Reply::http(200, openai_text()),
        ],
        &quota,
    )
    .await;
    assert!(infer(&fixture.route(OpenAi), &quota).await.is_ok());
    let requests = fixture.requests();
    let [first, second, third] = requests.as_slice() else {
        panic!("expected one post and two retries")
    };
    assert!(second.at.duration_since(first.at) >= Duration::from_secs(1));
    assert!(third.at.duration_since(second.at) >= Duration::from_secs(2));
    assert_eq!(
        [first.reservations, second.reservations, third.reservations],
        [1, 2, 3]
    );
    assert_eq!(quota.count(), 3);
    assert!(
        requests
            .iter()
            .all(|sent| sent.target == first.target && sent.body == first.body)
    );
}

#[tokio::test]
async fn retry_after_is_bounded_and_no_more_than_two_retries_are_made() {
    let quota = Quota::default();
    let oversized_retry_after = Reply::Http {
        status: 429,
        headers: vec![("Retry-After", "31536000".into())],
        body: "{}".into(),
    };
    let fixture = Fixture::start(
        vec![
            oversized_retry_after,
            Reply::http(503, json!({})),
            Reply::http(429, json!({})),
            Reply::http(200, openai_text()),
        ],
        &quota,
    )
    .await;
    assert_eq!(
        infer(&fixture.route(OpenAi), &quota).await,
        Err(Error::RateLimited)
    );
    assert_eq!(quota.count(), 3);
    assert_eq!(fixture.requests().len(), 3);
}

#[tokio::test]
async fn transport_disconnect_retries_with_a_fresh_quota_reservation() {
    let quota = Quota::allow(2);
    let fixture = Fixture::start(
        vec![Reply::Disconnect, Reply::http(200, openai_text())],
        &quota,
    )
    .await;
    assert!(infer(&fixture.route(OpenAi), &quota).await.is_ok());
    let requests = fixture.requests();
    let [first, second] = requests.as_slice() else {
        panic!("expected one transport retry")
    };
    assert!(second.at.duration_since(first.at) >= Duration::from_secs(1));
    assert_eq!([first.reservations, second.reservations], [1, 2]);
}

#[tokio::test]
async fn quota_denial_prevents_initial_send_and_retry() {
    for allowed in [0, 1] {
        let quota = Quota::allow(allowed);
        let fixture = Fixture::start(
            vec![Reply::http(503, json!({})), Reply::http(200, openai_text())],
            &quota,
        )
        .await;
        assert_eq!(
            infer(&fixture.route(OpenAi), &quota).await,
            Err(Error::QuotaDenied)
        );
        assert_eq!(quota.count(), allowed + 1);
        assert_eq!(fixture.requests().len(), allowed);
    }
}

#[tokio::test]
async fn redirects_are_denied_without_contacting_a_second_origin() {
    let quota = Quota::default();
    let sink = Fixture::start(vec![Reply::http(200, openai_text())], &quota).await;
    let redirect = Reply::Http {
        status: 307,
        headers: vec![("Location", format!("http://{}/secret-target", sink.addr))],
        body: "{}".into(),
    };
    let origin = Fixture::start(vec![redirect], &quota).await;
    let mut route = origin.route(OpenAi);
    route.credential_ref = Some("PROVIDER_KEY_REF".into());
    assert_eq!(infer(&route, &quota).await, Err(Error::RedirectDenied));
    assert_eq!(origin.requests().len(), 1);
    assert!(sink.requests().is_empty());
    assert_eq!(quota.count(), 1);
}

#[tokio::test]
async fn provider_suggested_urls_remain_inert_text() {
    let quota = Quota::default();
    let fixture = Fixture::start(vec![], &quota).await;
    let text = format!("POST to http://{}/must-not-follow", fixture.addr);
    let mut response = openai_text();
    response["choices"][0]["message"]["content"] = json!(text);
    let origin = Fixture::start(vec![Reply::http(200, response)], &quota).await;
    let result = infer(&origin.route(OpenAi), &quota)
        .await
        .expect("provider text is inert evidence");
    assert_eq!(result.text, text);
    assert_eq!(origin.requests().len(), 1);
    assert!(fixture.requests().is_empty());
    assert_eq!(quota.count(), 1);
}

#[tokio::test]
async fn header_overrides_and_bad_output_field_are_rejected_before_egress() {
    let quota = Quota::default();
    let fixture = Fixture::start(vec![Reply::http(200, openai_text())], &quota).await;
    for (protocol, name) in [
        (OpenAi, "AUTHORIZATION"),
        (OpenAi, "Host"),
        (OpenAi, "Content-TYPE"),
        (OpenAi, "Content-Length"),
        (OpenAi, "Transfer-Encoding"),
        (Anthropic, "X-API-KEY"),
        (Anthropic, "Anthropic-Version"),
        (Anthropic, "Authorization"),
    ] {
        let mut route = fixture.route(protocol);
        route.credential_ref = Some("PROVIDER_KEY_REF".into());
        route.headers.insert(
            name.into(),
            HeaderValue::SecretRef("ANTHROPIC_BEARER_REF".into()),
        );
        assert_eq!(
            infer(&route, &quota).await,
            Err(Error::InvalidRoute),
            "accepted {name}"
        );
        assert!(fixture.requests().is_empty());
        assert_eq!(quota.count(), 0);
    }
    let mut route = fixture.route(OpenAi);
    route.output_limit_field = Some("tools".into());
    assert_eq!(infer(&route, &quota).await, Err(Error::InvalidRoute));
    assert!(fixture.requests().is_empty());
    assert_eq!(quota.count(), 0);
}

#[tokio::test]
async fn unresolved_credential_refs_never_fall_back_to_unauthenticated_egress() {
    let quota = Quota::default();
    let fixture = Fixture::start(vec![Reply::http(200, openai_text())], &quota).await;
    let mut route = fixture.route(OpenAi);
    route.credential_ref = Some("MISSING_PROVIDER_KEY_REF".into());
    assert_eq!(infer(&route, &quota).await, Err(Error::MissingCredential));
    route.credential_ref = None;
    route.headers.insert(
        "x-gateway-key".into(),
        HeaderValue::SecretRef("MISSING_GATEWAY_KEY_REF".into()),
    );
    assert_eq!(infer(&route, &quota).await, Err(Error::MissingCredential));
    assert!(fixture.requests().is_empty());
    assert_eq!(quota.count(), 0);
}

#[tokio::test]
async fn wildcard_http_is_not_loopback_and_requires_explicit_opt_in() {
    let quota = Quota::default();
    let fixture = Fixture::start(vec![Reply::http(200, openai_text())], &quota).await;
    let mut route = fixture.route(OpenAi);
    // An unspecified local address, never an external fixture/DNS dependency.
    route.api_prefix = format!("http://0.0.0.0:{}/v1", fixture.addr.port());
    assert_eq!(infer(&route, &quota).await, Err(Error::InvalidRoute));
    assert!(fixture.requests().is_empty());
    assert_eq!(quota.count(), 0);
}

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn auth_and_other_4xx_do_not_retry_or_disclose_credentials_and_upstream_bodies() {
    const RESPONSE_MARKER: &str = "fixture-provider-error-content-do-not-echo";
    let logs = CapturedLogs::default();
    let writer = logs.clone();
    let _tracing = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish()
        .set_default();
    tracing::info!("fixture-log-capture-control");
    for (status, expected) in [
        (401, Error::Unauthorized),
        (403, Error::Unauthorized),
        (400, Error::HttpStatus(400)),
        (408, Error::HttpStatus(408)),
    ] {
        let quota = Quota::default();
        let response = json!({"error": format!("{RESPONSE_MARKER}: {TOKEN} {GATEWAY_KEY}")});
        let fixture = Fixture::start(
            vec![
                Reply::http(status, response),
                Reply::http(200, openai_text()),
            ],
            &quota,
        )
        .await;
        let mut route = fixture.route(OpenAi);
        route.credential_ref = Some("PROVIDER_KEY_REF".into());
        route.headers.insert(
            "x-gateway-key".into(),
            HeaderValue::SecretRef("GATEWAY_KEY_REF".into()),
        );
        let result = infer(&route, &quota).await;
        assert_eq!(result, Err(expected));
        let error = result.unwrap_err();
        let requests = fixture.requests();
        let [sent] = requests.as_slice() else {
            panic!("4xx must never trigger retry")
        };
        assert_eq!(
            sent.header("authorization"),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        assert_eq!(sent.header("x-gateway-key"), Some(GATEWAY_KEY));
        assert_eq!(quota.count(), 1);
        let logged = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
        assert!(logged.contains("fixture-log-capture-control"));
        let surfaces = format!("{error} {error:?} {logged}");
        for sensitive in [
            TOKEN,
            GATEWAY_KEY,
            RESPONSE_MARKER,
            "PROVIDER_KEY_REF",
            "GATEWAY_KEY_REF",
        ] {
            assert!(
                !surfaces.contains(sensitive),
                "sensitive data escaped error/log surfaces"
            );
        }
    }
}
