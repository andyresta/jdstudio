// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! Recording the screen: the picture, and optionally a camera and a
//! microphone beside it, each into a file of its own.
//!
//! Three files rather than one so the editor gets three clips: the screen
//! on one lane, the camera over it in a corner where it can still be moved,
//! cut or dropped, and the voice under both. Each source runs on its own
//! thread from the moment Record is pressed and notes when its first
//! picture or sample arrived; [`Finished`] gives every file its start
//! relative to the earliest, so the clips land in step.
//!
//! A video file is written at a constant rate however unevenly the device
//! delivers: a picture late from the device is held over the frames it
//! missed, and one early waits for its slot. Pause stops every source at
//! once and cuts the paused stretch out of every file alike.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use concat_core::frame::Frame;
use concat_core::time::{FrameRate, Rational};
use concat_media::capture::{Area, Grabber};
use concat_media::encode::{EncodeOptions, Encoder, FrameSink, RateMode, VideoCodec};

use crate::record::Microphone;

/// The largest camera picture recorded. It is shown in a corner, and a
/// second 1080p encode beside the screen's costs a laptop its frame rate.
const CAMERA_LIMIT: (u32, u32) = (1280, 720);

/// What to record, and where.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The folder the files are written into; made if missing.
    pub folder: PathBuf,
    /// The part of the screen, or all of it.
    pub area: Option<Area>,
    /// Pictures a second for the screen.
    pub fps: u32,
    /// The camera to record too, by its id.
    pub camera: Option<String>,
    /// The microphone to record too, by its name.
    pub microphone: Option<String>,
}

/// One recorded file, and when it starts against the others.
#[derive(Clone, Debug)]
pub struct Track {
    /// The file.
    pub path: PathBuf,
    /// Seconds after the earliest source began.
    pub start: f64,
    /// Its picture's size; zero for sound.
    pub width: u32,
    /// See `width`.
    pub height: u32,
}

/// A recording, stopped and on disk.
#[derive(Clone, Debug)]
pub struct Finished {
    /// The screen.
    pub screen: Track,
    /// The camera, when one was recorded and gave a picture.
    pub camera: Option<Track>,
    /// The microphone, when one was recorded and heard something.
    pub voice: Option<Track>,
    /// The frame rate the screen was written at.
    pub fps: u32,
}

/// What the threads share with the handle.
#[derive(Default)]
struct Control {
    stop: AtomicBool,
    paused: AtomicBool,
    failed: Mutex<Option<String>>,
}

impl Control {
    fn fail(&self, error: String) {
        let mut failed = self
            .failed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        failed.get_or_insert(error);
    }
}

/// The camera's latest picture, for a preview to show: written by whoever
/// reads the camera, read by the window as often as it likes.
pub type Tap = Arc<Mutex<Option<Frame>>>;

/// The camera open before a recording, for its preview. Its device is
/// handed to the recording when Record is pressed rather than closed and
/// opened again: no second of black at the start, and no camera that will
/// not open twice.
pub struct CameraPreview {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<Grabber, String>>>,
    tap: Tap,
}

impl CameraPreview {
    /// Opens the camera and starts showing it on the tap.
    pub fn open(id: &str) -> Result<CameraPreview, String> {
        let mut grabber = Grabber::camera(id)
            .map_err(|error| error.to_string())?
            .limit(CAMERA_LIMIT.0, CAMERA_LIMIT.1);
        let stop = Arc::new(AtomicBool::new(false));
        let tap: Tap = Arc::default();
        let thread = {
            let (stop, tap) = (Arc::clone(&stop), Arc::clone(&tap));
            std::thread::Builder::new()
                .name("concat camera preview".to_owned())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let frame = grabber.grab().map_err(|error| error.to_string())?;
                        *tap.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(frame);
                    }
                    Ok(grabber)
                })
                .map_err(|error| error.to_string())?
        };
        Ok(CameraPreview {
            stop,
            thread: Some(thread),
            tap,
        })
    }

    /// Where its pictures land.
    pub fn tap(&self) -> Tap {
        Arc::clone(&self.tap)
    }

    /// Stops the preview and gives the device back, with the tap the
    /// recording goes on showing on. Waits for one picture at most.
    pub fn into_device(mut self) -> Result<(Grabber, Tap), String> {
        self.stop.store(true, Ordering::Release);
        let thread = self.thread.take().expect("there until handed over");
        let grabber = thread
            .join()
            .unwrap_or_else(|_| Err("the camera preview stopped".to_owned()))?;
        Ok((grabber, Arc::clone(&self.tap)))
    }
}

