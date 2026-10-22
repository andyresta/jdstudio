// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! The screen recorder: the sheet that says what to record, the area
//! picker, the bar that floats over everything while it records, and the
//! landing of the files in the editor afterwards.
//!
//! Record from the launch screen and the recording becomes a new project
//! the size of what was recorded; record from the editor and it lands in
//! the project that is open, at the playhead. Either way the screen goes on
//! a lane of its own, the camera over it in the bottom right corner where
//! it can still be moved or cut, and the voice and what the computer
//! played under both, each at the moment its device actually began so all
//! of them stay in step.
//!
//! While it records the editor is minimised and a small bar of its own
//! (`RecorderBar`) stays on top with the time, Pause and Stop. On Windows
//! the bar is kept out of every capture, so it is not in the recording even
//! when it sits inside the recorded area.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use concat_core::frame::Frame;
use concat_host::media::{self, MediaSummary};
use concat_host::record;
use concat_host::screen::{
    CameraPreview, CameraShape, Finished, Plan, Recording, Tap, Track, corner, shaped,
};
use concat_media::capture::{self, Area, Camera};
use concat_project::Command;
use slint::ComponentHandle;

use crate::host::{Shell, on_ui, spawn, spawn_in_project};
use crate::i18n::{t, tf};
use crate::panes::Msg;
use crate::studio::Studio;
use crate::ui::{App, AreaPicker, CameraPanel, I18n, RecorderBar, RecorderData, Theme};

/// Seconds counted in before the recording starts.
const COUNT_IN: u8 = 3;

/// The floating camera panel, open and folded, in logical pixels.
const PANEL_OPEN: (f32, f32) = (320.0, 180.0);
const PANEL_ROUND: (f32, f32) = (200.0, 200.0);
const PANEL_FOLDED: (f32, f32) = (68.0, 44.0);

/// The camera's shapes, as the sheet lists them.
const SHAPE_RECTANGLE: i32 = 0;
const SHAPE_ROUNDED: i32 = 1;
const SHAPE_CIRCLE: i32 = 2;

/// How wide the panel's picture is made: no wider than the panel shows it.
const PANEL_PICTURE: u32 = 480;

/// Everything that can happen to the recorder.
pub enum RecorderMsg {
    /// Put the sheet up.
    Open,
    /// Take it down.
    Close,
    /// The microphones and cameras the system offers.
    Listed(Vec<String>, Vec<Camera>),
    /// Whole screen (0) or an area (1).
    Mode(i32),
    /// Pick an area on a still of the screen.
    PickArea,
    /// The still is taken, or why not.
    Snapped(Box<Result<Frame, String>>),
    /// An area picked, and whether to start recording it at once.
    AreaChosen(Area, bool),
    AreaCancelled,
    /// A device list row: 0 is none.
    Microphone(i32),
    /// Record what the computer plays, or not.
    SystemAudio(bool),
    Camera(i32),
    Fps(i32),
    Record,
    /// A second of the count-in.
    Tick,
    /// The devices opened and the recording runs, or why not.
    Started(Box<Result<Recording, String>>),
    PauseToggled,
    Stop,
    /// The files are finished, or why not.
    Finished(Box<Result<Finished, String>>),
    /// Make the project they go into, a turn after the overlay says so.
    Settle(Box<Finished>),
    /// The camera is open for its preview, or why not. The number is the
    /// opening it answers, so a late answer for an older pick is dropped.
    CameraOpened(u32, Box<Result<CameraPreview, String>>),
    /// The microphone is open for its meter, or why not; as above.
    MicrophoneOpened(u32, Box<Result<record::Microphone, String>>),
    /// What the computer plays is open for its meter, or why not; as above.
    SystemOpened(u32, Box<Result<record::Microphone, String>>),
    /// The camera panel folded or unfolded.
    CameraToggled,
    /// The camera's shape picked: see `SHAPE_RECTANGLE` and on.
    CameraShape(i32),
    /// The files, probed, ready to go into the bin and onto the lanes.
    Probed(Box<Probed>),
}

impl std::fmt::Debug for RecorderMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Open => "Open",
            Self::Close => "Close",
            Self::Listed(..) => "Listed",
            Self::Mode(_) => "Mode",
            Self::PickArea => "PickArea",
            Self::Snapped(_) => "Snapped",
            Self::AreaChosen(..) => "AreaChosen",
            Self::AreaCancelled => "AreaCancelled",
            Self::Microphone(_) => "Microphone",
            Self::SystemAudio(_) => "SystemAudio",
            Self::Camera(_) => "Camera",
            Self::Fps(_) => "Fps",
            Self::Record => "Record",
            Self::Tick => "Tick",
            Self::Started(_) => "Started",
            Self::PauseToggled => "PauseToggled",
            Self::Stop => "Stop",
            Self::Finished(_) => "Finished",
            Self::Settle(_) => "Settle",
            Self::CameraOpened(..) => "CameraOpened",
            Self::MicrophoneOpened(..) => "MicrophoneOpened",
            Self::SystemOpened(..) => "SystemOpened",
            Self::CameraToggled => "CameraToggled",
            Self::CameraShape(_) => "CameraShape",
            Self::Probed(_) => "Probed",
        };
        f.write_str(name)
    }
}

/// The recorded files, probed, and where on the timeline they go.
pub struct Probed {
    /// Seconds the earliest file lands at.
    base: f64,
    screen: (Track, Result<MediaSummary, String>),
    camera: Option<(Track, Result<MediaSummary, String>)>,
    voice: Option<(Track, Result<MediaSummary, String>)>,
    system: Option<(Track, Result<MediaSummary, String>)>,
}

