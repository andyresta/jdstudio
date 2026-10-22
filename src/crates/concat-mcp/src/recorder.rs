// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! The screen recorder, for an agent: the devices there are, a recording
//! started and stopped, and its files put into a project as the window's
//! recorder puts them.
//!
//! The recording runs in this process, whichever backend edits projects:
//! the screen, camera and microphone are this machine's either way, and
//! the files it writes are plain media that any backend can import. Only
//! [`land`] goes through the backend, as the requests an agent could make
//! itself - import, add a clip, shape and place the camera - so a landed
//! recording is undone a step at a time like any other edit.
//!
//! A recording lives as long as the session: one still running when the
//! client goes is stopped and its files finished, as the window's recorder
//! finishes its files when it is closed mid-take.
//!
//! The recorder is shared between threads - the assistant keeps one toolbox
//! for every turn, each turn on a thread of its own - so a recording started
//! in one turn can be stopped in the next. That holds on every platform:
//! the microphone's stream never leaves the thread that opened it, so a
//! [`Recording`] may.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use concat_api::{ApiError, ErrorCode, Request};
use concat_host::screen::{self, CameraShape, Finished, Plan, Recording, Track};
use concat_media::capture::Area;
use concat_project::commands::Command;
use serde_json::{Value, json};

use crate::backend::Backend;

/// The folder a project's recordings are written into.
const PROJECT_FOLDER: &str = "recordings";

/// A recording, running or stopped.
enum Job {
    Running(Recording),
    Stopped(Finished),
}

/// Every recording of this session, by its job id.
#[derive(Default)]
pub struct Recorder {
    jobs: Mutex<HashMap<String, Job>>,
    next: AtomicU64,
}

fn invalid(why: &str) -> ApiError {
    ApiError::invalid(format!("invalid arguments: {why}"))
}

/// The arguments as an object, refusing any field not in `known`.
fn fields<'a>(
    arguments: &'a Value,
    known: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, ApiError> {
    let fields = arguments
        .as_object()
        .ok_or_else(|| invalid("the arguments are an object"))?;
    if let Some(unknown) = fields.keys().find(|key| !known.contains(&key.as_str())) {
        return Err(invalid(&format!("unknown field `{unknown}`")));
    }
    Ok(fields)
}

/// A string field, absent and null alike being none.
fn text<'a>(
    fields: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, ApiError> {
    match fields.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text)),
        Some(_) => Err(invalid(&format!("`{key}` is a non-empty string"))),
    }
}

fn job_of(fields: &serde_json::Map<String, Value>) -> Result<String, ApiError> {
    text(fields, "job")?
        .map(str::to_owned)
        .ok_or_else(|| invalid("missing field `job`"))
}

fn unknown_job(job: &str) -> ApiError {
    ApiError::new(
        ErrorCode::NotFound,
        format!("no recording {job} in this session"),
    )
}

/// `folder` as somewhere this server may write, or why not: under one of
/// `roots` once resolved, with no `..` to walk out of one.
fn writable(folder: &Path, roots: &[PathBuf]) -> Result<(), ApiError> {
    if !folder.is_absolute() {
        return Err(invalid("the folder is an absolute path"));
    }
    if roots.is_empty() {
        return Ok(());
    }
    let outside = || {
        let roots = roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        ApiError::new(
            ErrorCode::Refused,
            format!(
                "{} is outside where this server writes ({roots})",
                folder.display()
            ),
        )
    };
    if folder
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(outside());
    }
    // The part that exists, resolved, with the rest put back on.
    let mut existing = folder;
    let mut rest = Vec::new();
    let target = loop {
        if let Ok(real) = existing.canonicalize() {
            break rest
                .iter()
                .rev()
                .fold(real, |path: PathBuf, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => break folder.to_path_buf(),
        }
    };
    if roots.iter().any(|root| target.starts_with(root)) {
        Ok(())
    } else {
        Err(outside())
    }
}

/// `recorder_devices`: the cameras and microphones there are.
pub fn devices() -> Value {
    let cameras: Vec<Value> = concat_media::capture::cameras()
        .into_iter()
        .map(|camera| json!({ "id": camera.id, "label": camera.label }))
        .collect();
    json!({
        "cameras": cameras,
        "microphones": concat_host::record::microphones(),
        "platform": std::env::consts::OS,
    })
}

/// A track as the agent reads it, with the length its file probes to.
fn track(track: &Track, picture: bool) -> Value {
    let seconds = concat_host::media::probe(&track.path.to_string_lossy())
        .ok()
        .and_then(|summary| summary.duration);
    let mut out = json!({
        "file": track.path.display().to_string(),
        "start": track.start,
        "seconds": seconds,
    });
    if picture {
        out["width"] = json!(track.width);
        out["height"] = json!(track.height);
    }
    out
}

