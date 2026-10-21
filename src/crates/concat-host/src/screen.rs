// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! Recording the screen: the picture, and optionally a camera, a
//! microphone and the computer's own sound beside it, each into a file of
//! its own.
//!
//! Separate files rather than one so the editor gets separate clips: the
//! screen on one lane, the camera over it in a corner where it can still be
//! moved, cut or dropped, and the voice and the system's sound under both. Each source runs on its own
//! thread from the moment Record is pressed and notes when its first
//! picture or sample arrived; [`Finished`] gives every file its start
//! relative to the earliest, so the clips land in step.
//!
//! A video file keeps the clock's time however unevenly the device
//! delivers: each picture goes in the frame slot of the moment it arrived,
//! encoded once, held over the slots it missed, and one early waits for
//! its slot. Pause stops every source at
//! once and cuts the paused stretch out of every file alike. The sound is
//! held to the same clock (see [`crate::record`]), so a long recording
//! stays in step to the end.
//!
//! Every file is good on disk as it goes. The pictures are fragmented MP4
//! ([`Encoder::create_live`]), a fragment every two seconds; the sound is a
//! WAV whose header is brought up to date every second. A recording cut
//! short by a crash or a power cut plays up to the last fragment.

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
    /// Record what the computer plays too: Windows only, see
    /// [`crate::record::system_audio_available`].
    pub system_audio: bool,
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
    /// For sound, the samples added (more than zero) or dropped (less) to
    /// keep it in step with the pictures; zero for pictures.
    pub corrected: i64,
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
    /// What the computer played, when it was recorded.
    pub system: Option<Track>,
    /// The frame rate the screen was written at.
    pub fps: u32,
}