/// Where the recorder is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Idle,
    /// Counting in, seconds left.
    Counting(u8),
    /// The devices are opening.
    Starting,
    Rolling,
    /// The files are being finished.
    Saving,
}

#[derive(Default)]
pub struct RecorderPane {
    open: bool,
    mode: i32,
    area: Option<Area>,
    microphones: Vec<String>,
    cameras: Vec<Camera>,
    microphone: i32,
    camera: i32,
    /// Record what the computer plays too.
    system_audio: bool,
    fps: i32,
    listing: bool,
    busy: bool,
    error: String,
    phase: Phase,
    recording: Option<Recording>,
    timer: Option<slint::Timer>,
    /// Where the files are being written.
    folder: Option<PathBuf>,
    /// The last still of the screen, for the picked area's preview.
    still: Option<Frame>,
    /// The overlay between Stop and the editor: 0 down, 1 finishing the
    /// files, 2 making the project, 3 laying the clips out.
    loading: i32,
    /// The camera and microphone open for the preview while the sheet is
    /// up; handed to the recording when it starts.
    preview_camera: Option<CameraPreview>,
    preview_microphone: Option<record::Microphone>,
    preview_system: Option<record::Microphone>,
    /// Counted up at every opening, so a late answer is told apart.
    camera_opening: u32,
    microphone_opening: u32,
    system_opening: u32,
    /// The camera's pictures, for the floating panel: the preview's, then
    /// the recording's.
    tap: Option<Tap>,
    /// Draws the panel's picture and the meters.
    meter: Option<slint::Timer>,
    /// The panel is folded into its pill.
    camera_folded: bool,
    /// The camera's shape; see `SHAPE_RECTANGLE` and on.
    camera_shape: i32,
}

