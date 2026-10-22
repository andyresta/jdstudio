// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! The assistant: a chat panel beside the editor, where a language model
//! edits the open project through JDStudio's own MCP tools.
//!
//! - [`provider`]: the models it talks to - Claude through Anthropic's
//!   Messages API, everything else through the OpenAI chat format - with an
//!   API key the person brings.
//! - [`keys`]: where those keys are kept: the system's credential store.
//! - [`live`]: the open project as an MCP backend, so every edit is the
//!   window's own and can be undone.
//! - [`agent`]: the loop that runs one turn, on a thread of its own.

pub mod agent;
pub mod keys;
pub mod live;
pub mod provider;