impl Drop for CameraPreview {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What a video thread hands back: its file, picture size and first moment.
type Rolled = Result<Option<(PathBuf, u32, u32, Instant)>, String>;

/// A recording in progress.
pub struct Recording {
    control: Arc<Control>,
    screen: Option<JoinHandle<Rolled>>,
    camera: Option<JoinHandle<Rolled>>,
    microphone: Option<Microphone>,
    fps: u32,
    began: Instant,
    paused_at: Option<Instant>,
    paused_for: Duration,
}

impl Recording {
    /// Opens every source the plan names and starts them all. A device
    /// that will not open fails the whole start, before anything is
    /// written, so the person hears about it with the dialog still up.
    ///
    /// A camera or microphone already open for the preview is passed in
    /// and used as it is; only what is not is opened by name.
    pub fn start(
        plan: Plan,
        open_camera: Option<(Grabber, Tap)>,
        open_microphone: Option<Microphone>,
    ) -> Result<Recording, String> {
        std::fs::create_dir_all(&plan.folder)
            .map_err(|error| format!("{}: {error}", plan.folder.display()))?;
        let stamp = stamp();
        let fps = plan.fps.clamp(1, 60);
        let screen = Grabber::screen(plan.area, fps).map_err(|error| error.to_string())?;
        let camera = match (open_camera, &plan.camera) {
            (Some(open), _) => Some(open),
            (None, Some(id)) => Some((
                Grabber::camera(id)
                    .map_err(|error| error.to_string())?
                    .limit(CAMERA_LIMIT.0, CAMERA_LIMIT.1),
                Tap::default(),
            )),
            (None, None) => None,
        };
        let microphone = match (open_microphone, &plan.microphone) {
            (Some(open), _) => Some(open),
            (None, Some(name)) => Some(Microphone::open(Some(name))?),
            (None, None) => None,
        };

        let control = Arc::new(Control::default());
        let screen_path = plan.folder.join(format!("screen-{stamp}.mp4"));
        let screen = {
            let control = Arc::clone(&control);
            spawn("concat screen", move || {
                roll(screen, &screen_path, fps, &control, None)
            })?
        };
        let camera = match camera {
            Some((grabber, tap)) => {
                let control = Arc::clone(&control);
                let path = plan.folder.join(format!("camera-{stamp}.mp4"));
                Some(spawn("concat camera", move || {
                    roll(grabber, &path, 30, &control, Some(tap))
                })?)
            }
            None => None,
        };
        if let Some(mic) = &microphone {
            mic.begin(plan.folder.join(format!("voice-{stamp}.wav")))?;
        }
        Ok(Recording {
            control,
            screen: Some(screen),
            camera,
            microphone,
            fps,
            began: Instant::now(),
            paused_at: None,
            paused_for: Duration::ZERO,
        })
    }

    /// Holds every source; nothing is written until [`Recording::resume`].
    pub fn pause(&mut self) {
        if self.paused_at.is_none() {
            self.paused_at = Some(Instant::now());
            self.control.paused.store(true, Ordering::Release);
            if let Some(mic) = &self.microphone {
                mic.pause();
            }
        }
    }

    /// Carries every source on from where it paused.
    pub fn resume(&mut self) {
        if let Some(at) = self.paused_at.take() {
            self.paused_for += at.elapsed();
            self.control.paused.store(false, Ordering::Release);
            if let Some(mic) = &self.microphone {
                mic.resume();
            }
        }
    }