thread_local! {
    /// The bar and the picker: windows of their own, made the first time
    /// they are wanted and kept for the next.
    static BAR: RefCell<Option<RecorderBar>> = const { RefCell::new(None) };
    static PICKER: RefCell<Option<AreaPicker>> = const { RefCell::new(None) };
    static CAMERA: RefCell<Option<CameraPanel>> = const { RefCell::new(None) };
    /// The camera panel has been put in its corner once; after that it
    /// stays wherever it was dragged.
    static CAMERA_PLACED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl RecorderPane {
    pub fn update(&mut self, msg: RecorderMsg, studio: &mut Studio) {
        match msg {
            RecorderMsg::Open => self.open(),
            RecorderMsg::Close => {
                if self.phase == Phase::Idle {
                    self.open = false;
                    self.error.clear();
                    self.close_previews();
                }
            }
            RecorderMsg::Listed(microphones, cameras) => {
                self.listing = false;
                self.microphones = microphones;
                self.cameras = cameras;
                // The ones remembered, else the system's default microphone
                // and no camera: a camera is a choice, a voice is expected.
                self.microphone = remembered(&studio.prefs.recorder_microphone, &self.microphones)
                    .unwrap_or(if self.microphones.is_empty() { 0 } else { 1 });
                let labels: Vec<String> = self.cameras.iter().map(|c| c.label.clone()).collect();
                self.camera = remembered(&studio.prefs.recorder_camera, &labels).unwrap_or(0);
                self.camera_shape = i32::from(studio.prefs.recorder_camera_shape)
                    .clamp(SHAPE_RECTANGLE, SHAPE_CIRCLE);
                self.system_audio =
                    studio.prefs.recorder_system_audio && record::system_audio_available();
                self.reopen_camera();
                self.reopen_microphone();
                self.reopen_system();
            }
            RecorderMsg::Mode(mode) => {
                self.mode = mode.clamp(0, 1);
                if self.mode == 1 && self.area.is_none() {
                    self.pick_area();
                }
            }
            RecorderMsg::PickArea => self.pick_area(),
            RecorderMsg::Snapped(result) => match *result {
                Ok(frame) => {
                    show_picker(&frame, self.area);
                    self.still = Some(frame);
                }
                Err(error) => {
                    self.busy = false;
                    self.error = tf("recorder.failed", &[&error]);
                    restore_main();
                }
            },
            RecorderMsg::AreaChosen(area, record) => {
                hide_picker();
                self.busy = false;
                self.area = Some(area);
                self.mode = 1;
                let preview = self.still.as_ref().and_then(|still| cut(still, area));
                Shell::with(|_, app| {
                    app.set_recorder_preview(
                        preview
                            .as_ref()
                            .map(crate::host::image_of)
                            .unwrap_or_default(),
                    );
                });
                if record {
                    // Straight on: the editor stays out of the way.
                    self.record(studio);
                } else {
                    restore_main();
                }
            }
            RecorderMsg::AreaCancelled => {
                hide_picker();
                restore_main();
                self.busy = false;
                if self.area.is_none() {
                    self.mode = 0;
                }
            }
            RecorderMsg::Microphone(index) => {
                self.microphone = index;
                studio.prefs.recorder_microphone = self.microphone_name();
                studio.prefs.save(&studio.host.dirs);
                self.reopen_microphone();
            }
            RecorderMsg::SystemAudio(on) => {
                self.system_audio = on && record::system_audio_available();
                studio.prefs.recorder_system_audio = self.system_audio;
                studio.prefs.save(&studio.host.dirs);
                self.reopen_system();
            }
            RecorderMsg::Camera(index) => {
                self.camera = index;
                studio.prefs.recorder_camera = self.camera_pick().map(|camera| camera.label);
                studio.prefs.save(&studio.host.dirs);
                self.reopen_camera();
            }
            RecorderMsg::CameraOpened(token, result) => {
                if token != self.camera_opening || !self.previewing() {
                    return;
                }
                match *result {
                    Ok(preview) => {
                        self.tap = Some(preview.tap());
                        self.preview_camera = Some(preview);
                        show_camera(self.camera_folded, self.camera_shape);
                        self.ensure_meter();
                    }
                    Err(error) => {
                        log::warn!("screen recording: the camera would not open: {error}");
                        self.error = tf("recorder.cameraFailed", &[&error]);
                    }
                }
            }
            RecorderMsg::MicrophoneOpened(token, result) => {
                if token != self.microphone_opening || !self.previewing() {
                    return;
                }
                match *result {
                    Ok(microphone) => {
                        self.preview_microphone = Some(microphone);
                        self.ensure_meter();
                    }
                    Err(error) => {
                        log::warn!("screen recording: the microphone would not open: {error}");
                        self.error = tf("recorder.microphoneFailed", &[&error]);
                    }
                }
            }
            RecorderMsg::SystemOpened(token, result) => {
                if token != self.system_opening || !self.previewing() {
                    return;
                }
                match *result {
                    Ok(system) => {
                        self.preview_system = Some(system);
                        self.ensure_meter();
                    }
                    Err(error) => {
                        log::warn!("screen recording: system audio would not open: {error}");
                        self.error = tf("recorder.systemAudioFailed", &[&error]);
                    }
                }
            }
            RecorderMsg::CameraToggled => {
                self.camera_folded = !self.camera_folded;
                fold_camera(self.camera_folded, self.camera_shape);
            }
            RecorderMsg::CameraShape(shape) => {
                self.camera_shape = shape.clamp(SHAPE_RECTANGLE, SHAPE_CIRCLE);
                studio.prefs.recorder_camera_shape = u8::try_from(self.camera_shape).unwrap_or(0);
                studio.prefs.save(&studio.host.dirs);
                fold_camera(self.camera_folded, self.camera_shape);
            }
            RecorderMsg::Fps(index) => self.fps = index.clamp(0, 1),
            RecorderMsg::Record => self.record(studio),
            RecorderMsg::Tick => self.tick(studio),
            RecorderMsg::Started(result) => self.started(*result),
            RecorderMsg::PauseToggled => {
                if let Some(recording) = self.recording.as_mut() {
                    if recording.paused() {
                        recording.resume();
                    } else {
                        recording.pause();
                    }
                    let paused = recording.paused();
                    with_bar(|bar| bar.set_paused(paused));
                }
            }
            RecorderMsg::Stop => self.stop(),
            RecorderMsg::Finished(result) => self.finished(*result, studio),
            RecorderMsg::Settle(finished) => self.settle(*finished, studio),
            RecorderMsg::Probed(probed) => self.land(*probed, studio),
        }
    }

    /// The sheet's state, for the window.
    pub fn data(&self) -> RecorderData {
        let area = self
            .area
            .map(|area| {
                tf(
                    "recorder.areaAt",
                    &[
                        &format!("{} × {}", area.width & !1, area.height & !1),
                        &format!("{}, {}", area.x, area.y),
                    ],
                )
            })
            .unwrap_or_default();
        RecorderData {
            open: self.open,
            mode: self.mode,
            area: area.into(),
            microphone: self.microphone,
            camera: self.camera,
            system_audio: self.system_audio,
            system_audio_available: record::system_audio_available(),
            fps: self.fps,
            camera_shape: self.camera_shape,
            listing: self.listing,
            busy: self.busy || self.phase != Phase::Idle,
            loading: self.loading,
            error: self.error.clone().into(),
        }
    }

    /// "None", then each microphone.
    pub fn microphone_rows(&self) -> Vec<slint::SharedString> {
        std::iter::once(t("recorder.none"))
            .chain(self.microphones.iter().cloned())
            .map(Into::into)
            .collect()
    }

    /// "None", then each camera.
    pub fn camera_rows(&self) -> Vec<slint::SharedString> {
        std::iter::once(t("recorder.none"))
            .chain(self.cameras.iter().map(|camera| camera.label.clone()))
            .map(Into::into)
            .collect()
    }

    fn microphone_name(&self) -> Option<String> {
        usize::try_from(self.microphone - 1)
            .ok()
            .and_then(|index| self.microphones.get(index).cloned())
    }

    fn camera_pick(&self) -> Option<Camera> {
        usize::try_from(self.camera - 1)
            .ok()
            .and_then(|index| self.cameras.get(index).cloned())
    }

    /// Whether the previews are wanted: the sheet up, or a recording
    /// being counted in.
    fn previewing(&self) -> bool {
        self.open || matches!(self.phase, Phase::Counting(_))
    }

    /// Opens the picked camera for the floating panel, closing the one open.
    fn reopen_camera(&mut self) {
        self.preview_camera = None;
        self.tap = None;
        hide_camera();
        self.camera_opening = self.camera_opening.wrapping_add(1);
        let Some(camera) = self.camera_pick() else {
            return;
        };
        let token = self.camera_opening;
        spawn(
            move || CameraPreview::open(&camera.id),
            move |studio, _, _, result| {
                studio.handle(Msg::Recorder(RecorderMsg::CameraOpened(
                    token,
                    Box::new(result),
                )));
            },
        );
    }

    /// Opens the picked microphone for the sheet's meter, closing the one
    /// open.
    fn reopen_microphone(&mut self) {
        self.preview_microphone = None;
        self.microphone_opening = self.microphone_opening.wrapping_add(1);
        Shell::with(|_, app| app.set_recorder_level(0.0));
        let Some(name) = self.microphone_name() else {
            return;
        };
        if !crate::platform::microphone_allowed() {
            return;
        }
        let token = self.microphone_opening;
        spawn(
            move || record::Microphone::open(Some(&name)),
            move |studio, _, _, result| {
                studio.handle(Msg::Recorder(RecorderMsg::MicrophoneOpened(
                    token,
                    Box::new(result),
                )));
            },
        );
    }

    /// Opens what the computer plays for the sheet's meter when it is to be
    /// recorded, closing what is open.
    fn reopen_system(&mut self) {
        self.preview_system = None;
        self.system_opening = self.system_opening.wrapping_add(1);
        Shell::with(|_, app| app.set_recorder_system_level(0.0));
        if !self.system_audio {
            return;
        }
        let token = self.system_opening;
        spawn(
            record::Microphone::open_system,
            move |studio, _, _, result| {
                studio.handle(Msg::Recorder(RecorderMsg::SystemOpened(
                    token,
                    Box::new(result),
                )));
            },
        );
    }

    /// Closes the previews and the panel.
    fn close_previews(&mut self) {
        self.camera_opening = self.camera_opening.wrapping_add(1);
        self.microphone_opening = self.microphone_opening.wrapping_add(1);
        self.system_opening = self.system_opening.wrapping_add(1);
        self.preview_camera = None;
        self.preview_microphone = None;
        self.preview_system = None;
        self.tap = None;
        self.meter = None;
        hide_camera();
    }

    fn ensure_meter(&mut self) {
        if self.meter.is_none() {
            self.meter = Some(meter());
        }
    }

    fn open(&mut self) {
        if self.phase != Phase::Idle {
            return;
        }
        self.open = true;
        self.error.clear();
        if !crate::platform::microphone_allowed() {
            self.error = t("voiceover.permission");
        }
        // Listing devices can take a moment - DirectShow wakes each one -
        // so the sheet is up meanwhile, saying so.
        self.listing = true;
        spawn(
            || (record::microphones(), capture::cameras()),
            |studio, _, _, (microphones, cameras)| {
                studio.handle(Msg::Recorder(RecorderMsg::Listed(microphones, cameras)));
            },
        );
    }

    /// Gets the editor out of the way, takes a still of the screen and puts
    /// the picker up over it.
    fn pick_area(&mut self) {
        self.busy = true;
        self.error.clear();
        minimize_main();
        // Long enough for the minimise to finish, so the editor is not in
        // the still.
        slint::Timer::single_shot(Duration::from_millis(400), || {
            spawn(
                || capture::snapshot().map_err(|error| error.to_string()),
                |studio, _, _, result| {
                    studio.handle(Msg::Recorder(RecorderMsg::Snapped(Box::new(result))));
                },
            );
        });
    }

    fn record(&mut self, studio: &mut Studio) {
        if self.phase != Phase::Idle {
            return;
        }
        if self.mode == 1 && self.area.is_none() {
            self.error = t("recorder.pickAreaFirst");
            return;
        }
        // Into the open project, or a folder of the app's until the new
        // project it becomes exists.
        let folder = match &studio.session {
            Some(session) => PathBuf::from(session.path()).join("recordings"),
            None => studio.host.dirs.data.join("recordings").join(stamp()),
        };
        self.folder = Some(folder);
        self.error.clear();
        self.open = false;
        self.phase = Phase::Counting(COUNT_IN);
        minimize_main();
        show_bar();
        let has_mic = self.microphone_name().is_some() || self.system_audio;
        with_bar(|bar| {
            bar.set_has_mic(has_mic);
            bar.set_level(0.0);
            bar.set_counting(i32::from(COUNT_IN));
            bar.set_paused(false);
            bar.set_saving(false);
            bar.set_time("00:00".into());
        });
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), || {
            send(RecorderMsg::Tick);
        });
        self.timer = Some(timer);
    }

    fn tick(&mut self, studio: &mut Studio) {
        let Phase::Counting(left) = self.phase else {
            return;
        };
        if left > 1 {
            self.phase = Phase::Counting(left - 1);
            with_bar(|bar| bar.set_counting(i32::from(left - 1)));
            return;
        }
        self.timer = None;
        self.phase = Phase::Starting;
        with_bar(|bar| bar.set_counting(0));
        // The devices the preview has open go to the recording as they are.
        let camera = self.preview_camera.take();
        let microphone = self.preview_microphone.take();
        let system = self.preview_system.take();
        let plan = Plan {
            folder: self
                .folder
                .clone()
                .unwrap_or_else(|| studio.host.dirs.data.join("recordings")),
            area: if self.mode == 1 { self.area } else { None },
            fps: if self.fps == 1 { 60 } else { 30 },
            camera: self.camera_pick().map(|camera| camera.id),
            microphone: self.microphone_name(),
            system_audio: self.system_audio,
        };
        log::info!(
            "screen recording: starting, area {:?}, {} fps, camera {}, microphone {}, \
             system audio {}",
            plan.area,
            plan.fps,
            plan.camera.is_some(),
            plan.microphone.is_some(),
            plan.system_audio
        );
        spawn(
            move || {
                let camera = camera.map(CameraPreview::into_device).transpose()?;
                Recording::start(plan, camera, microphone, system)
            },
            |studio, _, _, result| {
                studio.handle(Msg::Recorder(RecorderMsg::Started(Box::new(result))));
            },
        );
    }

    fn started(&mut self, result: Result<Recording, String>) {
        match result {
            Ok(recording) => {
                self.recording = Some(recording);
                self.phase = Phase::Rolling;
                self.timer = Some(clock());
            }
            Err(error) => {
                log::warn!("screen recording: would not start: {error}");
                self.phase = Phase::Idle;
                hide_bar();
                restore_main();
                self.open = true;
                self.error = tf("recorder.failed", &[&error]);
                // The devices went with the attempt; open them again.
                self.reopen_camera();
                self.reopen_microphone();
                self.reopen_system();
            }
        }
    }

    fn stop(&mut self) {
        match self.phase {
            // Stopped before it began: nothing was written.
            Phase::Counting(_) => {
                self.timer = None;
                self.phase = Phase::Idle;
                hide_bar();
                restore_main();
                self.open = true;
            }
            Phase::Rolling => {
                let Some(recording) = self.recording.take() else {
                    return;
                };
                self.timer = None;
                self.phase = Phase::Saving;
                // Straight back to the window, under the overlay, which
                // stays up until the clips are on the lanes: one hand-over,
                // not the launch screen, then the editor, then clips
                // arriving one by one.
                hide_bar();
                self.close_previews();
                restore_main();
                self.loading = 1;
                spawn(
                    move || recording.stop(),
                    |studio, _, _, result| {
                        studio.handle(Msg::Recorder(RecorderMsg::Finished(Box::new(result))));
                    },
                );
            }
            _ => {}
        }
    }

    fn finished(&mut self, result: Result<Finished, String>, studio: &mut Studio) {
        self.phase = Phase::Idle;
        hide_bar();
        restore_main();
        let finished = match result {
            Ok(finished) => finished,
            Err(error) => {
                log::warn!("screen recording: failed: {error}");
                self.loading = 0;
                studio.notify(&tf("recorder.failed", &[&error]), true);
                return;
            }
        };
        log::info!(
            "screen recording: {}x{}, camera {}, voice {}, system audio {}",
            finished.screen.width,
            finished.screen.height,
            finished.camera.is_some(),
            finished.voice.is_some(),
            finished.system.is_some()
        );
        if studio.session.is_none() {
            // Said on the overlay first; made on the next turn, once the
            // words are drawn.
            self.loading = 2;
            send(RecorderMsg::Settle(Box::new(finished)));
        } else {
            self.settle(finished, studio);
        }
    }

    /// The project made when there is none, then the files probed for the
    /// lanes.
    fn settle(&mut self, mut finished: Finished, studio: &mut Studio) {
        let base = if studio.session.is_some() {
            f64::from(studio.playhead.max(0.0))
        } else {
            match self.new_project(&mut finished, studio) {
                Ok(()) => 0.0,
                Err(error) => {
                    self.loading = 0;
                    studio.notify(&tf("recorder.failed", &[&error]), true);
                    return;
                }
            }
        };
        self.loading = 3;
        let probe = |track: Track| {
            let summary = media::probe(&track.path.to_string_lossy());
            (track, summary)
        };
        spawn_in_project(
            move || Probed {
                base,
                screen: probe(finished.screen),
                camera: finished.camera.map(probe),
                voice: finished.voice.map(probe),
                system: finished.system.map(probe),
            },
            |studio, _, _, probed| {
                studio.handle(Msg::Recorder(RecorderMsg::Probed(Box::new(probed))));
            },
        );
    }

    /// Makes a project the size of the recording, opens it, and moves the
    /// files into it.
    fn new_project(&self, finished: &mut Finished, studio: &mut Studio) -> Result<(), String> {
        let location = studio
            .prefs
            .last_project_location
            .clone()
            .filter(|location| !location.trim().is_empty())
            .unwrap_or_else(|| studio.start.location.clone());
        if location.trim().is_empty() {
            return Err(t("start.chooseWhereProjectFolder"));
        }
        let base = t("recorder.projectName");
        let name = (1..1000)
            .map(|n| {
                if n == 1 {
                    base.clone()
                } else {
                    format!("{base} {n}")
                }
            })
            .find(|name| {
                !Path::new(&location)
                    .join(concat_host::projects::folder_name(name))
                    .exists()
            })
            .unwrap_or(base);
        let (width, height) = (finished.screen.width.max(2), finished.screen.height.max(2));
        let info = concat_host::projects::create(
            &location,
            &name,
            width,
            height,
            i64::from(finished.fps),
            1,
        )?;
        let into = PathBuf::from(&info.path).join("recordings");
        studio.open_project(info)?;
        std::fs::create_dir_all(&into).map_err(|error| error.to_string())?;
        for track in std::iter::once(&mut finished.screen)
            .chain(finished.camera.as_mut())
            .chain(finished.voice.as_mut())
            .chain(finished.system.as_mut())
        {
            track.path = move_into(&track.path, &into)?;
        }
        Ok(())
    }

    /// The files into the bin, and onto lanes: screen, camera over it in
    /// the corner, voice, what the computer played.
    fn land(&mut self, probed: Probed, studio: &mut Studio) {
        let (width, height) = studio.output_size();
        let frame = (f64::from(width.max(1)), f64::from(height.max(1)));
        let mut add = |(track, summary): (Track, Result<MediaSummary, String>)| -> Option<String> {
            let summary = match summary {
                Ok(summary) => summary,
                Err(error) => {
                    log::warn!(
                        "screen recording: {} would not read: {error}",
                        track.path.display()
                    );
                    return None;
                }
            };
            let media_id = studio
                .apply(Command::AddMedia {
                    item: summary.to_new_media(),
                })
                .or_else(|| {
                    studio
                        .project()
                        .media
                        .iter()
                        .find(|item| item.path == summary.path)
                        .map(|item| item.id.clone())
                })?;
            studio.apply(Command::AddClipAtFirstFree {
                media_id,
                start: probed.base + track.start,
            })
        };
        let screen = add(probed.screen);
        let camera_size = probed
            .camera
            .as_ref()
            .map(|(track, _)| (f64::from(track.width), f64::from(track.height)));
        let shape = self.camera_shape;
        let camera = probed.camera.and_then(&mut add);
        if let Some(voice) = probed.voice {
            add(voice);
        }
        if let Some(system) = probed.system {
            add(system);
        }
        if let (Some(clip_id), Some(picture)) = (camera, camera_size) {
            let (patch, picture, share) = shaped(CameraShape::from_index(shape), picture);
            if let Some(patch) = patch {
                studio.apply(Command::UpdateClip {
                    clip_id: clip_id.clone(),
                    patch,
                });
            }
            if let Some((scale, offset_x, offset_y)) = corner(picture, frame, share) {
                studio.apply(Command::SetClipTransform {
                    clip_id,
                    scale: Some(scale),
                    offset_x: Some(offset_x),
                    offset_y: Some(offset_y),
                    rotation: None,
                    stretch_x: None,
                    stretch_y: None,
                });
            }
        }
        self.loading = 0;
        if screen.is_some() {
            studio.notify(&t("recorder.added"), false);
        } else {
            studio.notify(
                &tf("recorder.failed", &[&t("recorder.screenUnreadable")]),
                true,
            );
        }
    }
}

