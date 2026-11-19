//! Transport boundary only: a provider reply is evidence, never mutation authority.
//!
//! API prefixes include the gateway and any version path (e.g. `/gateway/v1`). OpenAI
//! supports anonymous or Bearer requests and `max_tokens` or `max_completion_tokens`.
//! Anthropic uses `max_tokens`, version `2023-06-01`, and an API key or an explicit
//! `authorization` secret reference containing the complete Bearer value. That reference
//! is only allowed without `credential_ref`. Plain HTTP without opt-in is for literal
//! loopback IPs only. This is not a generic provider or proxy compatibility layer.

use std::fmt;
use std::io::{self, Write};
use std::net::IpAddr;
use std::time::Duration;

use alexandria_engine::refinement::policy::{HeaderValue as RouteHeaderValue, Protocol, Route};
use reqwest::header::{
    AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER,
};
use reqwest::{Client, Request, Response, StatusCode, Url};
use serde::Serialize;
use serde_json::Value;
use tokio::time::{Instant, sleep, sleep_until};
use tokio_util::sync::CancellationToken;

use super::{anthropic, openai};

type CredentialLookup = dyn Fn(&str) -> Option<String> + Send + Sync;

#[derive(Debug, thiserror::Error)]
#[error("inference attempt not admitted")]
pub struct QuotaDenied;

/// Bound by the caller to one work item and its current policy/consent. Each outbound
/// attempt, including a retry, needs a fresh reservation before sending any bytes.
#[async_trait::async_trait]
pub trait AttemptQuota: Send + Sync {
    async fn reserve_attempt(&self) -> Result<(), QuotaDenied>;
}

pub struct InferenceRequest<'a> {
    pub prompt: &'a Value,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    /// Requested provider limit, not a locally tokenized count.
    pub max_output_tokens: u32,
    /// A single absolute deadline for reservations, attempts, and backoff.
    pub deadline: Instant,
    pub quota: &'a dyn AttemptQuota,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(PartialEq, Eq)]
pub struct InferenceReply {
    pub text: String,
    pub finish_reason: FinishReason,
    /// `None` means unknown, not zero.
    pub usage: Option<Usage>,
    pub configured_model: String,
    pub reported_model: Option<String>,
}

impl InferenceReply {
    pub(super) fn text(
        text: String,
        usage: Option<Usage>,
        model: &str,
        reported_model: Option<String>,
    ) -> Result<Self, InferenceError> {
        if text.trim().is_empty() {
            return Err(InferenceError::InvalidResponse);
        }
        Ok(Self {
            text,
            finish_reason: FinishReason::Stop,
            usage,
            configured_model: model.to_owned(),
            reported_model,
        })
    }
}

impl fmt::Debug for InferenceReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Provider-controlled text and metadata may echo private request material.
        f.debug_struct("InferenceReply")
            .field("finish_reason", &self.finish_reason)
            .field("usage", &self.usage)
            .finish_non_exhaustive()
    }
}

/// Classified errors only. Do not attach URLs, header values, prompts, response bodies,
/// or upstream errors as sources: callers may log this type with `Display` or `Debug`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InferenceError {
    #[error("invalid inference route")]
    InvalidRoute,
    #[error("missing inference credential")]
    MissingCredential,
    #[error("invalid inference request")]
    InvalidRequest,
    #[error("inference request exceeds the byte limit")]
    RequestTooLarge,
    #[error("inference response exceeds the byte limit")]
    ResponseTooLarge,
    #[error("inference cancelled")]
    Cancelled,
    #[error("inference deadline exceeded")]
    DeadlineExceeded,
    #[error("inference attempt not admitted")]
    QuotaDenied,
    #[error("inference redirect denied")]
    RedirectDenied,
    #[error("inference provider rejected authentication")]
    Unauthorized,
    #[error("inference provider rate limit")]
    RateLimited,
    #[error("inference transport failed")]
    Transport,
    #[error("inference provider returned HTTP {0}")]
    HttpStatus(u16),
    #[error("invalid inference response")]
    InvalidResponse,
    #[error("truncated inference response")]
    Truncated,
    #[error("inference provider refused the request")]
    Refused,
}

impl InferenceError {
    fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport | Self::RateLimited | Self::HttpStatus(500..=599)
        )
    }
}

pub struct InferenceClient {
    allow_insecure_remote_http: bool,
    credentials: Box<CredentialLookup>,
    http: Option<Client>,
}

impl InferenceClient {
    pub fn new(
        allow_insecure_remote_http: bool,
        credentials: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        // Keep defaults for TLS certificate/hostname verification. Quota and route policy
        // cannot govern transparent redirects, proxy egress, or another retry layer.
        let http = Client::builder()
            .tls_backend_rustls()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .build()
            .ok();
        Self {
            allow_insecure_remote_http,
            credentials: Box::new(credentials),
            http,
        }
    }