fn finished_view(job: &str, finished: &Finished) -> Value {
    json!({
        "job": job,
        "state": "stopped",
        "fps": finished.fps,
        "screen": track(&finished.screen, true),
        "camera": finished.camera.as_ref().map(|camera| track(camera, true)),
        "voice": finished.voice.as_ref().map(|voice| track(voice, false)),
    })
}

impl Recorder {
    /// The jobs, even after a tool panicked holding them: each job is whole
    /// on its own, so the rest are still good.
    fn jobs(&self) -> MutexGuard<'_, HashMap<String, Job>> {
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `recorder_start`.
    pub fn start(&self, arguments: &Value, roots: &[PathBuf]) -> Result<Value, ApiError> {
        let fields = fields(
            arguments,
            &[
                "folder",
                "path",
                "area",
                "fps",
                "camera",
                "microphone",
                "system_audio",
            ],
        )?;
        let folder = match (text(fields, "folder")?, text(fields, "path")?) {
            (Some(folder), None) => PathBuf::from(folder),
            (None, Some(project)) => Path::new(project).join(PROJECT_FOLDER),
            (Some(_), Some(_)) => return Err(invalid("give `folder` or `path`, not both")),
            (None, None) => {
                return Err(invalid(
                    "give `folder` to write into, or a project's `path`",
                ));
            }
        };
        writable(&folder, roots)?;
        let area = match fields.get("area") {
            None | Some(Value::Null) => None,
            Some(area) => {
                let side = |key: &str| {
                    area.get(key)
                        .and_then(Value::as_u64)
                        .and_then(|value| u32::try_from(value).ok())
                        .ok_or_else(|| {
                            invalid(&format!("`area.{key}` is a whole number of pixels"))
                        })
                };
                let area = Area {
                    x: side("x")?,
                    y: side("y")?,
                    width: side("width")?,
                    height: side("height")?,
                };
                if area.width < 2 || area.height < 2 {
                    return Err(invalid("`area` is at least 2 pixels each way"));
                }
                Some(area)
            }
        };
        let fps = match fields.get("fps") {
            None | Some(Value::Null) => 30,
            Some(fps) => fps
                .as_u64()
                .filter(|fps| (1..=60).contains(fps))
                .and_then(|fps| u32::try_from(fps).ok())
                .ok_or_else(|| invalid("`fps` is a whole number from 1 to 60"))?,
        };
        let camera = text(fields, "camera")?.map(str::to_owned);
        let microphone = text(fields, "microphone")?.map(str::to_owned);
        let system_audio = match fields.get("system_audio") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(on)) => *on,
            Some(_) => return Err(invalid("`system_audio` is true or false")),
        };
        if system_audio && !concat_host::record::system_audio_available() {
            return Err(invalid(
                "`system_audio` records what the computer plays, which this platform cannot",
            ));
        }

        let mut jobs = self.jobs();
        if let Some((running, _)) = jobs.iter().find(|(_, job)| matches!(job, Job::Running(_))) {
            return Err(ApiError::new(
                ErrorCode::Busy,
                format!("recording {running} is still running: recorder_stop it first"),
            ));
        }
        let recording = Recording::start(
            Plan {
                folder: folder.clone(),
                area,
                fps,
                camera: camera.clone(),
                microphone: microphone.clone(),
                system_audio,
            },
            None,
            None,
            None,
        )
        .map_err(|error| ApiError::failed(format!("the recording would not start: {error}")))?;
        let number = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let job = format!("r{number}");
        jobs.insert(job.clone(), Job::Running(recording));
        Ok(json!({
            "job": job,
            "state": "recording",
            "folder": folder.display().to_string(),
            "fps": fps,
            "camera": camera,
            "microphone": microphone,
            "system_audio": system_audio,
        }))
    }

    /// `recorder_status`.
    pub fn status(&self, arguments: &Value) -> Result<Value, ApiError> {
        let job = job_of(fields(arguments, &["job"])?)?;
        let jobs = self.jobs();
        match jobs.get(&job) {
            Some(Job::Running(recording)) => Ok(json!({
                "job": job,
                "state": if recording.paused() { "paused" } else { "recording" },
                "seconds": recording.seconds(),
                "failed": recording.failed(),
            })),
            Some(Job::Stopped(finished)) => Ok(finished_view(&job, finished)),
            None => Err(unknown_job(&job)),
        }
    }

    /// `recorder_pause`: holds or carries on a running recording.
    pub fn pause(&self, arguments: &Value) -> Result<Value, ApiError> {
        let fields = fields(arguments, &["job", "paused"])?;
        let job = job_of(fields)?;
        let paused = match fields.get("paused") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(paused)) => *paused,
            Some(_) => return Err(invalid("`paused` is true or false")),
        };
        match self.jobs().get_mut(&job) {
            Some(Job::Running(recording)) => {
                if paused {
                    recording.pause();
                } else {
                    recording.resume();
                }
                Ok(json!({
                    "job": job,
                    "state": if recording.paused() { "paused" } else { "recording" },
                    "seconds": recording.seconds(),
                }))
            }
            Some(Job::Stopped(_)) => Err(ApiError::new(
                ErrorCode::Refused,
                format!("recording {job} has stopped"),
            )),
            None => Err(unknown_job(&job)),
        }
    }

    /// `recorder_stop`: the files, finished. Stopping one already stopped
    /// gives its files again.
    pub fn stop(&self, arguments: &Value) -> Result<Value, ApiError> {
        let job = job_of(fields(arguments, &["job"])?)?;
        let mut jobs = self.jobs();
        let finished = match jobs.remove(&job) {
            Some(Job::Running(recording)) => recording
                .stop()
                .map_err(|error| ApiError::failed(format!("the recording failed: {error}")))?,
            Some(Job::Stopped(finished)) => finished,
            None => return Err(unknown_job(&job)),
        };
        let view = finished_view(&job, &finished);
        jobs.insert(job, Job::Stopped(finished));
        Ok(view)
    }

    /// `recorder_land`: a stopped recording's files into an open project -
    /// the screen, the camera over it in the bottom-right corner, the voice
    /// - from `start` seconds, as the window's recorder lands them.
    pub fn land(&self, arguments: &Value, backend: &mut dyn Backend) -> Result<Value, ApiError> {
        let fields = fields(arguments, &["path", "job", "start", "cameraShape"])?;
        let path = text(fields, "path")?
            .ok_or_else(|| invalid("missing field `path`"))?
            .to_owned();
        let job = job_of(fields)?;
        let base = match fields.get("start") {
            None | Some(Value::Null) => 0.0,
            Some(start) => start
                .as_f64()
                .filter(|start| start.is_finite() && *start >= 0.0)
                .ok_or_else(|| invalid("`start` is seconds, 0 or more"))?,
        };
        let shape = match text(fields, "cameraShape")? {
            None => CameraShape::Rectangle,
            Some(word) => CameraShape::parse(word)
                .ok_or_else(|| invalid("`cameraShape` is rectangle, rounded or circle"))?,
        };
        let finished = match self.jobs().get(&job) {
            Some(Job::Stopped(finished)) => finished.clone(),
            Some(Job::Running(_)) => {
                return Err(ApiError::new(
                    ErrorCode::Refused,
                    format!("recording {job} is still running: recorder_stop it first"),
                ));
            }
            None => return Err(unknown_job(&job)),
        };

        let edit = |backend: &mut dyn Backend, command: Command| {
            backend.call(Request::EditApply {
                path: path.clone(),
                command: Box::new(command),
            })
        };
        // Into the bin, and onto the first lane free at its start.
        let add = |backend: &mut dyn Backend, track: &Track| -> Result<String, ApiError> {
            let file = track.path.to_string_lossy().into_owned();
            let view = backend.call(Request::MediaImport {
                path: path.clone(),
                file: file.clone(),
            })?;
            let media_id = match view.get("createdId").and_then(Value::as_str) {
                Some(id) => id.to_owned(),
                // In the bin already: the item that has its path.
                None => view
                    .pointer("/project/media")
                    .and_then(Value::as_array)
                    .and_then(|media| {
                        media
                            .iter()
                            .find(|item| item["path"].as_str() == Some(file.as_str()))
                    })
                    .and_then(|item| item["id"].as_str())
                    .map(str::to_owned)
                    .ok_or_else(|| ApiError::failed(format!("{file} did not reach the bin")))?,
            };
            let view = edit(
                backend,
                Command::AddClipAtFirstFree {
                    media_id,
                    start: base + track.start,
                },
            )?;
            view.get("createdId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| ApiError::failed(format!("{file} could not be placed")))
        };

        let screen_clip = add(backend, &finished.screen)?;
        let camera_clip = match &finished.camera {
            Some(camera) => Some(add(backend, camera)?),
            None => None,
        };
        let voice_clip = match &finished.voice {
            Some(voice) => Some(add(backend, voice)?),
            None => None,
        };
        if let (Some(clip_id), Some(camera)) = (&camera_clip, &finished.camera) {
            let view = backend.call(Request::ProjectGet { path: path.clone() })?;
            let size = |key: &str| {
                view.pointer(&format!("/settings/{key}"))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
            };
            let frame = (size("width"), size("height"));
            let picture = (f64::from(camera.width), f64::from(camera.height));
            let (patch, picture, share) = screen::shaped(shape, picture);
            if let Some(patch) = patch {
                edit(
                    backend,
                    Command::UpdateClip {
                        clip_id: clip_id.clone(),
                        patch,
                    },
                )?;
            }
            if let Some((scale, offset_x, offset_y)) = screen::corner(picture, frame, share) {
                edit(
                    backend,
                    Command::SetClipTransform {
                        clip_id: clip_id.clone(),
                        scale: Some(scale),
                        offset_x: Some(offset_x),
                        offset_y: Some(offset_y),
                        rotation: None,
                        stretch_x: None,
                        stretch_y: None,
                    },
                )?;
            }
        }
        Ok(json!({
            "job": job,
            "path": path,
            "clips": {
                "screen": screen_clip,
                "camera": camera_clip,
                "voice": voice_clip,
            },
            "note": "Each import and placement is its own undoable step; project_save keeps them.",
        }))
    }
}