/// The part of `still` an area covers: the dialog's preview of it.
fn cut(still: &Frame, area: Area) -> Option<Frame> {
    let area = area.within(still.width(), still.height())?;
    let row = area.width as usize * 4;
    let stride = still.width() as usize * 4;
    let mut pixels = Vec::with_capacity(row * area.height as usize);
    for y in area.y as usize..(area.y + area.height) as usize {
        let start = y * stride + area.x as usize * 4;
        pixels.extend_from_slice(&still.pixels()[start..start + row]);
    }
    Frame::from_rgba(area.width, area.height, pixels)
}

/// The panel's size and outline for a shape, open or folded.
fn panel_form(folded: bool, shape: i32) -> ((f32, f32), crate::platform::WindowShape) {
    use crate::platform::WindowShape;
    match (folded, shape) {
        (true, _) => (PANEL_FOLDED, WindowShape::Rounded(12.0)),
        (false, SHAPE_CIRCLE) => (PANEL_ROUND, WindowShape::Ellipse),
        (false, SHAPE_ROUNDED) => (PANEL_OPEN, WindowShape::Rounded(20.0)),
        (false, _) => (PANEL_OPEN, WindowShape::Square),
    }
}

/// The row of `remembered` in a list that starts with "None", if it is
/// still there.
fn remembered(remembered: &Option<String>, names: &[String]) -> Option<i32> {
    let wanted = remembered.as_ref()?;
    names
        .iter()
        .position(|name| name == wanted)
        .and_then(|index| i32::try_from(index + 1).ok())
}

