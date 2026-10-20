// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! The project on screen as an MCP backend: "live mode".
//!
//! `concat_mcp::backend` leaves room for exactly this - a third
//! [`Backend`] beside the window's server and a headless API, with nothing
//! above it changing - so the assistant hands the MCP server's own tools to
//! the model and their requests land here. Every edit is made through the
//! studio on the window's thread, as a click would make it: the timeline
//! redraws, and Undo takes it back. A frame to look at is gathered there
//! and painted on the assistant's own thread, so the window does not wait
//! for the GPU. Requests about no project in particular - the version, a
//! probe, the effect catalogue - go to a headless API. Making, opening and
//! closing projects, and exports, are the window's own and are refused
//! with a note saying so.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use concat_api::{Api, ApiError, ErrorCode, Request, Response};
use concat_host::preview::{FrameSpec, Monitor};
use concat_mcp::Backend;
use concat_project::Command;
use serde_json::{Value, json};

use crate::host::Shell;
use crate::studio::Studio;

/// How long the window may take to make one edit before the request gives
/// up: far longer than any edit takes, short enough that a window stuck in
/// a native dialog does not hang the assistant for good.
const WINDOW_PATIENCE: Duration = Duration::from_secs(30);

/// The open project, reached from the assistant's thread.
pub struct Live {
    api: Api,
    monitor: Monitor,
    /// The project the turn began on. Every request checks it is still the
    /// one open: a project opened in the middle of a turn must not take the
    /// rest of that turn's edits.
    project: String,
}

impl Live {
    pub fn new(project: String) -> Result<Live, String> {
        Ok(Live {
            api: Api::new(Arc::new(|_| {}))?,
            monitor: Monitor::new(),
            project,
        })
    }

    /// [`on_window`] for this turn's project.
    fn on_studio<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Studio) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        on_window(self.project.clone(), work)
    }
}

/// Runs `work` with the studio on the window's thread and waits for it,
/// once it is sure `project` is still the one open.
fn on_window<T: Send + 'static>(
    project: String,
    work: impl FnOnce(&mut Studio) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let (send, receive) = mpsc::channel();
    slint::invoke_from_event_loop(move || {
        Shell::with(|shell, app| {
            let result = match shell.studio.try_borrow_mut() {
                Ok(studio) if !is_open(&studio, &project) => Err(ApiError::new(
                    ErrorCode::Refused,
                    format!(
                        "the project this conversation works on ({project}) is no longer the one open in the window; stop and tell the user"
                    ),
                )),
                Ok(mut studio) => work(&mut studio),
                Err(_) => Err(ApiError::new(
                    ErrorCode::Busy,
                    "the window is busy with something else; try again in a moment",
                )),
            };
            let _ = send.send(result);
            if let Ok(studio) = shell.studio.try_borrow() {
                studio.publish(&app, &shell.models);
            }
        });
    })
    .map_err(|error| ApiError::failed(format!("the window is gone: {error}")))?;
    receive
        .recv_timeout(WINDOW_PATIENCE)
        .map_err(|_| ApiError::failed("the window did not answer"))?
}

/// Whether `project` is the project open in the window.
fn is_open(studio: &Studio, project: &str) -> bool {
    studio
        .session
        .as_ref()
        .is_some_and(|session| same_folder(session.path(), project))
}

/// Two spellings of one folder: separators and a trailing one aside, and
/// case on Windows, where paths are not case-sensitive.
fn same_folder(a: &str, b: &str) -> bool {
    let tidy = |path: &str| {
        let path = path.replace('\\', "/");
        let path = path.trim_end_matches('/').to_owned();
        if cfg!(windows) { path.to_lowercase() } else { path }
    };
    tidy(a) == tidy(b)
}

fn no_project() -> ApiError {
    ApiError::new(
        ErrorCode::NotOpen,
        "no project is open in the window; open or create one first",
    )
}

fn encoded<T: serde::Serialize>(value: &T) -> Result<Value, ApiError> {
    serde_json::to_value(value)
        .map_err(|error| ApiError::failed(format!("the reply did not encode: {error}")))
}

/// The open project's view, as `project.get` answers.
fn view(studio: &Studio) -> Result<Value, ApiError> {
    let session = studio.session.as_ref().ok_or_else(no_project)?;
    encoded(&session.view())
}

