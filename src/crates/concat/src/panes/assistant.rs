// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! The assistant's panel: the conversation shown, the turn running on its
//! own thread, and the choice of provider, model and key.
//!
//! A turn is sent with the conversation so far and runs in
//! [`crate::assistant::agent`]; what it does arrives here step by step as
//! [`Event`]s - text, a tool starting, a tool done with what it found - and
//! is shown as it comes, so the person watches the edit being made. Stop
//! ends it after the step under way. Changing provider starts a new
//! conversation: each provider keeps its turns in a shape of its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::assistant::agent::{self, Event};
use crate::assistant::keys;
use crate::assistant::provider::{self, Entry, PRESETS, Settings};
use crate::host::{on_ui, spawn};
use crate::i18n::{t, tf};
use crate::panes::Msg;
use crate::studio::Studio;
use crate::ui::{AssistantData, ChatItem};

/// Everything that can happen to the panel.
pub enum AssistantMsg {
    /// The title bar's button, or the panel's own close: show it or hide it.
    Toggle,
    Send(String),
    Stop,
    /// A new conversation.
    Clear,
    /// A step of the turn running.
    Event(Box<Event>),
    ToggleSettings,
    Provider(i32),
    /// A row of the fetched model list.
    ModelPicked(i32),
    BaseUrl(String),
    /// Save the key typed, if any, and ask the provider for its models.
    Connect(String),
    /// The provider's models, or why it could not be reached; the number is
    /// the attempt it answers.
    Connected(u32, Result<Vec<String>, String>),
    RemoveKey,
    /// The panel was dragged to a new width, in logical pixels.
    Resized(f32),
}

impl std::fmt::Debug for AssistantMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the words: a message, a key or an address is nobody's log.
        f.write_str(match self {
            Self::Toggle => "Toggle",
            Self::Send(_) => "Send",
            Self::Stop => "Stop",
            Self::Clear => "Clear",
            Self::Event(_) => "Event",
            Self::ToggleSettings => "ToggleSettings",
            Self::Provider(_) => "Provider",
            Self::ModelPicked(_) => "ModelPicked",
            Self::BaseUrl(_) => "BaseUrl",
            Self::Connect(_) => "Connect",
            Self::Connected(..) => "Connected",
            Self::RemoveKey => "RemoveKey",
            Self::Resized(_) => "Resized",
        })
    }
}

/// One step shown.
#[derive(Clone)]
struct Item {
    kind: i32,
    text: String,
    detail: String,
    ok: bool,
    running: bool,
    picture: Option<slint::Image>,
}

impl Item {
    fn new(kind: i32, text: impl Into<String>) -> Item {
        Item {
            kind,
            text: text.into(),
            detail: String::new(),
            ok: true,
            running: false,
            picture: None,
        }
    }
}

const USER: i32 = 0;
const ASSISTANT: i32 = 1;
const TOOL: i32 = 2;
const PROBLEM: i32 = 3;
const NOTE: i32 = 4;

#[derive(Default)]
pub struct AssistantPane {
    open: bool,
    settings_open: bool,
    items: Vec<Item>,
    history: Vec<Entry>,
    busy: bool,
    stop: Option<Arc<AtomicBool>>,
    usage: (u64, u64),
    /// Whether a key is saved for the current provider: asked of the
    /// credential store when the provider changes, not every frame.
    has_key: Option<bool>,
    /// The models the provider listed on the last Connect.
    models: Vec<String>,
    connect: Connect,
    /// Counted up at every Connect, so a late answer is dropped.
    attempt: u32,
}

/// Where Connect stands.
#[derive(Clone, Debug, Default, PartialEq)]
enum Connect {
    #[default]
    Idle,
    Busy,
    Done,
    Failed(String),
}