/// Moves a file into `folder`, by rename where it can and by copy where the
/// folder is on another disk.
fn move_into(file: &Path, folder: &Path) -> Result<PathBuf, String> {
    let name = file.file_name().ok_or("a recording has no name")?;
    let target = folder.join(name);
    if std::fs::rename(file, &target).is_err() {
        std::fs::copy(file, &target).map_err(|error| error.to_string())?;
        let _ = std::fs::remove_file(file);
    }
    Ok(target)
}

fn stamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default()
        .to_string()
}

/// Hands `msg` to the recorder from a window's callback or a timer, with
/// the studio free to borrow, and publishes after.
fn send(msg: RecorderMsg) {
    on_ui(move |studio, _, _| studio.handle(Msg::Recorder(msg)));
}

/// The camera panel's picture and the microphone meters, thirty times a
/// second, straight onto the windows with no publish around it: the
/// sheet's meter from the preview's microphone, the bar's from the
/// recording's, and the panel from whichever has the camera.
fn meter() -> slint::Timer {
    let timer = slint::Timer::default();
    let mut shown = 0.0f32;
    let mut shown_system = 0.0f32;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(33),
        move || {
            Shell::with(|shell, app| {
                let Ok(studio) = shell.studio.try_borrow() else {
                    return;
                };
                let pane = &studio.recorder;
                let level = pane
                    .recording
                    .as_ref()
                    .and_then(Recording::level)
                    .or_else(|| {
                        pane.preview_microphone
                            .as_ref()
                            .map(record::Microphone::level)
                    });
                let system = pane
                    .recording
                    .as_ref()
                    .and_then(Recording::system_level)
                    .or_else(|| pane.preview_system.as_ref().map(record::Microphone::level));
                // Up at once, down gently, as the voiceover's meter.
                let peak = level.map_or(0.0, |level| record::meter_position(level.peak));
                shown = peak.max(shown * 0.85);
                let peak = system.map_or(0.0, |level| record::meter_position(level.peak));
                shown_system = peak.max(shown_system * 0.85);
                app.set_recorder_level(shown);
                app.set_recorder_system_level(shown_system);
                // The bar has one meter: the voice, or the computer's sound
                // when no voice is recorded.
                let bar = if level.is_some() { shown } else { shown_system };
                with_bar(|bar_window| bar_window.set_level(bar));
                let picture = pane.tap.as_ref().and_then(|tap| {
                    tap.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take()
                });
                if let Some(picture) = picture {
                    let small = mirrored_thumbnail(&picture, PANEL_PICTURE);
                    with_camera(|panel| {
                        panel.set_picture(crate::host::image_of(&small));
                        panel.set_live(true);
                    });
                }
            });
        },
    );
    timer
}