    /// The microphone's level since last asked, for the bar's meter; None
    /// when no microphone is recorded.
    pub fn level(&self) -> Option<crate::record::Level> {
        self.microphone.as_ref().map(Microphone::level)
    }

    /// Whether it is paused.
    pub fn paused(&self) -> bool {
        self.paused_at.is_some()
    }

    /// Seconds recorded so far, pauses left out.
    pub fn seconds(&self) -> f64 {
        let held = self.paused_for + self.paused_at.map_or(Duration::ZERO, |at| at.elapsed());
        self.began.elapsed().saturating_sub(held).as_secs_f64()
    }

    /// Why a source stopped on its own, if one did: a camera unplugged, a
    /// disk full. The recording should be stopped then.
    pub fn failed(&self) -> Option<String> {
        if let Some(error) = self
            .control
            .failed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            return Some(error);
        }
        self.microphone.as_ref().and_then(Microphone::failed)
    }

    /// Stops every source and finishes the files. Blocks while the
    /// encoders drain - a second or two - so it belongs on a worker.
    pub fn stop(mut self) -> Result<Finished, String> {
        self.resume();
        self.control.stop.store(true, Ordering::Release);
        let voice = match self.microphone.take() {
            Some(mic) => Some(mic.end()?),
            None => None,
        };
        let join = |handle: Option<JoinHandle<Rolled>>| -> Rolled {
            match handle {
                Some(handle) => handle
                    .join()
                    .unwrap_or_else(|_| Err("the recording thread stopped".to_owned())),
                None => Ok(None),
            }
        };
        let screen =
            join(self.screen.take())?.ok_or_else(|| "the screen gave no picture".to_owned())?;
        // A camera that failed loses the camera, not the recording.
        let camera = join(self.camera.take()).unwrap_or_else(|error| {
            log::warn!("screen recording: the camera failed: {error}");
            None
        });
        let voice = voice.and_then(|take| {
            let first = take.first?;
            (take.seconds > 0.0).then_some((take.path, first))
        });

        let earliest = std::iter::once(screen.3)
            .chain(camera.as_ref().map(|camera| camera.3))
            .chain(voice.as_ref().map(|voice| voice.1))
            .min()
            .expect("the screen is always there");
        let after = |moment: Instant| moment.saturating_duration_since(earliest).as_secs_f64();
        Ok(Finished {
            screen: Track {
                start: after(screen.3),
                width: screen.1,
                height: screen.2,
                path: screen.0,
            },
            camera: camera.map(|(path, width, height, first)| Track {
                path,
                start: after(first),
                width,
                height,
            }),
            voice: voice.map(|(path, first)| Track {
                path,
                start: after(first),
                width: 0,
                height: 0,
            }),
            fps: self.fps,
        })
    }
}

impl Drop for Recording {
    /// A recording dropped without a stop still finishes its files.
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Release);
        for handle in [self.screen.take(), self.camera.take()]
            .into_iter()
            .flatten()
        {
            let _ = handle.join();
        }
    }
}

fn spawn(
    name: &str,
    body: impl FnOnce() -> Rolled + Send + 'static,
) -> Result<JoinHandle<Rolled>, String> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .map_err(|error| error.to_string())
}

/// Reads `grabber` into an H.264 file at `fps` until told to stop.
fn roll(
    mut grabber: Grabber,
    path: &Path,
    fps: u32,
    control: &Control,
    tap: Option<Tap>,
) -> Rolled {
    let result = roll_inner(&mut grabber, path, fps, control, tap.as_ref());
    if let Err(error) = &result {
        control.fail(error.clone());
    }
    result
}