/// The argument schemas of the recorder tools, by tool name.
pub fn schema(tool: &str) -> Option<Value> {
    let job = json!({ "type": "string", "description": "The job id recorder_start returned, e.g. \"r1\"." });
    Some(match tool {
        "recorder_devices" => json!({ "type": "object", "properties": {} }),
        "recorder_start" => json!({
            "type": "object",
            "properties": {
                "folder": { "type": "string", "description": "An absolute folder to write the files into; made if missing. Give this or `path`." },
                "path": { "type": "string", "description": "A project folder: the files go into its `recordings` folder. Give this or `folder`." },
                "area": {
                    "type": "object",
                    "description": "The part of the screen to record, in screen pixels from the top left; the whole screen when absent.",
                    "properties": {
                        "x": { "type": "integer", "minimum": 0 },
                        "y": { "type": "integer", "minimum": 0 },
                        "width": { "type": "integer", "minimum": 2 },
                        "height": { "type": "integer", "minimum": 2 },
                    },
                    "required": ["x", "y", "width", "height"],
                },
                "fps": { "type": "integer", "minimum": 1, "maximum": 60, "default": 30, "description": "Screen pictures a second, usually 30 or 60." },
                "camera": { "type": "string", "description": "A camera's `id` from recorder_devices, to record beside the screen." },
                "microphone": { "type": "string", "description": "A microphone's name from recorder_devices, to record beside the screen." },
                "system_audio": { "type": "boolean", "default": false, "description": "Record what the computer plays too, as a sound track of its own. Windows only." },
            },
        }),
        "recorder_status" | "recorder_stop" => json!({
            "type": "object",
            "properties": { "job": job },
            "required": ["job"],
        }),
        "recorder_pause" => json!({
            "type": "object",
            "properties": {
                "job": job,
                "paused": { "type": "boolean", "default": true, "description": "true holds the recording (the pause is cut from every file); false carries it on." },
            },
            "required": ["job"],
        }),
        "recorder_land" => json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "An open project's folder." },
                "job": job,
                "start": { "type": "number", "minimum": 0, "default": 0, "description": "Where the recording begins on the timeline, in seconds." },
                "cameraShape": { "type": "string", "enum": ["rectangle", "rounded", "circle"], "default": "rectangle", "description": "How the camera is cut over the screen." },
            },
            "required": ["path", "job"],
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recorder_can_be_shared_between_threads() {
        fn shared<T: Send + Sync>() {}
        shared::<Recorder>();
    }

    #[test]
    fn a_folder_outside_the_roots_is_refused() {
        let root = tempfile::tempdir().expect("a scratch folder");
        let roots = vec![root.path().canonicalize().expect("resolves")];
        assert!(writable(&root.path().join("new").join("deeper"), &roots).is_ok());
        let outside = writable(&root.path().join("..").join("x"), &roots).expect_err("refused");
        assert_eq!(outside.code, ErrorCode::Refused);
        let elsewhere = tempfile::tempdir().expect("another");
        assert!(writable(elsewhere.path(), &roots).is_err());
        assert!(writable(Path::new("relative"), &roots).is_err());
    }

    #[test]
    fn a_start_is_checked_before_any_device_opens() {
        let recorder = Recorder::default();
        let root = tempfile::tempdir().expect("a scratch folder");
        let roots = vec![root.path().canonicalize().expect("resolves")];
        let folder = root.path().join("rec").display().to_string();
        for wrong in [
            json!({}),
            json!({ "folder": folder, "fps": 0 }),
            json!({ "folder": folder, "fps": 120 }),
            json!({ "folder": folder, "area": { "x": 0, "y": 0, "width": 1 , "height": 10 } }),
            json!({ "folder": folder, "path": folder }),
            json!({ "folder": folder, "loud": true }),
        ] {
            let error = recorder.start(&wrong, &roots).expect_err("refused");
            assert_eq!(error.code, ErrorCode::Invalid, "{wrong}: {}", error.message);
        }
        assert_eq!(
            recorder
                .stop(&json!({ "job": "r9" }))
                .expect_err("none")
                .code,
            ErrorCode::NotFound
        );
    }
}