/// `frame` at most `width` wide, its shape kept, flipped left to right as
/// a mirror shows a face - nearest pixel, which at a panel's size is
/// indistinguishable from better and costs next to nothing.
fn mirrored_thumbnail(frame: &Frame, width: u32) -> Frame {
    let (fw, fh) = (frame.width().max(1), frame.height().max(1));
    let tw = fw.min(width).max(1);
    let th = ((u64::from(fh) * u64::from(tw) / u64::from(fw)) as u32).max(1);
    let source = frame.pixels();
    let mut pixels = Vec::with_capacity(tw as usize * th as usize * 4);
    for y in 0..th {
        let sy = (u64::from(y) * u64::from(fh) / u64::from(th)) as usize;
        for x in 0..tw {
            let sx = fw as usize - 1 - (u64::from(x) * u64::from(fw) / u64::from(tw)) as usize;
            let at = (sy * fw as usize + sx) * 4;
            pixels.extend_from_slice(&source[at..at + 4]);
        }
    }
    Frame::from_rgba(tw, th, pixels).expect("sized to fit")
}

fn with_camera(body: impl FnOnce(&CameraPanel)) {
    CAMERA.with(|panel| {
        if let Some(panel) = panel.borrow().as_ref() {
            body(panel);
        }
    });
}