    pub async fn complete(
        &self,
        route: &Route,
        request: &InferenceRequest<'_>,
        cancel: CancellationToken,
    ) -> Result<InferenceReply, InferenceError> {
        check_active(request.deadline, &cancel)?;
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(InferenceError::Cancelled),
            () = sleep_until(request.deadline) => Err(InferenceError::DeadlineExceeded),
            result = self.complete_active(route, request, &cancel) => result,
        }
    }

    async fn complete_active(
        &self,
        route: &Route,
        request: &InferenceRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<InferenceReply, InferenceError> {
        let http = self.http.as_ref().ok_or(InferenceError::Transport)?;
        let prepared = self.prepare(http, route, request)?;
        let mut backoffs = [Duration::from_secs(1), Duration::from_secs(2)].into_iter();
        loop {
            check_active(request.deadline, cancel)?;
            let outbound = prepared.try_clone().ok_or(InferenceError::InvalidRequest)?;
            let reservation = request.quota.reserve_attempt().await;
            check_active(request.deadline, cancel)?;
            reservation.map_err(|_| InferenceError::QuotaDenied)?;
            let result = attempt(http, outbound, route, request.max_response_bytes).await;
            check_active(request.deadline, cancel)?;
            match result {
                Ok(reply) => return Ok(reply),
                Err(failure) => {
                    let Some(backoff) = backoffs.next().filter(|_| failure.error.retryable())
                    else {
                        return Err(failure.error);
                    };
                    sleep(failure.retry_after.unwrap_or(backoff)).await;
                }
            }
        }
    }

    fn prepare(
        &self,
        http: &Client,
        route: &Route,
        request: &InferenceRequest<'_>,
    ) -> Result<Request, InferenceError> {
        if route.model.trim().is_empty() {
            return Err(InferenceError::InvalidRoute);
        }
        if request.max_request_bytes == 0
            || request.max_response_bytes == 0
            || request.max_output_tokens == 0
        {
            return Err(InferenceError::InvalidRequest);
        }
        let url = endpoint(route, self.allow_insecure_remote_http)?;
        let headers = self.headers(route)?;
        let prompt = String::from_utf8(encode_request(request.prompt, request.max_request_bytes)?)
            .map_err(|_| InferenceError::InvalidRequest)?;
        let encode = match route.protocol {
            Protocol::OpenAiChatCompletions => openai::request_body,
            Protocol::AnthropicMessages => anthropic::request_body,
        };
        let body = encode(
            &route.model,
            &prompt,
            route.output_limit_field.as_deref(),
            request.max_output_tokens,
            request.max_request_bytes,
        )?;
        http.post(url)
            .headers(headers)
            .body(body)
            .build()
            .map_err(|_| InferenceError::InvalidRequest)
    }

    fn headers(&self, route: &Route) -> Result<HeaderMap, InferenceError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let is_anthropic = route.protocol == Protocol::AnthropicMessages;
        if is_anthropic {
            headers.insert(
                "anthropic-version",
                HeaderValue::from_static(anthropic::API_VERSION),
            );
        }
        if let Some(reference) = &route.credential_ref {
            let key = self.credential(reference)?;
            if is_anthropic {
                headers.insert("x-api-key", sensitive_header(&key)?);
            } else {
                headers.insert(AUTHORIZATION, bearer_header(&format!("Bearer {key}"))?);
            }
        }
        for (name, value) in &route.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| InferenceError::InvalidRoute)?;
            if headers.contains_key(&name) {
                return Err(InferenceError::InvalidRoute);
            }
            let header = match value {
                RouteHeaderValue::SecretRef(reference)
                    if is_anthropic && route.credential_ref.is_none() && name == AUTHORIZATION =>
                {
                    bearer_header(&self.credential(reference)?)?
                }
                _ if reserved_header(name.as_str()) => return Err(InferenceError::InvalidRoute),
                RouteHeaderValue::SecretRef(reference) => {
                    sensitive_header(&self.credential(reference)?)?
                }
                RouteHeaderValue::Literal(value) => sensitive_header(value)?,
            };
            headers.insert(name, header);
        }
        if is_anthropic
            && !(headers.contains_key("x-api-key") || headers.contains_key(AUTHORIZATION))
        {
            return Err(InferenceError::MissingCredential);
        }
        Ok(headers)
    }

    fn credential(&self, reference: &str) -> Result<String, InferenceError> {
        (self.credentials)(reference)
            .filter(|value| !value.trim().is_empty())
            .ok_or(InferenceError::MissingCredential)
    }
}