/// What the threads share with the handle.
#[derive(Default)]
struct Control {
    stop: AtomicBool,
    /// The moment Stop was pressed: every file runs to it.
    stopped_at: Mutex<Option<Instant>>,
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
    system: Option<Microphone>,
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
    /// A camera, microphone or system sound already open for the preview
    /// is passed in and used as it is; only what is not is opened by name.
    pub fn start(
        plan: Plan,
        open_camera: Option<(Grabber, Tap)>,
        open_microphone: Option<Microphone>,
        open_system: Option<Microphone>,
    ) -> Result<Recording, String> {
        std::fs::create_dir_all(&plan.folder)
            .map_err(|error| format!("{}: {error}", plan.folder.display()))?;
        let stamp = stamp();
        let fps = plan.fps.clamp(1, 60);
        let screen = Grabber::screen(plan.area, fps).map_err(|error| error.to_string())?;
        log::info!(
            "screen recording: reading the screen through {}",
            screen.kind()
        );
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
        let system = match (open_system, plan.system_audio) {
            (Some(open), _) => Some(open),
            (None, true) => Some(Microphone::open_system()?),
            (None, false) => None,
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
        if let Some(system) = &system {
            system.begin(plan.folder.join(format!("system-{stamp}.wav")))?;
        }
        Ok(Recording {
            control,
            screen: Some(screen),
            camera,
            microphone,
            system,
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
            for sound in self.sounds() {
                sound.pause();
            }
        }
    }

    /// Carries every source on from where it paused.
    pub fn resume(&mut self) {
        if let Some(at) = self.paused_at.take() {
            self.paused_for += at.elapsed();
            self.control.paused.store(false, Ordering::Release);
            for sound in self.sounds() {
                sound.resume();
            }
        }
    }

    /// The microphone and the system sound, whichever are recorded.
    fn sounds(&self) -> impl Iterator<Item = &Microphone> {
        self.microphone.iter().chain(self.system.iter())
    }

    /// The microphone's level since last asked, for the bar's meter; None
    /// when no microphone is recorded.
    pub fn level(&self) -> Option<crate::record::Level> {
        self.microphone.as_ref().map(Microphone::level)
    }

    /// The system sound's level since last asked; None when it is not
    /// recorded.
    pub fn system_level(&self) -> Option<crate::record::Level> {
        self.system.as_ref().map(Microphone::level)
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
        self.sounds().find_map(Microphone::failed)
    }

    /// Stops every source and finishes the files. Blocks while the
    /// encoders drain - a second or two - so it belongs on a worker.
    pub fn stop(mut self) -> Result<Finished, String> {
        self.resume();
        // One moment for every file to end at: the sound is cut there, by
        // when it was heard, and the pictures held up to it.
        let at = Instant::now();
        for sound in self.sounds() {
            sound.cut_at(at);
        }
        *self
            .control
            .stopped_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(at);
        self.control.stop.store(true, Ordering::Release);
        let voice = match self.microphone.take() {
            Some(mic) => Some(mic.end()?),
            None => None,
        };
        // System sound that failed loses that track, not the recording.
        let system = self.system.take().and_then(|system| {
            system
                .end()
                .inspect_err(|error| log::warn!("screen recording: system sound: {error}"))
                .ok()
        });
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
        let heard = |take: crate::record::Take| {
            let first = take.first?;
            (take.seconds > 0.0).then_some((take.path, first, take.corrected))
        };
        let voice = voice.and_then(heard);
        let system = system.and_then(heard);

        let earliest = std::iter::once(screen.3)
            .chain(camera.as_ref().map(|camera| camera.3))
            .chain(voice.as_ref().map(|voice| voice.1))
            .chain(system.as_ref().map(|system| system.1))
            .min()
            .expect("the screen is always there");
        let after = |moment: Instant| moment.saturating_duration_since(earliest).as_secs_f64();
        let sound = |(path, first, corrected): (PathBuf, Instant, i64)| Track {
            path,
            start: after(first),
            width: 0,
            height: 0,
            corrected,
        };
        Ok(Finished {
            screen: Track {
                start: after(screen.3),
                width: screen.1,
                height: screen.2,
                path: screen.0,
                corrected: 0,
            },
            camera: camera.map(|(path, width, height, first)| Track {
                path,
                start: after(first),
                width,
                height,
                corrected: 0,
            }),
            voice: voice.map(sound),
            system: system.map(sound),
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

/// Where a video thread's pictures come from: a device, or in the tests a
/// picture made up on the spot.
trait Source: Send {
    /// The next picture, waiting for it; None while there is none to be
    /// had (the screen lost for a moment), when the last is held.
    fn grab(&mut self) -> Result<Option<Frame>, String>;
}

impl Source for Grabber {
    fn grab(&mut self) -> Result<Option<Frame>, String> {
        self.grab_live().map_err(|error| error.to_string())
    }
}

/// Reads `grabber` into an H.264 file at `fps` until told to stop.
fn roll(
    mut grabber: impl Source,
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
    grabber: &mut impl Source,
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
    // The last picture written, to hold to the end.
    let mut last: Option<Frame> = None;
    while !control.stop.load(Ordering::Acquire) {
        // Nothing to be had: the slots go by holding the last picture.
        let Some(frame) = grabber.grab()? else {
            continue;
        };
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
                    Encoder::create_live(path, frame.width(), frame.height(), rate, &options)
                        .map_err(|error| error.to_string())?,
                )
            }
        };
        // The frame slot this moment falls in. The picture goes there,
        // encoded once; the slots since the last picture hold that one. A
        // picture early for its slot waits for the next. However slowly
        // the pictures come or the encoder goes, the file keeps the
        // clock's time - it never falls behind the sound.
        let active = now
            .saturating_duration_since(began)
            .saturating_sub(paused_for);
        let slot = (active.as_secs_f64() * f64::from(fps)) as u64;
        if slot < written {
            continue;
        }
        sink.write_frame_at(&frame, slot)
            .map_err(|error| error.to_string())?;
        written = slot + 1;
        last = Some(frame);
    }
    match (encoder, first) {
        (Some(mut sink), Some(first)) => {
            // Held up to Stop: the last picture may have come a while
            // before it, and the sound runs to it.
            let stopped_at = *control
                .stopped_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let (Some(at), Some(frame)) = (stopped_at, &last) {
                let held = paused_since
                    .map_or(Duration::ZERO, |since| at.saturating_duration_since(since));
                let active = at
                    .saturating_duration_since(first)
                    .saturating_sub(paused_for + held);
                let slot = (active.as_secs_f64() * f64::from(fps)) as u64;
                if slot > written {
                    // The slot before the end, so the file lasts to it.
                    sink.write_frame_at(frame, slot - 1)
                        .map_err(|error| error.to_string())?;
                }
            }
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
                system_audio: crate::record::system_audio_available(),
            },
            None,
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

    /// Records the real screen, camera, microphone and system sound for
    /// `CONCAT_RECORD_MINUTES` (twenty unless set) and says how far apart
    /// the files end and how much each sound was pulled into step; run by
    /// hand: `cargo test -p concat-host screen::tests::a_long -- --ignored
    /// --nocapture`. With `CONCAT_BURN` set, every core is kept busy for
    /// twenty seconds in every forty, to see it hold under load.
    #[test]
    #[ignore = "needs a screen, a camera and a microphone, and takes twenty minutes"]
    fn a_long_recording_stays_in_step() {
        let minutes: f64 = std::env::var("CONCAT_RECORD_MINUTES")
            .ok()
            .and_then(|minutes| minutes.parse().ok())
            .unwrap_or(20.0);
        let folder = std::env::temp_dir().join("concat-screen-long");
        let _ = std::fs::remove_dir_all(&folder);
        let recording = Recording::start(
            Plan {
                folder: folder.clone(),
                area: None,
                fps: 30,
                camera: concat_media::capture::cameras()
                    .first()
                    .map(|camera| camera.id.clone()),
                microphone: crate::record::microphones().first().cloned(),
                system_audio: crate::record::system_audio_available(),
            },
            None,
            None,
            None,
        )
        .expect("starts");
        let until = Instant::now() + Duration::from_secs_f64(minutes * 60.0);
        while Instant::now() < until {
            std::thread::sleep(Duration::from_secs(20));
            if std::env::var("CONCAT_BURN").is_ok() {
                let stop = Arc::new(AtomicBool::new(false));
                let burners: Vec<_> = (0..16)
                    .map(|_| {
                        let stop = Arc::clone(&stop);
                        std::thread::spawn(move || {
                            let mut x = 1u64;
                            while !stop.load(Ordering::Relaxed) {
                                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                            }
                            x
                        })
                    })
                    .collect();
                std::thread::sleep(Duration::from_secs(20));
                stop.store(true, Ordering::Relaxed);
                for burner in burners {
                    let _ = burner.join();
                }
            }
        }
        let finished = recording.stop().expect("stops");
        let ends: Vec<(&str, f64, i64)> = [
            ("screen", Some(&finished.screen)),
            ("camera", finished.camera.as_ref()),
            ("voice", finished.voice.as_ref()),
            ("system", finished.system.as_ref()),
        ]
        .into_iter()
        .filter_map(|(name, track)| {
            let track = track?;
            let seconds = concat_media::probe(&track.path)
                .expect("reads")
                .duration
                .map_or(0.0, |d| d.as_f64());
            println!(
                "{name}: starts {:.3}s, lasts {seconds:.3}s, ends {:.3}s, corrected {} samples",
                track.start,
                track.start + seconds,
                track.corrected
            );
            Some((name, track.start + seconds, track.corrected))
        })
        .collect();
        let latest = ends.iter().map(|end| end.1).fold(f64::MIN, f64::max);
        let earliest = ends.iter().map(|end| end.1).fold(f64::MAX, f64::min);
        println!("the files end {:.1} ms apart", (latest - earliest) * 1000.0);
        // The pictures stop at the next frame after Stop, the sound at
        // once: apart by under two frames, whatever the length.
        assert!(latest - earliest < 2.0 / 30.0 + 0.02);
    }

    /// Records 1920 x 1080 of the real screen and nothing else for
    /// `CONCAT_RECORD_SECONDS` (twenty unless set) at `CONCAT_RECORD_FPS`
    /// (thirty unless set), for timing the process's CPU from outside; run
    /// by hand with `CONCAT_SCREEN_GRABBER=gdigrab` and without to compare.
    #[test]
    #[ignore = "needs a screen at least 1920 x 1080; for measuring by hand"]
    fn records_1080p_for_a_while() {
        let number = |name: &str, default: u32| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        let folder = std::env::temp_dir().join("concat-screen-cost");
        let _ = std::fs::remove_dir_all(&folder);
        let recording = Recording::start(
            Plan {
                folder,
                area: Some(Area {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                }),
                fps: number("CONCAT_RECORD_FPS", 30),
                camera: None,
                microphone: None,
                system_audio: false,
            },
            None,
            None,
            None,
        )
        .expect("starts");
        std::thread::sleep(Duration::from_secs(u64::from(number(
            "CONCAT_RECORD_SECONDS",
            20,
        ))));
        let finished = recording.stop().expect("stops");
        let seconds = concat_media::probe(&finished.screen.path)
            .expect("reads")
            .duration
            .map_or(0.0, |d| d.as_f64());
        println!("recorded {seconds:.2}s");
    }

    /// Set in the child [`a_recording_cut_short_by_a_crash_still_plays`]
    /// starts: the file it records into until it is killed.
    const CRASH_CHILD: &str = "CONCAT_SCREEN_CRASH_CHILD";

    /// A made-up picture at a steady rate: a bar sweeping across grey.
    struct Painted {
        width: u32,
        height: u32,
        fps: u32,
        next: Instant,
        index: u32,
        /// Pictures, by number, that are not to be had: a screen lost.
        dark: std::ops::Range<u32>,
    }

    impl Source for Painted {
        fn grab(&mut self) -> Result<Option<Frame>, String> {
            std::thread::sleep(self.next.saturating_duration_since(Instant::now()));
            self.next += Duration::from_secs_f64(1.0 / f64::from(self.fps));
            if self.dark.contains(&self.index) {
                self.index += 1;
                return Ok(None);
            }
            let bar = self.index * 8 % self.width;
            self.index += 1;
            let mut pixels = Vec::with_capacity((self.width * self.height * 4) as usize);
            for _ in 0..self.height {
                for x in 0..self.width {
                    let shade = if x.abs_diff(bar) < 8 { 240 } else { 64 };
                    pixels.extend_from_slice(&[shade, shade, shade, 255]);
                }
            }
            Frame::from_rgba(self.width, self.height, pixels)
                .map(Some)
                .ok_or_else(|| "size".to_owned())
        }
    }

    /// A screen lost for a second and a half mid-way - a UAC prompt, the
    /// lock screen - holds its last picture: the file still lasts as long
    /// as the recording did, and the recording does not stop.
    #[test]
    fn a_screen_lost_for_a_while_holds_its_last_picture() {
        let path = std::env::temp_dir().join(format!("concat-lost-{}.mp4", std::process::id()));
        let control = Arc::new(Control::default());
        let thread = {
            let (control, path) = (Arc::clone(&control), path.clone());
            std::thread::spawn(move || {
                let source = Painted {
                    width: 320,
                    height: 180,
                    fps: 30,
                    next: Instant::now(),
                    index: 0,
                    dark: 30..75,
                };
                roll(source, &path, 30, &control, None)
            })
        };
        std::thread::sleep(Duration::from_secs(4));
        control.stop.store(true, Ordering::Release);
        let rolled = thread.join().expect("joins").expect("rolls");
        assert!(rolled.is_some());
        assert!(control.failed.lock().expect("lock").is_none());
        let summary = concat_media::probe(&path).expect("reads");
        let _ = std::fs::remove_file(&path);
        let seconds = summary.duration.map_or(0.0, |d| d.as_f64());
        assert!((3.8..=4.2).contains(&seconds), "{seconds}s");
    }

    /// Records a made-up picture in a child process, kills it mid-way - as
    /// a crash or a pulled plug would - and reads what it left: a file
    /// that plays up to its last fragment, not one nothing opens.
    #[test]
    fn a_recording_cut_short_by_a_crash_still_plays() {
        if let Ok(path) = std::env::var(CRASH_CHILD) {
            let control = Control::default();
            let source = Painted {
                width: 320,
                height: 180,
                fps: 30,
                next: Instant::now(),
                index: 0,
                dark: 0..0,
            };
            // Runs until the parent kills the process.
            let _ = roll(source, Path::new(&path), 30, &control, None);
            return;
        }
        let path = std::env::temp_dir().join(format!("concat-crash-{}.mp4", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut child = std::process::Command::new(std::env::current_exe().expect("the test"))
            .args([
                "--exact",
                "screen::tests::a_recording_cut_short_by_a_crash_still_plays",
                "--test-threads=1",
            ])
            .env(CRASH_CHILD, &path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the child starts");
        // Wait for the file, then let it record a little over three
        // fragments' worth before pulling the plug.
        let waited = Instant::now();
        while !path.exists() && waited.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(7000));
        child.kill().expect("killed");
        let _ = child.wait();
        let summary = concat_media::probe(&path).expect("the file cut short still reads");
        let seconds = summary.duration.map_or(0.0, |d| d.as_f64());
        let _ = std::fs::remove_file(&path);
        println!("cut short after {seconds:.2}s");
        assert!(
            (3.9..=7.5).contains(&seconds),
            "at least two whole fragments, not {seconds}s"
        );
    }
}