fn show_camera(folded: bool, shape: i32) {
    let made = CAMERA.with(|slot| slot.borrow().is_some());
    if !made {
        let Ok(panel) = CameraPanel::new() else {
            log::warn!("screen recording: the camera panel would not open");
            return;
        };
        panel.on_toggled(|| send(RecorderMsg::CameraToggled));
        let weak = panel.as_weak();
        panel.on_drag(move || {
            if let Some(panel) = weak.upgrade() {
                crate::platform::begin_drag(panel.window());
            }
        });
        CAMERA.with(|slot| *slot.borrow_mut() = Some(panel));
    }
    with_camera(|panel| {
        dress_camera(panel);
        panel.set_live(false);
        panel.set_collapsed(folded);
        panel.set_shape(shape);
        let ((w, h), outline) = panel_form(folded, shape);
        panel.window().set_size(slint::LogicalSize::new(w, h));
        let _ = panel.show();
        let first = !CAMERA_PLACED.with(std::cell::Cell::get);
        CAMERA_PLACED.with(|placed| placed.set(true));
        settle(panel.as_weak(), move |window| {
            crate::platform::shape_window(window, outline, w, h)
                && (!first || crate::platform::float_corner(window))
        });
    });
}

/// Folds the panel into its pill, opens it again or gives it another
/// shape, keeping its bottom right corner where it was, so the new form
/// sits where the old one ended.
fn fold_camera(folded: bool, shape: i32) {
    with_camera(|panel| {
        let window = panel.window();
        let scale = window.scale_factor();
        let before = window.size();
        let at = window.position();
        let ((w, h), outline) = panel_form(folded, shape);
        panel.set_collapsed(folded);
        panel.set_shape(shape);
        window.set_size(slint::LogicalSize::new(w, h));
        crate::platform::shape_window(window, outline, w, h);
        let after = slint::LogicalSize::new(w, h).to_physical(scale);
        window.set_position(slint::PhysicalPosition::new(
            at.x + before.width as i32 - after.width as i32,
            at.y + before.height as i32 - after.height as i32,
        ));
    });
}

fn hide_camera() {
    with_camera(|panel| {
        let _ = panel.hide();
    });
}

/// The bar's clock, four times a second, straight onto the bar; it also
/// stops a recording a device dropped out of.
fn clock() -> slint::Timer {
    let timer = slint::Timer::default();
    let mut reported = false;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            Shell::with(|shell, _| {
                let Ok(studio) = shell.studio.try_borrow() else {
                    return;
                };
                let Some(recording) = studio.recorder.recording.as_ref() else {
                    return;
                };
                let seconds = recording.seconds() as u64;
                let time = format!("{:02}:{:02}", seconds / 60, seconds % 60);
                with_bar(|bar| bar.set_time(time.into()));
                if !reported && let Some(error) = recording.failed() {
                    reported = true;
                    log::warn!("screen recording: a source failed: {error}");
                    send(RecorderMsg::Stop);
                }
            });
        },
    );
    timer
}

fn with_bar(body: impl FnOnce(&RecorderBar)) {
    BAR.with(|bar| {
        if let Some(bar) = bar.borrow().as_ref() {
            body(bar);
        }
    });
}