impl AssistantPane {
    pub fn update(&mut self, msg: AssistantMsg, studio: &mut Studio) {
        match msg {
            AssistantMsg::Toggle => {
                self.open = !self.open;
                self.refresh_key(studio);
                if self.open && !self.configured(studio) {
                    self.open_settings(studio);
                }
            }
            AssistantMsg::ToggleSettings => {
                if self.settings_open {
                    self.settings_open = false;
                } else {
                    self.open_settings(studio);
                }
            }
            AssistantMsg::Send(text) => self.send(text, studio),
            AssistantMsg::Stop => {
                if let Some(stop) = &self.stop {
                    stop.store(true, Ordering::Release);
                }
            }
            AssistantMsg::Clear => {
                if let Some(stop) = self.stop.take() {
                    stop.store(true, Ordering::Release);
                }
                self.items.clear();
                self.history.clear();
                self.usage = (0, 0);
                self.busy = false;
            }
            AssistantMsg::Event(event) => self.event(*event),
            AssistantMsg::Provider(index) => {
                let index = usize::try_from(index).unwrap_or(0).min(PRESETS.len() - 1);
                let chosen = &PRESETS[index];
                if studio.prefs.assistant_provider.as_deref() != Some(chosen.id) {
                    studio.prefs.assistant_provider = Some(chosen.id.to_owned());
                    studio.prefs.assistant_model =
                        chosen.models.first().map(|model| (*model).to_owned());
                    studio.prefs.assistant_base_url = Some(chosen.base_url.to_owned());
                    studio.prefs.save(&studio.host.dirs);
                    if !self.history.is_empty() {
                        self.history.clear();
                        self.items
                            .push(Item::new(NOTE, t("assistant.newProviderNote")));
                    }
                    self.models.clear();
                    self.connect = Connect::Idle;
                }
                self.refresh_key(studio);
                self.connect_if_ready(studio);
            }
            AssistantMsg::ModelPicked(index) => {
                if let Some(model) = usize::try_from(index).ok().and_then(|i| self.models.get(i)) {
                    studio.prefs.assistant_model = Some(model.clone());
                    studio.prefs.save(&studio.host.dirs);
                }
            }
            AssistantMsg::Connect(key) => {
                if !key.trim().is_empty() {
                    let id = self.preset(studio).id;
                    if let Err(error) = keys::set(id, &key) {
                        studio.notify(&tf("assistant.keyFailed", &[&error]), true);
                        return;
                    }
                    self.has_key = None;
                    self.refresh_key(studio);
                }
                self.start_connect(studio);
            }
            AssistantMsg::Connected(attempt, result) => {
                if attempt != self.attempt {
                    return;
                }
                match result {
                    Ok(models) => {
                        // The model chosen before stays chosen when it is
                        // still offered; else the provider's default, else
                        // the first listed.
                        let current = self.model(studio);
                        let keep = models.contains(&current);
                        let fallback = self
                            .preset(studio)
                            .models
                            .iter()
                            .find(|model| models.iter().any(|m| m == *model))
                            .map(|model| (*model).to_owned())
                            .unwrap_or_else(|| models[0].clone());
                        if !keep {
                            studio.prefs.assistant_model = Some(fallback);
                            studio.prefs.save(&studio.host.dirs);
                        }
                        self.models = models;
                        self.connect = Connect::Done;
                    }
                    Err(error) => {
                        self.models.clear();
                        self.connect = Connect::Failed(error);
                    }
                }
            }
            AssistantMsg::BaseUrl(url) => {
                studio.prefs.assistant_base_url = Some(url.trim().to_owned());
                studio.prefs.save(&studio.host.dirs);
            }
            AssistantMsg::Resized(width) => {
                studio.prefs.assistant_width = Some(width.clamp(300.0, 900.0));
                studio.prefs.save(&studio.host.dirs);
            }
            AssistantMsg::RemoveKey => {
                let id = self.preset(studio).id;
                if let Err(error) = keys::remove(id) {
                    studio.notify(&tf("assistant.keyFailed", &[&error]), true);
                }
                self.has_key = None;
                self.refresh_key(studio);
                self.models.clear();
                self.connect = Connect::Idle;
            }
        }
    }

