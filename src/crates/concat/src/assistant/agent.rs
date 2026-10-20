// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! The assistant's loop, on a thread of its own: the conversation goes to
//! the model; the tools it asks for run against the open project through
//! the MCP server's own toolbox and the [`Live`] backend; their answers go
//! back; until the model is done, the person presses Stop, or the turn has
//! taken too many steps.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use concat_mcp::tools::{Context, Toolbox};
use serde_json::Value;

use super::live::Live;
use super::provider::{self, Call, Entry, Settings, Stop, ToolDef, ToolOutput};

/// The most model replies one message from the person may lead to: a long
/// edit is a few dozen tool calls; a model going round in circles is
/// stopped here.
const MOST_STEPS: usize = 40;

/// Tools the assistant is not given: they make, open or close projects,
/// or export, which the window does itself (see [`Live`]).
const WITHHELD: &[&str] = &[
    "project_list",
    "project_create",
    "project_open",
    "project_close",
    "template_instantiate",
    "export_start",
    "export_status",
    "export_wait",
    "export_cancel",
];

/// What the loop reports to the window as it goes.
#[derive(Debug)]
pub enum Event {
    /// What the model said this step.
    Text(String),
    /// A tool is starting: its name and a short reading of its arguments.
    ToolStarted { name: String, detail: String },
    /// It finished: whether it worked, a short reading of its answer, and a
    /// picture when it returned one (base64 PNG).
    ToolDone {
        name: String,
        ok: bool,
        detail: String,
        picture: Option<String>,
    },
    /// The model declined.
    Refused(String),
    /// Something went wrong and the turn is over.
    Failed(String),
    /// The turn is over: the conversation as it now stands, and the tokens
    /// it took (read, written).
    Finished {
        history: Vec<Entry>,
        usage: (u64, u64),
    },
}

/// The toolbox, built once: generating its schemas takes a moment.
fn toolbox() -> &'static Toolbox {
    static TOOLBOX: OnceLock<Toolbox> = OnceLock::new();
    TOOLBOX.get_or_init(Toolbox::new)
}

/// The tools the model is told about.
pub fn tool_defs() -> Vec<ToolDef> {
    toolbox()
        .list()
        .into_iter()
        .filter_map(|tool| {
            let name = tool["name"].as_str()?.to_owned();
            if WITHHELD.contains(&name.as_str()) {
                return None;
            }
            Some(ToolDef {
                description: tool["description"].as_str().unwrap_or_default().to_owned(),
                schema: tool["inputSchema"].clone(),
                name,
            })
        })
        .collect()
}

/// Runs one turn: from the person's latest message to the model's answer.
/// Blocks; call it on a thread of its own. `report` is called from that
/// thread.
pub fn run(
    settings: Settings,
    system: String,
    project: String,
    mut history: Vec<Entry>,
    stop: Arc<AtomicBool>,
    report: impl Fn(Event),
) {
    let tools = tool_defs();
    let mut live = match Live::new(project) {
        Ok(live) => live,
        Err(error) => {
            report(Event::Failed(error));
            report(Event::Finished {
                history,
                usage: (0, 0),
            });
            return;
        }
    };
    let mut usage = (0u64, 0u64);
    for _ in 0..MOST_STEPS {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let turn = match provider::send(&settings, &system, &tools, &history) {
            Ok(turn) => turn,
            Err(error) => {
                report(Event::Failed(error));
                break;
            }
        };
        usage.0 += turn.usage.0;
        usage.1 += turn.usage.1;
        if !turn.text.trim().is_empty() {
            report(Event::Text(turn.text.trim().to_owned()));
        }
        let calls = turn.calls.clone();
        history.push(Entry::Assistant {
            text: turn.text,
            calls: turn.calls,
            raw: turn.raw,
        });
        match turn.stop {
            Stop::Tools if !calls.is_empty() => {
                let outputs = calls
                    .iter()
                    .map(|call| {
                        if stop.load(Ordering::Acquire) {
                            return stopped(call);
                        }
                        report(Event::ToolStarted {
                            name: call.name.clone(),
                            detail: brief(&call.input),
                        });
                        let output = execute(call, &mut live, &stop);
                        report(Event::ToolDone {
                            name: call.name.clone(),
                            ok: !output.is_error,
                            detail: outcome_line(&output),
                            picture: output.images.first().cloned(),
                        });
                        output
                    })
                    .collect();
                // Every call gets its answer, in one message, even when the
                // person stopped part way: the model is told which ran.
                history.push(Entry::Results(outputs));
            }
            Stop::Truncated => {
                report(Event::Failed(
                    "the answer was cut off at its length limit; ask for less at once".to_owned(),
                ));
                break;
            }
            Stop::Refused(why) => {
                report(Event::Refused(why));
                break;
            }
            _ => break,
        }
    }
    report(Event::Finished { history, usage });
}