fn show_bar() {
    let made = BAR.with(|slot| slot.borrow().is_some());
    if !made {
        let Ok(bar) = RecorderBar::new() else {
            log::warn!("screen recording: the bar would not open");
            return;
        };
        dress_bar(&bar);
        bar.on_pause_toggled(|| send(RecorderMsg::PauseToggled));
        bar.on_stop(|| send(RecorderMsg::Stop));
        let weak = bar.as_weak();
        bar.on_drag(move || {
            if let Some(bar) = weak.upgrade() {
                crate::platform::begin_drag(bar.window());
            }
        });
        BAR.with(|slot| *slot.borrow_mut() = Some(bar));
    }
    with_bar(|bar| {
        dress_bar(bar);
        let _ = bar.show();
        settle(bar.as_weak(), crate::platform::float_bar);
    });
}

/// Runs `place` on a window just shown, once its platform window exists:
/// winit makes it on a later turn of the event loop, so a placement made
/// straight after `show` lands on nothing. Tried every few milliseconds,
/// for at most a second.
fn settle<C, F>(window: slint::Weak<C>, place: F)
where
    C: ComponentHandle + 'static,
    F: Fn(&slint::Window) -> bool + 'static,
{
    fn attempt<C, F>(window: slint::Weak<C>, place: F, left: u32)
    where
        C: ComponentHandle + 'static,
        F: Fn(&slint::Window) -> bool + 'static,
    {
        let Some(shown) = window.upgrade() else {
            return;
        };
        if place(shown.window()) || left == 0 {
            return;
        }
        slint::Timer::single_shot(Duration::from_millis(16), move || {
            attempt(window, place, left - 1);
        });
    }
    attempt(window, place, 60);
}

fn hide_bar() {
    with_bar(|bar| {
        let _ = bar.hide();
    });
}

fn show_picker(frame: &Frame, area: Option<Area>) {
    let made = PICKER.with(|slot| slot.borrow().is_some());
    if !made {
        let Ok(picker) = AreaPicker::new() else {
            log::warn!("screen recording: the area picker would not open");
            send(RecorderMsg::AreaCancelled);
            return;
        };
        picker.on_chosen(|x, y, width, height, record| {
            let at = |value: i32| u32::try_from(value).unwrap_or(0);
            send(RecorderMsg::AreaChosen(
                Area {
                    x: at(x),
                    y: at(y),
                    width: at(width),
                    height: at(height),
                },
                record,
            ));
        });
        picker.on_cancelled(|| send(RecorderMsg::AreaCancelled));
        PICKER.with(|slot| *slot.borrow_mut() = Some(picker));
    }
    PICKER.with(|slot| {
        if let Some(picker) = slot.borrow().as_ref() {
            dress_picker(picker);
            picker.set_still(crate::host::image_of(frame));
            picker.set_still_width(i32::try_from(frame.width()).unwrap_or(1));
            picker.set_still_height(i32::try_from(frame.height()).unwrap_or(1));
            // The last area again, to adjust rather than draw anew; none
            // the first time, with the picker saying what to do.
            let area = area.and_then(|area| area.within(frame.width(), frame.height()));
            let (x, y, w, h) = area.map_or((0.0, 0.0, 0.0, 0.0), |area| {
                (
                    area.x as f32,
                    area.y as f32,
                    area.width as f32,
                    area.height as f32,
                )
            });
            picker.set_sx(x);
            picker.set_sy(y);
            picker.set_sw(w);
            picker.set_sh(h);
            picker.set_ratio(0.0);
            let _ = picker.show();
            settle(picker.as_weak(), crate::platform::cover_screen);
        }
    });
}

fn hide_picker() {
    PICKER.with(|slot| {
        if let Some(picker) = slot.borrow().as_ref() {
            let _ = picker.hide();
        }
    });
}

fn minimize_main() {
    Shell::with(|_, app| crate::platform::minimize(app.window()));
}

fn restore_main() {
    Shell::with(|_, app| crate::platform::restore(app.window()));
}

/// The new windows' globals are their own: the same words and the same
/// theme as the editor's are copied in.
macro_rules! dress {
    ($window:expr) => {{
        let window = $window;
        let words = window.global::<I18n>();
        words.on_lookup(|_, key| crate::i18n::t(&key).into());
        words.on_lookup1(|_, key, a| crate::i18n::tf(&key, &[&a]).into());
        words.on_lookup2(|_, key, a, b| crate::i18n::tf(&key, &[&a, &b]).into());
        words.on_lookup_upper(|_, key| crate::i18n::t(&key).to_uppercase().into());
        words.set_lang(crate::i18n::current().into());
        Shell::with(|_, app: App| {
            let from = app.global::<Theme>();
            let to = window.global::<Theme>();
            to.set_dark(from.get_dark());
            to.set_accent_custom(from.get_accent_custom());
            to.set_accent_choice(from.get_accent_choice());
        });
    }};
}

fn dress_bar(bar: &RecorderBar) {
    dress!(bar);
}

fn dress_picker(picker: &AreaPicker) {
    dress!(picker);
}

fn dress_camera(panel: &CameraPanel) {
    dress!(panel);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remembered_device_is_found_past_none() {
        let names = vec!["A".to_owned(), "B".to_owned()];
        assert_eq!(remembered(&Some("B".to_owned()), &names), Some(2));
        assert_eq!(remembered(&Some("C".to_owned()), &names), None);
        assert_eq!(remembered(&None, &names), None);
    }
}