    fn preset(&self, studio: &Studio) -> &'static provider::Preset {
        let id = studio
            .prefs
            .assistant_provider
            .as_deref()
            .unwrap_or(PRESETS[0].id);
        PRESETS
            .iter()
            .find(|preset| preset.id == id)
            .unwrap_or(&PRESETS[0])
    }

    fn preset_index(&self, studio: &Studio) -> i32 {
        let id = self.preset(studio).id;
        PRESETS
            .iter()
            .position(|preset| preset.id == id)
            .and_then(|index| i32::try_from(index).ok())
            .unwrap_or(0)
    }

    fn model(&self, studio: &Studio) -> String {
        studio
            .prefs
            .assistant_model
            .clone()
            .filter(|model| !model.is_empty())
            .or_else(|| {
                self.preset(studio)
                    .models
                    .first()
                    .map(|model| (*model).to_owned())
            })
            .unwrap_or_default()
    }

    fn base_url(&self, studio: &Studio) -> String {
        studio
            .prefs
            .assistant_base_url
            .clone()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| self.preset(studio).base_url.to_owned())
    }

    fn open_settings(&mut self, studio: &Studio) {
        self.settings_open = true;
        self.refresh_key(studio);
        self.connect_if_ready(studio);
    }

    /// Connects by itself when nothing more is needed from the person: a
    /// key is saved, or the provider needs none.
    fn connect_if_ready(&mut self, studio: &Studio) {
        let ready = self.has_key.unwrap_or(false) || !self.preset(studio).needs_key;
        if ready && matches!(self.connect, Connect::Idle) && !self.base_url(studio).is_empty() {
            self.start_connect(studio);
        }
    }

    /// Asks the provider for its models, off the window's thread.
    fn start_connect(&mut self, studio: &Studio) {
        let preset = self.preset(studio);
        let settings = Settings {
            preset,
            model: String::new(),
            base_url: self.base_url(studio),
            key: keys::get(preset.id),
        };
        self.attempt = self.attempt.wrapping_add(1);
        let attempt = self.attempt;
        self.connect = Connect::Busy;
        spawn(
            move || provider::list_models(&settings),
            move |studio, _, _, result| {
                studio.handle(Msg::Assistant(AssistantMsg::Connected(attempt, result)));
            },
        );
    }

    fn refresh_key(&mut self, studio: &Studio) {
        self.has_key = Some(keys::get(self.preset(studio).id).is_some());
    }

    fn configured(&self, studio: &Studio) -> bool {
        let preset = self.preset(studio);
        !self.model(studio).is_empty()
            && !self.base_url(studio).is_empty()
            && (!preset.needs_key || self.has_key.unwrap_or(false))
    }

    fn send(&mut self, text: String, studio: &mut Studio) {
        let text = text.trim().to_owned();
        if text.is_empty() || self.busy {
            return;
        }
        let Some(session) = studio.session.as_ref() else {
            self.items
                .push(Item::new(PROBLEM, t("assistant.openProjectFirst")));
            return;
        };
        let preset = self.preset(studio);
        let settings = Settings {
            preset,
            model: self.model(studio),
            base_url: self.base_url(studio),
            key: keys::get(preset.id),
        };
        let system = agent::system_prompt(session.path(), &studio.project_name);
        let project = session.path().to_owned();
        self.items.push(Item::new(USER, text.clone()));
        self.history.push(Entry::User(text));
        self.busy = true;
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(Arc::clone(&stop));
        let history = self.history.clone();
        log::info!(
            "assistant: a turn with {} ({}), {} steps of history",
            preset.id,
            settings.model,
            history.len()
        );
        let spawned = std::thread::Builder::new()
            .name("concat assistant".to_owned())
            .spawn(move || {
                agent::run(settings, system, project, history, stop, |event| {
                    on_ui(move |studio, _, _| {
                        studio.handle(Msg::Assistant(AssistantMsg::Event(Box::new(event))));
                    });
                });
            });
        if let Err(error) = spawned {
            self.busy = false;
            self.items.push(Item::new(PROBLEM, error.to_string()));
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Text(text) => self.items.push(Item::new(ASSISTANT, text)),
            Event::ToolStarted { name, detail } => {
                let mut item = Item::new(TOOL, name);
                item.detail = detail;
                item.running = true;
                self.items.push(item);
            }
            Event::ToolDone {
                name,
                ok,
                detail,
                picture,
            } => {
                if let Some(item) = self
                    .items
                    .iter_mut()
                    .rev()
                    .find(|item| item.kind == TOOL && item.running && item.text == name)
                {
                    item.running = false;
                    item.ok = ok;
                    if !ok || item.detail.is_empty() {
                        item.detail = detail;
                    }
                    item.picture = picture.as_deref().and_then(picture_of);
                }
            }
            Event::Refused(why) => {
                let text = if why.is_empty() {
                    t("assistant.refused")
                } else {
                    tf("assistant.refusedBecause", &[&why])
                };
                self.items.push(Item::new(PROBLEM, text));
            }
            Event::Failed(error) => self.items.push(Item::new(PROBLEM, error)),
            Event::Finished { history, usage } => {
                self.history = history;
                self.usage.0 += usage.0;
                self.usage.1 += usage.1;
                self.busy = false;
                self.stop = None;
                // A tool still marked running was cut short.
                for item in &mut self.items {
                    if item.running {
                        item.running = false;
                        item.ok = false;
                    }
                }
            }
        }
    }

    pub fn data(&self, studio: &Studio) -> AssistantData {
        let preset = self.preset(studio);
        let model = self.model(studio);
        AssistantData {
            open: self.open,
            busy: self.busy,
            settings_open: self.settings_open,
            configured: self.configured(studio),
            project_open: studio.session.is_some(),
            provider: self.preset_index(studio),
            model: model.clone().into(),
            base_url: self.base_url(studio).into(),
            has_key: self.has_key.unwrap_or(false),
            needs_key: preset.needs_key,
            key_page: preset.key_page.into(),
            connect_state: match self.connect {
                Connect::Idle => 0,
                Connect::Busy => 1,
                Connect::Done => 2,
                Connect::Failed(_) => 3,
            },
            connect_message: match &self.connect {
                Connect::Idle => Default::default(),
                Connect::Busy => t("assistant.connecting").into(),
                Connect::Done => {
                    tf("assistant.connected", &[&self.models.len().to_string()]).into()
                }
                Connect::Failed(error) => error.clone().into(),
            },
            model_index: self
                .models
                .iter()
                .position(|m| *m == model)
                .and_then(|i| i32::try_from(i).ok())
                .unwrap_or(-1),
            show_address: preset.base_url.is_empty() || preset.id == "ollama",
            summary: if model.is_empty() {
                preset.label.into()
            } else {
                format!("{} · {model}", preset.label).into()
            },
            usage: if self.usage == (0, 0) {
                Default::default()
            } else {
                tf(
                    "assistant.tokens",
                    &[&thousands(self.usage.0), &thousands(self.usage.1)],
                )
                .into()
            },
        }
    }

    pub fn item_rows(&self) -> Vec<ChatItem> {
        self.items
            .iter()
            .map(|item| ChatItem {
                kind: item.kind,
                text: item.text.clone().into(),
                detail: item.detail.clone().into(),
                ok: item.ok,
                running: item.running,
                has_picture: item.picture.is_some(),
                picture: item.picture.clone().unwrap_or_default(),
            })
            .collect()
    }

    pub fn provider_rows(&self) -> Vec<slint::SharedString> {
        PRESETS.iter().map(|preset| preset.label.into()).collect()
    }

    pub fn model_rows(&self, _studio: &Studio) -> Vec<slint::SharedString> {
        self.models
            .iter()
            .map(|model| model.as_str().into())
            .collect()
    }
}

/// A base64 PNG as an image for the panel.
fn picture_of(png: &str) -> Option<slint::Image> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(png).ok()?;
    let decoded = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (width, height) = decoded.dimensions();
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        decoded.as_raw(),
        width,
        height,
    );
    Some(slint::Image::from_rgba8(buffer))
}

/// 12345 as "12.3k".
fn thousands(count: u64) -> String {
    if count >= 1000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else {
        count.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_read_short() {
        assert_eq!(thousands(950), "950");
        assert_eq!(thousands(12_345), "12.3k");
    }
}