impl Backend for Live {
    fn call(&mut self, request: Request) -> Result<Value, ApiError> {
        match request {
            // About no project: the headless API answers.
            Request::Version
            | Request::MediaProbe { .. }
            | Request::CatalogueList { .. }
            | Request::TemplateList => match self.api.dispatch(request) {
                Response::Result(reply) => encoded(&reply),
                Response::Error(error) => Err(error),
            },
            Request::ProjectGet { .. } => self.on_studio(|studio| view(studio)),
            Request::EditApply { command, .. } => self.on_studio(move |studio| {
                studio
                    .apply_for_agent(*command)
                    .and_then(|view| encoded(&view))
            }),
            Request::EditUndo { .. } => self.on_studio(|studio| {
                studio.undo();
                view(studio)
            }),
            Request::EditRedo { .. } => self.on_studio(|studio| {
                studio.redo();
                view(studio)
            }),
            Request::MediaImport { file, .. } => {
                // Probed here, off the window's thread; added there.
                let item = concat_host::media::probe(&file)
                    .map_err(ApiError::failed)?
                    .to_new_media();
                self.on_studio(move |studio| {
                    studio
                        .apply_for_agent(Command::AddMedia { item })
                        .and_then(|view| encoded(&view))
                })
            }
            Request::ProjectSave { .. } => self.on_studio(|studio| {
                if studio.session.is_none() {
                    return Err(no_project());
                }
                studio.save(true);
                Ok(json!({}))
            }),
            Request::ProjectSetVideo { video, .. } => self.on_studio(move |studio| {
                let session = studio.session.as_mut().ok_or_else(no_project)?;
                let view = session
                    .set_video(video)
                    .map_err(|error| ApiError::new(ErrorCode::Refused, error))?;
                studio.after_agent_change();
                encoded(&view)
            }),
            Request::PreviewFrame {
                time,
                width,
                height,
                output: None,
                ..
            } => {
                // What to paint, gathered on the window's thread...
                let (settings, clips, color_space) = self.on_studio(move |studio| {
                    let session = studio.session.as_ref().ok_or_else(no_project)?;
                    let settings = session.settings();
                    let (w, h) = width
                        .zip(height)
                        .unwrap_or((settings.width, settings.height));
                    let mut clips = session.flattened_clips();
                    clips.extend(
                        studio
                            .host
                            .titles
                            .clips(session.project(), w, h)
                            .into_iter()
                            .map(|title| title.clip),
                    );
                    Ok((settings, clips, session.video().color_space))
                })?;
                // ...and painted here.
                let (w, h) = width
                    .zip(height)
                    .unwrap_or((settings.width, settings.height));
                let (w, h) = (w.clamp(16, 3840), h.clamp(16, 3840));
                let pixels = self
                    .monitor
                    .frame(
                        Arc::new(clips),
                        &settings,
                        FrameSpec {
                            time,
                            width: w,
                            height: h,
                            moving: false,
                            proxy: false,
                            color_space,
                        },
                    )
                    .map_err(ApiError::failed)?;
                let png = png_of(w, h, pixels)?;
                Ok(json!({ "width": w, "height": h, "png": png }))
            }
            Request::ExportRun { .. }
            | Request::ExportStatus { .. }
            | Request::ExportCancel { .. } => Err(ApiError::new(
                ErrorCode::Refused,
                "exports are started from the window's Export button; the assistant does not export yet",
            )),
            _ => Err(ApiError::new(
                ErrorCode::Refused,
                "the assistant works on the project open in the window; making, opening, closing or saving other projects is done from the window",
            )),
        }
    }

    fn describe(&self) -> Value {
        json!({ "mode": "live" })
    }
}

/// RGBA pixels as a base64 PNG.
fn png_of(width: u32, height: u32, pixels: Vec<u8>) -> Result<String, ApiError> {
    use base64::Engine as _;
    let image = image::RgbaImage::from_raw(width, height, pixels)
        .ok_or_else(|| ApiError::failed("the frame came out the wrong size"))?;
    let mut bytes = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(|error| ApiError::failed(format!("could not encode the frame: {error}")))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::same_folder;

    #[test]
    fn one_folder_spelt_two_ways_is_the_same_folder() {
        assert!(same_folder("C:/Projects/Rec", "C:/Projects/Rec/"));
        assert!(same_folder("C:\\Projects\\Rec", "C:/Projects/Rec"));
        assert!(!same_folder("C:/Projects/Rec", "C:/Projects/Other"));
    }
}