fn endpoint(route: &Route, allow_insecure_remote_http: bool) -> Result<Url, InferenceError> {
    let destination = route
        .destination()
        .map_err(|_| InferenceError::InvalidRoute)?;
    let is_loopback = destination
        .host
        .trim_matches(['[', ']'])
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_loopback());
    if destination.scheme == "http" && !(allow_insecure_remote_http || is_loopback) {
        return Err(InferenceError::InvalidRoute);
    }
    let mut url = Url::parse(&route.api_prefix).map_err(|_| InferenceError::InvalidRoute)?;
    let suffix = match route.protocol {
        Protocol::OpenAiChatCompletions => openai::ENDPOINT,
        Protocol::AnthropicMessages => anthropic::ENDPOINT,
    };
    let prefix = destination.api_prefix;
    let separator = if prefix.ends_with('/') { "" } else { "/" };
    url.set_path(&format!("{prefix}{separator}{suffix}"));
    Ok(url)
}

fn reserved_header(name: &str) -> bool {
    matches!(
        name,
        "authorization"
            | "x-api-key"
            | "anthropic-version"
            | "host"
            | "content-type"
            | "content-length"
            | "content-encoding"
            | "accept-encoding"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "upgrade"
            | "te"
            | "trailer"
            | "expect"
            | "range"
            | "forwarded"
            | "x-http-method-override"
            | "x-original-url"
            | "x-rewrite-url"
    ) || name.starts_with("proxy-")
        || name.starts_with("x-forwarded-")
}

fn sensitive_header(value: &str) -> Result<HeaderValue, InferenceError> {
    let mut header = HeaderValue::from_str(value).map_err(|_| InferenceError::InvalidRoute)?;
    header.set_sensitive(true);
    Ok(header)
}

fn bearer_header(value: &str) -> Result<HeaderValue, InferenceError> {
    if !value.split_once(' ').is_some_and(|(scheme, token)| {
        let token = token.trim_end_matches('=');
        scheme.eq_ignore_ascii_case("bearer")
            && !token.is_empty()
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~+/".contains(&b))
    }) {
        return Err(InferenceError::InvalidRoute);
    }
    sensitive_header(value)
}

fn check_active(deadline: Instant, cancel: &CancellationToken) -> Result<(), InferenceError> {
    if cancel.is_cancelled() {
        return Err(InferenceError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(InferenceError::DeadlineExceeded);
    }
    Ok(())
}

struct FailedAttempt {
    error: InferenceError,
    retry_after: Option<Duration>,
}

impl From<InferenceError> for FailedAttempt {
    fn from(error: InferenceError) -> Self {
        Self {
            error,
            retry_after: None,
        }
    }
}

async fn attempt(
    http: &Client,
    outbound: Request,
    route: &Route,
    max_bytes: usize,
) -> Result<InferenceReply, FailedAttempt> {
    let response = http
        .execute(outbound)
        .await
        .map_err(|_| InferenceError::Transport)?;
    let status = response.status();
    if status != StatusCode::OK {
        let error = match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => InferenceError::Unauthorized,
            StatusCode::TOO_MANY_REQUESTS => InferenceError::RateLimited,
            status if status.is_redirection() => InferenceError::RedirectDenied,
            status => InferenceError::HttpStatus(status.as_u16()),
        };
        // Only short delta-seconds values replace the default delay. No upstream body is needed.
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
            .filter(|seconds| *seconds <= 2)
            .map(Duration::from_secs);
        return Err(FailedAttempt { error, retry_after });
    }
    let body = read_limited(response, max_bytes).await?;
    match route.protocol {
        Protocol::OpenAiChatCompletions => openai::parse(&body, &route.model),
        Protocol::AnthropicMessages => anthropic::parse(&body, &route.model),
    }
    .map_err(Into::into)
}

async fn read_limited(mut response: Response, max_bytes: usize) -> Result<Vec<u8>, InferenceError> {
    if response
        .content_length()
        .is_some_and(|len| !usize::try_from(len).is_ok_and(|n| n <= max_bytes))
    {
        return Err(InferenceError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| InferenceError::Transport)?
    {
        if chunk.len() > max_bytes - body.len() {
            return Err(InferenceError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Serialize)]
pub(super) struct TextMessage<'a> {
    role: &'static str,
    content: &'a str,
}

impl<'a> TextMessage<'a> {
    pub(super) fn user(content: &'a str) -> Self {
        Self {
            role: "user",
            content,
        }
    }
}

pub(super) fn encode_request(
    value: &impl Serialize,
    limit: usize,
) -> Result<Vec<u8>, InferenceError> {
    let mut body = CappedBody {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    serde_json::to_writer(&mut body, value).map_err(|_| {
        if body.exceeded {
            InferenceError::RequestTooLarge
        } else {
            InferenceError::InvalidRequest
        }
    })?;
    Ok(body.bytes)
}

struct CappedBody {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl Write for CappedBody {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            self.exceeded = true;
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