/// A tool's answer in one line for its card: its first sentence, or
/// nothing when the answer is JSON, which the card's arguments already say
/// more usefully than a brace would.
fn outcome_line(output: &ToolOutput) -> String {
    let first = output.text.lines().next().unwrap_or_default().trim();
    if first.starts_with('{') || first.starts_with('[') {
        return String::new();
    }
    first.chars().take(140).collect()
}

/// The answer for a call that was never run.
fn stopped(call: &Call) -> ToolOutput {
    ToolOutput {
        id: call.id.clone(),
        text: "Not run: the user pressed Stop.".to_owned(),
        images: Vec::new(),
        is_error: true,
    }
}

/// Runs one tool against the open project.
fn execute(call: &Call, live: &mut Live, stop: &AtomicBool) -> ToolOutput {
    let failed = |text: String| ToolOutput {
        id: call.id.clone(),
        text,
        images: Vec::new(),
        is_error: true,
    };
    if WITHHELD.contains(&call.name.as_str()) {
        return failed(format!("{} is not available in the assistant", call.name));
    }
    if !call.input.is_object() {
        return failed("the arguments were not a JSON object; send them again as one".to_owned());
    }
    let context = Context {
        backend: live,
        cancelled: stop,
        progress: None,
    };
    let Some(outcome) = toolbox().call(&call.name, call.input.clone(), context) else {
        return failed(format!("there is no tool called {}", call.name));
    };
    let mut text = Vec::new();
    let mut images = Vec::new();
    for block in outcome.content {
        match block["type"].as_str() {
            Some("text") => text.push(block["text"].as_str().unwrap_or_default().to_owned()),
            Some("image") => {
                if let Some(data) = block["data"].as_str() {
                    images.push(data.to_owned());
                }
            }
            _ => {}
        }
    }
    ToolOutput {
        id: call.id.clone(),
        text: text.join("\n"),
        images,
        is_error: outcome.is_error,
    }
}

/// A one-line reading of a tool's arguments for its card: the fields a
/// person recognises, without the project's path.
fn brief(input: &Value) -> String {
    let Some(fields) = input.as_object() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for (key, value) in fields {
        if key == "path" {
            continue;
        }
        let shown = match value {
            Value::String(text) => format!("\"{}\"", text.chars().take(40).collect::<String>()),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            Value::Null => "null".to_owned(),
            Value::Array(items) => format!("[{}]", items.len()),
            Value::Object(object) => match object.get("op").and_then(Value::as_str) {
                Some(op) => op.to_owned(),
                None => "{…}".to_owned(),
            },
        };
        parts.push(format!("{key}: {shown}"));
    }
    parts.join(", ").chars().take(120).collect()
}

/// The system prompt: who the assistant is and the project it works on.
pub fn system_prompt(project_path: &str, project_name: &str) -> String {
    format!(
        "You are the editing assistant built into JDStudio, a desktop video editor. \
You edit the project the user has open in the window, through the tools you are given; \
every edit appears on their timeline at once and they can undo it.\n\n\
The open project is \"{project_name}\" at:\n{project_path}\n\
Pass exactly that path as `path` to every tool that asks for one.\n\n\
How to work:\n\
- Read the timeline (timeline_read) before editing, to learn the ids of lanes, clips and media.\n\
- Times are seconds on the timeline; keyframe positions are fractions of a clip (0 to 1); \
sizes and offsets are fractions of the frame.\n\
- After a visual change, look at it with preview_frame when it matters, and fix what is wrong.\n\
- Prefer one tool call per clear step; use edit_apply with a batch for many small edits as one undo step.\n\
- Edits stay in memory until saved; save with project_save only when the user asks.\n\
- You cannot create, open or close projects, or export; tell the user to use the window for those.\n\
- When the request leaves out something that changes the result (which clip, where, how long, \
what words) and no choice is clearly right, ask one short question first and wait for the answer; \
offer two or three concrete options when you can.\n\
- When a sensible default exists and the edit is easy to undo, make it, then say in one line what \
you assumed so the user can correct it.\n\
- Before removing clips or anything hard to recover, say what you are about to do and wait for a yes.\n\
- Answer in the language the user writes in, briefly: say what you changed, not how the tools work."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_card_reads_the_arguments_without_the_path() {
        let text = brief(
            &json!({ "path": "C:/x", "start": 1.5, "style": { "content": "Hi" }, "clipIds": ["a", "b"] }),
        );
        assert!(!text.contains("C:/x"));
        assert!(text.contains("start: 1.5"));
        assert!(text.contains("clipIds: [2]"));
    }

    #[test]
    fn the_window_s_own_verbs_are_withheld() {
        let names: Vec<String> = tool_defs().into_iter().map(|tool| tool.name).collect();
        assert!(names.contains(&"timeline_read".to_owned()));
        assert!(names.contains(&"preview_frame".to_owned()));
        assert!(!names.contains(&"project_create".to_owned()));
        assert!(!names.contains(&"export_start".to_owned()));
    }
}
