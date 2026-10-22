// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! The language models the assistant talks to, and the one conversation
//! shape both kinds of API are written from.
//!
//! Two wire formats cover every provider offered: Anthropic's Messages API
//! for Claude, and the OpenAI chat-completions format that OpenAI, DeepSeek,
//! OpenRouter, Ollama and most others speak. The conversation is kept in a
//! shape of its own ([`Entry`]) and written out in the provider's format on
//! every request; an assistant turn also keeps its reply exactly as the
//! provider sent it, because Claude's thinking blocks must go back to it
//! unchanged.
//!
//! Requests are plain HTTPS through `ureq`, on a worker thread: Rust has no
//! official Anthropic SDK, so the Messages API is called as its reference
//! documents it.

use std::time::Duration;

use serde_json::{Value, json};

/// The two wire formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Anthropic's Messages API.
    Anthropic,
    /// OpenAI's chat completions, which most providers also speak.
    OpenAi,
}

/// A provider the settings list, with what it needs.
#[derive(Clone, Copy, Debug)]
pub struct Preset {
    /// Stable id: preferences and the key store name the provider by it.
    pub id: &'static str,
    /// What a person is shown.
    pub label: &'static str,
    pub kind: Kind,
    /// Where requests go; empty for one the person must give.
    pub base_url: &'static str,
    /// Models offered in the list, the default first. Empty: the person
    /// types one.
    pub models: &'static [&'static str],
    /// Whether requests need an API key.
    pub needs_key: bool,
    /// Whether the models take pictures: preview frames are sent to them.
    pub vision: bool,
    /// Where a key is made, for the settings' hint.
    pub key_page: &'static str,
}

/// Every provider the settings offer, in order.
pub const PRESETS: &[Preset] = &[
    Preset {
        id: "anthropic",
        label: "Anthropic (Claude)",
        kind: Kind::Anthropic,
        base_url: "https://api.anthropic.com",
        models: &["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"],
        needs_key: true,
        vision: true,
        key_page: "console.anthropic.com",
    },
    Preset {
        id: "openai",
        label: "OpenAI",
        kind: Kind::OpenAi,
        base_url: "https://api.openai.com/v1",
        models: &[],
        needs_key: true,
        vision: true,
        key_page: "platform.openai.com",
    },
    Preset {
        id: "deepseek",
        label: "DeepSeek",
        kind: Kind::OpenAi,
        base_url: "https://api.deepseek.com/v1",
        models: &["deepseek-chat"],
        needs_key: true,
        vision: false,
        key_page: "platform.deepseek.com",
    },
    Preset {
        id: "openrouter",
        label: "OpenRouter",
        kind: Kind::OpenAi,
        base_url: "https://openrouter.ai/api/v1",
        models: &[],
        needs_key: true,
        vision: true,
        key_page: "openrouter.ai",
    },
    Preset {
        id: "ollama",
        label: "Ollama (local)",
        kind: Kind::OpenAi,
        base_url: "http://localhost:11434/v1",
        models: &[],
        needs_key: false,
        vision: false,
        key_page: "",
    },
    Preset {
        id: "custom",
        label: "Custom (OpenAI-compatible)",
        kind: Kind::OpenAi,
        base_url: "",
        models: &[],
        needs_key: false,
        vision: false,
        key_page: "",
    },
];

/// What one request is sent with.
#[derive(Clone, Debug)]
pub struct Settings {
    pub preset: &'static Preset,
    pub model: String,
    pub base_url: String,
    pub key: Option<String>,
}

/// A tool as a model is told about it.
#[derive(Clone, Debug)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// A tool the model asked for.
#[derive(Clone, Debug)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// A tool's answer.
#[derive(Clone, Debug)]
pub struct ToolOutput {
    pub id: String,
    pub text: String,
    /// PNGs, base64.
    pub images: Vec<String>,
    pub is_error: bool,
}

