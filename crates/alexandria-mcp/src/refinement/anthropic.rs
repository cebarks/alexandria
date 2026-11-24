//! Minimal Anthropic Messages: accept only end-of-turn, text-only assistant output.

use serde::{Deserialize, Serialize};

use super::inference::{InferenceError, InferenceReply, TextMessage, Usage, encode_request};

pub(super) const ENDPOINT: &str = "messages";
pub(super) const API_VERSION: &str = "2023-06-01";

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    messages: [TextMessage<'a>; 1],
    stream: bool,
    max_tokens: u32,
}

pub(super) fn request_body(
    model: &str,
    prompt: &str,
    output_limit_field: Option<&str>,
    max_tokens: u32,
    max_bytes: usize,
) -> Result<Vec<u8>, InferenceError> {
    if !matches!(output_limit_field, None | Some("max_tokens")) {
        return Err(InferenceError::InvalidRoute);
    }
    encode_request(
        &MessagesRequest {
            model,
            messages: [TextMessage::user(prompt)],
            stream: false,
            max_tokens,
        },
        max_bytes,
    )
}

#[derive(Deserialize)]
struct MessagesResponse {
    #[serde(rename = "type")]
    kind: String,
    role: String,
    model: Option<String>,
    content: Vec<Block>,
    stop_reason: String,
    usage: Option<MessagesUsage>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        text: String,
    },
    #[serde(other)]
    Unsupported,
}

#[derive(Deserialize)]
struct MessagesUsage {
    input_tokens: u64,
    output_tokens: u64,
}

pub(super) fn parse(body: &[u8], model: &str) -> Result<InferenceReply, InferenceError> {
    let reply: MessagesResponse =
        serde_json::from_slice(body).map_err(|_| InferenceError::InvalidResponse)?;
    if reply.kind != "message" || reply.role != "assistant" {
        return Err(InferenceError::InvalidResponse);
    }
    match reply.stop_reason.as_str() {
        "end_turn" => {}
        "max_tokens" | "model_context_window_exceeded" => return Err(InferenceError::Truncated),
        "refusal" => return Err(InferenceError::Refused),
        _ => return Err(InferenceError::InvalidResponse),
    }
    let mut text = String::new();
    for block in reply.content {
        let Block::Text { text: part } = block else {
            return Err(InferenceError::InvalidResponse);
        };
        text.push_str(&part);
    }
    let usage = reply.usage.map(|value| Usage {
        input_tokens: value.input_tokens,
        output_tokens: value.output_tokens,
    });
    InferenceReply::text(text, usage, model, reply.model)
}
