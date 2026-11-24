//! Minimal OpenAI Chat Completions: one text message, one completed text choice.

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use super::inference::{InferenceError, InferenceReply, TextMessage, Usage, encode_request};

pub(super) const ENDPOINT: &str = "chat/completions";

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [TextMessage<'a>; 1],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
}

pub(super) fn request_body(
    model: &str,
    prompt: &str,
    output_limit_field: Option<&str>,
    max_tokens: u32,
    max_bytes: usize,
) -> Result<Vec<u8>, InferenceError> {
    let (max_tokens, max_completion_tokens) = match output_limit_field {
        None | Some("max_tokens") => (Some(max_tokens), None),
        Some("max_completion_tokens") => (None, Some(max_tokens)),
        Some(_) => return Err(InferenceError::InvalidRoute),
    };
    encode_request(
        &ChatRequest {
            model,
            messages: [TextMessage::user(prompt)],
            stream: false,
            max_tokens,
            max_completion_tokens,
        },
        max_bytes,
    )
}

#[derive(Deserialize)]
struct ChatResponse {
    model: Option<String>,
    choices: Vec<Choice>,
    usage: Option<ChatUsage>,
}

#[derive(Deserialize)]
struct Choice {
    finish_reason: String,
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    role: String,
    content: Option<String>,
    refusal: Option<String>,
    tool_calls: Option<Vec<IgnoredAny>>,
    function_call: Option<IgnoredAny>,
    audio: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct ChatUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

pub(super) fn parse(body: &[u8], model: &str) -> Result<InferenceReply, InferenceError> {
    let reply: ChatResponse =
        serde_json::from_slice(body).map_err(|_| InferenceError::InvalidResponse)?;
    let [choice]: [Choice; 1] = reply
        .choices
        .try_into()
        .map_err(|_| InferenceError::InvalidResponse)?;
    let message = choice.message;
    if message.refusal.is_some() || choice.finish_reason == "content_filter" {
        return Err(InferenceError::Refused);
    }
    match choice.finish_reason.as_str() {
        "stop" => {}
        "length" => return Err(InferenceError::Truncated),
        _ => return Err(InferenceError::InvalidResponse),
    }
    if message.role != "assistant"
        || message.tool_calls.is_some_and(|calls| !calls.is_empty())
        || message.function_call.is_some()
        || message.audio.is_some()
    {
        return Err(InferenceError::InvalidResponse);
    }
    let text = message.content.ok_or(InferenceError::InvalidResponse)?;
    let usage = reply.usage.map(|value| Usage {
        input_tokens: value.prompt_tokens,
        output_tokens: value.completion_tokens,
    });
    InferenceReply::text(text, usage, model, reply.model)
}