/// One step of a conversation.
#[derive(Clone, Debug)]
pub enum Entry {
    User(String),
    Assistant {
        text: String,
        calls: Vec<Call>,
        /// The reply as the provider sent it: Claude's content blocks,
        /// thinking included, go back exactly as they came.
        raw: Value,
    },
    Results(Vec<ToolOutput>),
}

/// Why a reply ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Stop {
    /// The model is done.
    Done,
    /// The model asked for tools.
    Tools,
    /// The reply hit its length limit.
    Truncated,
    /// The model declined; why, when it says.
    Refused(String),
}

/// One reply.
#[derive(Clone, Debug)]
pub struct Turn {
    pub text: String,
    pub calls: Vec<Call>,
    pub raw: Value,
    pub stop: Stop,
    /// Tokens read and written.
    pub usage: (u64, u64),
}

/// The longest one reply may take: a model thinking before a long answer.
const PATIENCE: Duration = Duration::from_secs(600);

/// The most one reply may write. A chat turn and its tool calls fit well
/// within it; it stays under the size a non-streaming request can return
/// before HTTP timeouts.
const MAX_TOKENS: u32 = 16_000;

/// Sends the conversation and returns the model's next turn.
pub fn send(
    settings: &Settings,
    system: &str,
    tools: &[ToolDef],
    history: &[Entry],
) -> Result<Turn, String> {
    if settings.model.trim().is_empty() {
        return Err("choose a model in the assistant's settings".to_owned());
    }
    match settings.preset.kind {
        Kind::Anthropic => anthropic(settings, system, tools, history),
        Kind::OpenAi => openai(settings, system, tools, history),
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(PATIENCE)
        .build()
}

/// POSTs `body` and returns the JSON answer, or the provider's own words
/// for what went wrong.
fn post(request: ureq::Request, body: &Value) -> Result<Value, String> {
    match request
        .set("content-type", "application/json")
        .send_string(&body.to_string())
    {
        Ok(response) => {
            let text = response.into_string().map_err(|error| error.to_string())?;
            serde_json::from_str(&text).map_err(|error| format!("the answer was not JSON: {error}"))
        }
        Err(ureq::Error::Status(code, response)) => {
            let text = response.into_string().unwrap_or_default();
            let said = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| {
                    value
                        .pointer("/error/message")
                        .or_else(|| value.pointer("/message"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| text.chars().take(300).collect());
            Err(match code {
                401 | 403 => format!("the provider refused the API key ({code}): {said}"),
                429 => format!("the provider is rate-limiting requests (429): {said}"),
                _ => format!("the provider answered {code}: {said}"),
            })
        }
        Err(error) => Err(format!("could not reach the provider: {error}")),
    }
}

fn joined(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// Model ids an OpenAI-compatible list carries that cannot chat: pictures,
/// speech, embeddings, moderation.
const NOT_CHAT: &[&str] = &[
    "embedding",
    "tts",
    "whisper",
    "dall-e",
    "moderation",
    "transcribe",
    "image",
    "audio",
    "realtime",
    "search",
];

/// The models the provider offers this key: what Connect shows to pick
/// from, and the proof the key and the address work. In the provider's own
/// order for Anthropic (newest first), alphabetical for the others.
pub fn list_models(settings: &Settings) -> Result<Vec<String>, String> {
    let key = settings.key.as_deref().filter(|key| !key.is_empty());
    let ids: Vec<String> = match settings.preset.kind {
        Kind::Anthropic => {
            let key = key.ok_or("paste an Anthropic API key first")?;
            let reply = get(agent()
                .get(&joined(&settings.base_url, "v1/models?limit=1000"))
                .set("x-api-key", key)
                .set("anthropic-version", "2023-06-01"))?;
            ids_of(&reply)
        }
        Kind::OpenAi => {
            if settings.base_url.trim().is_empty() {
                return Err("give the provider's address first".to_owned());
            }
            if settings.preset.needs_key && key.is_none() {
                return Err(format!("paste a {} API key first", settings.preset.label));
            }
            let mut request = agent().get(&joined(&settings.base_url, "models"));
            if let Some(key) = key {
                request = request.set("authorization", &format!("Bearer {key}"));
            }
            let mut ids: Vec<String> = ids_of(&get(request)?)
                .into_iter()
                .filter(|id| {
                    let lower = id.to_lowercase();
                    !NOT_CHAT.iter().any(|word| lower.contains(word))
                })
                .collect();
            ids.sort();
            ids
        }
    };
    if ids.is_empty() {
        return Err("the provider lists no models for this key".to_owned());
    }
    Ok(ids)
}

/// The `data[].id` of a model list.
fn ids_of(reply: &Value) -> Vec<String> {
    reply["data"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|model| model["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// GETs JSON, with the same error reading as [`post`].
fn get(request: ureq::Request) -> Result<Value, String> {
    match request.call() {
        Ok(response) => {
            let text = response.into_string().map_err(|error| error.to_string())?;
            serde_json::from_str(&text).map_err(|error| format!("the answer was not JSON: {error}"))
        }
        Err(ureq::Error::Status(code, response)) => {
            let text = response.into_string().unwrap_or_default();
            let said = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| {
                    value
                        .pointer("/error/message")
                        .or_else(|| value.pointer("/message"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| text.chars().take(300).collect());
            Err(match code {
                401 | 403 => format!("the key was not accepted ({code}): {said}"),
                404 => format!("nothing at that address (404); check it: {said}"),
                _ => format!("the provider answered {code}: {said}"),
            })
        }
        Err(error) => Err(format!("could not reach the provider: {error}")),
    }
}

// ── Anthropic ──────────────────────────────────────────────────────────────

/// Models that take `output_config.effort`.
fn takes_effort(model: &str) -> bool {
    model.starts_with("claude-") && PRESETS[0].models.contains(&model)
}

/// Models whose refusals can fall back to another model on the server.
fn takes_fallbacks(model: &str) -> bool {
    matches!(model, "claude-opus-5-5" | "claude-sonnet-5-5")
}

fn anthropic(
    settings: &Settings,
    system: &str,
    tools: &[ToolDef],
    history: &[Entry],
) -> Result<Turn, String> {
    let key = settings
        .key
        .as_deref()
        .filter(|key| !key.is_empty())
        .ok_or("add an Anthropic API key in the assistant's settings")?;
    let mut messages: Vec<Value> = Vec::new();
    // Consecutive user content goes into one message.
    let mut push = |role: &str, blocks: Vec<Value>| {
        if let Some(last) = messages.last_mut()
            && last["role"] == role
            && let Some(content) = last["content"].as_array_mut()
        {
            content.extend(blocks);
            return;
        }
        messages.push(json!({ "role": role, "content": blocks }));
    };
    for entry in history {
        match entry {
            Entry::User(text) => push("user", vec![json!({ "type": "text", "text": text })]),
            Entry::Assistant { raw, .. } => {
                let blocks = raw.as_array().cloned().unwrap_or_default();
                push("assistant", blocks);
            }
            Entry::Results(outputs) => {
                let blocks = outputs
                    .iter()
                    .map(|output| {
                        let mut content = vec![json!({ "type": "text", "text": output.text })];
                        for png in &output.images {
                            content.push(json!({
                                "type": "image",
                                "source": { "type": "base64", "media_type": "image/png", "data": png },
                            }));
                        }
                        json!({
                            "type": "tool_result",
                            "tool_use_id": output.id,
                            "content": content,
                            "is_error": output.is_error,
                        })
                    })
                    .collect();
                push("user", blocks);
            }
        }
    }
    let mut body = json!({
        "model": settings.model,
        "max_tokens": MAX_TOKENS,
        "system": system,
        "tools": tools.iter().map(|tool| json!({
            "name": tool.name,
            "description": tool.description,
            "input_schema": tool.schema,
        })).collect::<Vec<_>>(),
        "messages": messages,
        // The tools and the system prompt are the same on every turn and
        // large: caching them is most of what a conversation costs.
        "cache_control": { "type": "ephemeral" },
    });
    if takes_effort(&settings.model) {
        body["output_config"] = json!({ "effort": "medium" });
    }
    let mut request = agent()
        .post(&joined(&settings.base_url, "v1/messages"))
        .set("x-api-key", key)
        .set("anthropic-version", "2023-06-01");
    if takes_fallbacks(&settings.model) {
        // A request a safety classifier declines is run again on a
        // fallback model inside the same call, rather than just stopping.
        body["fallbacks"] = json!("default");
        request = request.set("anthropic-beta", "server-side-fallback-2026-07-01");
    }
    let reply = post(request, &body)?;
    let content = reply["content"].clone();
    let blocks = content.as_array().cloned().unwrap_or_default();
    let text = blocks
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let calls: Vec<Call> = blocks
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .map(|block| Call {
            id: block["id"].as_str().unwrap_or_default().to_owned(),
            name: block["name"].as_str().unwrap_or_default().to_owned(),
            input: block["input"].clone(),
        })
        .collect();
    let stop = match reply["stop_reason"].as_str().unwrap_or("end_turn") {
        "tool_use" => Stop::Tools,
        "max_tokens" => Stop::Truncated,
        "refusal" => Stop::Refused(
            reply
                .pointer("/stop_details/explanation")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ),
        _ => Stop::Done,
    };
    let usage = (
        reply
            .pointer("/usage/input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + reply
                .pointer("/usage/cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
            + reply
                .pointer("/usage/cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        reply
            .pointer("/usage/output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    Ok(Turn {
        text,
        calls,
        raw: content,
        stop,
        usage,
    })
}

// ── OpenAI-compatible ──────────────────────────────────────────────────────

fn openai(
    settings: &Settings,
    system: &str,
    tools: &[ToolDef],
    history: &[Entry],
) -> Result<Turn, String> {
    if settings.base_url.trim().is_empty() {
        return Err("give the provider's address in the assistant's settings".to_owned());
    }
    if settings.preset.needs_key && settings.key.as_deref().is_none_or(str::is_empty) {
        return Err(format!(
            "add a {} API key in the assistant's settings",
            settings.preset.label
        ));
    }
    let mut messages = vec![json!({ "role": "system", "content": system })];
    for entry in history {
        match entry {
            Entry::User(text) => messages.push(json!({ "role": "user", "content": text })),
            Entry::Assistant { text, calls, .. } => {
                let mut message = json!({
                    "role": "assistant",
                    "content": if text.is_empty() { Value::Null } else { json!(text) },
                });
                if !calls.is_empty() {
                    message["tool_calls"] = calls
                        .iter()
                        .map(|call| json!({
                            "id": call.id,
                            "type": "function",
                            "function": { "name": call.name, "arguments": call.input.to_string() },
                        }))
                        .collect();
                }
                messages.push(message);
            }
            Entry::Results(outputs) => {
                let mut pictures = Vec::new();
                for output in outputs {
                    let text = if output.is_error {
                        format!("Error: {}", output.text)
                    } else {
                        output.text.clone()
                    };
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": output.id,
                        "content": text,
                    }));
                    pictures.extend(output.images.iter().cloned());
                }
                // A tool message carries only text in this format; pictures
                // follow as a user message, for models that see.
                if !pictures.is_empty() && settings.preset.vision {
                    let mut content = vec![
                        json!({ "type": "text", "text": "The frames the tools above returned." }),
                    ];
                    content.extend(pictures.iter().map(|png| {
                        json!({
                            "type": "image_url",
                            "image_url": { "url": format!("data:image/png;base64,{png}") },
                        })
                    }));
                    messages.push(json!({ "role": "user", "content": content }));
                }
            }
        }
    }
    let body = json!({
        "model": settings.model,
        "messages": messages,
        "tools": tools.iter().map(|tool| json!({
            "type": "function",
            "function": {
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.schema,
            },
        })).collect::<Vec<_>>(),
        "tool_choice": "auto",
    });
    let mut request = agent().post(&joined(&settings.base_url, "chat/completions"));
    if let Some(key) = settings.key.as_deref().filter(|key| !key.is_empty()) {
        request = request.set("authorization", &format!("Bearer {key}"));
    }
    let reply = post(request, &body)?;
    let message = reply
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or(Value::Null);
    let text = message["content"].as_str().unwrap_or_default().to_owned();
    let calls: Vec<Call> = message["tool_calls"]
        .as_array()
        .map(|calls| {
            calls
                .iter()
                .map(|call| {
                    let arguments = call.pointer("/function/arguments");
                    let input = match arguments {
                        Some(Value::String(text)) => serde_json::from_str(text)
                            .unwrap_or_else(|_| Value::String(text.clone())),
                        Some(other) => other.clone(),
                        None => json!({}),
                    };
                    Call {
                        id: call["id"].as_str().unwrap_or_default().to_owned(),
                        name: call
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        input,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let stop = match reply
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop")
    {
        _ if !calls.is_empty() => Stop::Tools,
        "length" => Stop::Truncated,
        "content_filter" => Stop::Refused(String::new()),
        _ => Stop::Done,
    };
    let usage = (
        reply
            .pointer("/usage/prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reply
            .pointer("/usage/completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    Ok(Turn {
        text,
        calls,
        raw: message,
        stop,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_first_preset_and_its_first_model() {
        assert_eq!(PRESETS[0].id, "anthropic");
        assert_eq!(PRESETS[0].models[0], "claude-opus-5-5");
        assert!(takes_effort("claude-opus-5-5"));
        assert!(!takes_effort("gpt-something"));
        assert!(takes_fallbacks("claude-opus-5-5"));
        assert!(!takes_fallbacks("claude-haiku-5-5"));
    }

    /// Against a running OpenAI-compatible server - the scripted mock in
    /// development: `ASSISTANT_MOCK_URL=http://127.0.0.1:8765/v1 cargo test
    /// -p concat assistant -- --ignored`.
    #[test]
    #[ignore = "needs a provider to talk to"]
    fn a_tool_call_comes_back_from_an_openai_compatible_server() {
        let Ok(url) = std::env::var("ASSISTANT_MOCK_URL") else {
            return;
        };
        let settings = Settings {
            preset: &PRESETS[5],
            model: "mock-model".to_owned(),
            base_url: url,
            key: None,
        };
        let tools = vec![ToolDef {
            name: "timeline_read".to_owned(),
            description: "Reads the timeline.".to_owned(),
            schema: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        }];
        let system = "The open project is \"T\" at:\nC:/projects/T\nPass it.";
        let mut history = vec![Entry::User("hello".to_owned())];
        let turn = send(&settings, system, &tools, &history).expect("an answer");
        assert_eq!(turn.stop, Stop::Tools);
        assert_eq!(turn.calls[0].name, "timeline_read");
        assert_eq!(turn.calls[0].input["path"], "C:/projects/T");
        // The answer goes back, and the next step follows it.
        history.push(Entry::Assistant {
            text: turn.text,
            calls: turn.calls.clone(),
            raw: turn.raw,
        });
        history.push(Entry::Results(vec![ToolOutput {
            id: turn.calls[0].id.clone(),
            text: "{\"tracks\": []}".to_owned(),
            images: vec![],
            is_error: false,
        }]));
        let next = send(&settings, system, &tools, &history).expect("a second answer");
        assert_eq!(next.calls[0].name, "title_add");
    }

    #[test]
    fn an_address_is_joined_with_one_slash() {
        assert_eq!(
            joined("https://a/v1/", "/chat/completions"),
            "https://a/v1/chat/completions"
        );
        assert_eq!(
            joined("https://api.anthropic.com", "v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
    }
}