fn roll_inner(
    grabber: &mut Grabber,
    path: &Path,
    fps: u32,
    control: &Control,
    tap: Option<&Tap>,
) -> Rolled {
    let rate = FrameRate::new(Rational::new(i64::from(fps), 1));
    let options = EncodeOptions {
        codec: VideoCodec::H264,
        // Fast enough to keep up live on a laptop beside a second encode;
        // the hardware encoder leads where there is one.
        preset: "veryfast".to_owned(),
        crf: 20,
        rate_mode: RateMode::Vbr,
        bitrate_kbps: 0,
        ten_bit: false,
        color_range: concat_media::ColorRange::Limited,
        hardware: true,
        threads: 0,
    };
    let mut encoder: Option<Encoder> = None;
    let mut size = (0, 0);
    let mut first: Option<Instant> = None;
    let mut written: u64 = 0;
    let mut paused_since: Option<Instant> = None;
    let mut paused_for = Duration::ZERO;
    while !control.stop.load(Ordering::Acquire) {
        let frame = grabber.grab().map_err(|error| error.to_string())?;
        let now = Instant::now();
        // Paused or not, the preview keeps moving.
        if let Some(tap) = tap {
            *tap.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(frame.clone());
        }
        if control.paused.load(Ordering::Acquire) {
            paused_since.get_or_insert(now);
            continue;
        }
        if let Some(since) = paused_since.take() {
            paused_for += now - since;
        }
        let began = *first.get_or_insert(now);
        let sink = match &mut encoder {
            Some(sink) => sink,
            None => {
                size = (frame.width(), frame.height());
                encoder.insert(
                    Encoder::create(path, frame.width(), frame.height(), rate, &options)
                        .map_err(|error| error.to_string())?,
                )
            }
        };
        // The frame slot this moment falls in; the picture fills every slot
        // up to it that is still empty, and no more than a second's worth
        // at once, so a long stall is not a burst of copies.
        let active = now
            .saturating_duration_since(began)
            .saturating_sub(paused_for);
        let due = (active.as_secs_f64() * f64::from(fps)) as u64 + 1;
        let copies = due.saturating_sub(written).min(u64::from(fps));
        for _ in 0..copies {
            sink.write_frame(&frame)
                .map_err(|error| error.to_string())?;
        }
        written += copies;
    }
    match (encoder, first) {
        (Some(mut sink), Some(first)) => {
            sink.finish().map_err(|error| error.to_string())?;
            Ok(Some((path.to_path_buf(), size.0, size.1, first)))
        }
        _ => Ok(None),
    }
}

/// A sortable local-ish moment for file names: seconds since the epoch is
/// unique and needs no calendar.
fn stamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    seconds.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records two seconds of the real screen, camera and microphone; run
    /// by hand on a desk:
    /// `cargo test -p concat-host screen -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a screen, a camera and a microphone"]
    fn records_the_screen_a_camera_and_a_voice_in_step() {
        let folder = std::env::temp_dir().join("concat-screen-test");
        let _ = std::fs::remove_dir_all(&folder);
        let camera = concat_media::capture::cameras()
            .first()
            .map(|camera| camera.id.clone());
        let microphone = crate::record::microphones().first().cloned();
        let mut recording = Recording::start(
            Plan {
                folder: folder.clone(),
                area: Some(Area {
                    x: 0,
                    y: 0,
                    width: 1280,
                    height: 720,
                }),
                fps: 30,
                camera,
                microphone,
            },
            None,
            None,
        )
        .expect("starts");
        std::thread::sleep(Duration::from_millis(1200));
        recording.pause();
        std::thread::sleep(Duration::from_millis(500));
        recording.resume();
        std::thread::sleep(Duration::from_millis(800));
        let finished = recording.stop().expect("stops");
        println!("{finished:#?}");
        let screen = concat_media::probe(&finished.screen.path).expect("the screen file reads");
        let seconds = screen.duration.map_or(0.0, |d| d.as_f64());
        println!("screen lasts {seconds:.2}s");
        assert_eq!((finished.screen.width, finished.screen.height), (1280, 720));
        assert!(
            (1.5..=2.6).contains(&seconds),
            "two seconds less the pause, not {seconds}"
        );
    }
}
