// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! Live pictures from the machine itself: the screen and the cameras, read
//! through libavdevice the way a file is read through libavformat.
//!
//! Each platform names its devices its own way - gdigrab and DirectShow on
//! Windows, AVFoundation on macOS, X11 and Video4Linux on Linux - and this
//! module is the one place that knows which. What comes out is the same
//! everywhere: RGBA [`Frame`]s, one per call to [`Grabber::grab`], as fast
//! as the device hands them over.
//!
//! The screen is always grabbed whole and an [`Area`] is cut out of it
//! here, after the picture is RGBA. That costs a little against asking the
//! device for the area alone, and buys one meaning for an area's numbers on
//! every platform: pixels of the picture [`snapshot`] returns, top left at
//! zero. The area picker draws on that picture, so what was framed there is
//! exactly what is recorded.

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;

use concat_core::frame::Frame;
use ffmpeg_the_third as ffmpeg;
use ffmpeg_the_third::format::Pixel;
use ffmpeg_the_third::software::scaling;
use ffmpeg_the_third::util::frame::video::Video;

use crate::error::{Error, Result};
use crate::ffi;

/// A rectangle of the screen, in the pixels of [`snapshot`]'s picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Area {
    /// The area clamped inside a `width` by `height` picture and shrunk to
    /// even sides, which 4:2:0 video needs. None when nothing is left.
    pub fn within(self, width: u32, height: u32) -> Option<Area> {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let w = self.width.min(width - x) & !1;
        let h = self.height.min(height - y) & !1;
        (w >= 2 && h >= 2).then_some(Area {
            x,
            y,
            width: w,
            height: h,
        })
    }
}

/// A camera the system offers: the name to open it by, and the one to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Camera {
    /// What the device is opened by: DirectShow's unique name, a
    /// `/dev/video*` path, an AVFoundation index.
    pub id: String,
    /// What a person is shown.
    pub label: String,
}

/// The device family for the screen and for cameras on this platform.
#[cfg(target_os = "windows")]
const SCREEN_FORMAT: &str = "gdigrab";
#[cfg(target_os = "windows")]
const CAMERA_FORMAT: &str = "dshow";
#[cfg(target_os = "macos")]
const SCREEN_FORMAT: &str = "avfoundation";
#[cfg(target_os = "macos")]
const CAMERA_FORMAT: &str = "avfoundation";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const SCREEN_FORMAT: &str = "x11grab";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const CAMERA_FORMAT: &str = "v4l2";

/// The cameras the system lists, by the platform's device listing. Empty
/// when there are none or the platform will not list them.
pub fn cameras() -> Vec<Camera> {
    ffi::init();
    let mut found = list_sources(CAMERA_FORMAT);
    // AVFoundation lists no sources through libavdevice; its cameras open
    // by index, and the first is the built-in one on every Mac with one.
    if found.is_empty() && cfg!(target_os = "macos") {
        found.push(Camera {
            id: "0".to_owned(),
            label: "Camera".to_owned(),
        });
    }
    found
}

/// libavdevice's listing of `format`'s video sources.
fn list_sources(format: &str) -> Vec<Camera> {
    let Ok(name) = CString::new(format) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    // SAFETY: the format pointer is FFmpeg's static table entry or null;
    // the list is FFmpeg's to allocate and is freed with its own call, and
    // every pointer read out of it is checked for null first.
    unsafe {
        let device = ffmpeg::sys::av_find_input_format(name.as_ptr());
        if device.is_null() {
            return found;
        }
        let mut list: *mut ffmpeg::sys::AVDeviceInfoList = ptr::null_mut();
        if ffmpeg::sys::avdevice_list_input_sources(device, ptr::null(), ptr::null_mut(), &mut list)
            < 0
            || list.is_null()
        {
            if !list.is_null() {
                ffmpeg::sys::avdevice_free_list_devices(&mut list);
            }
            return found;
        }
        let devices = (*list).devices;
        for index in 0..(*list).nb_devices.max(0) as usize {
            let info = *devices.add(index);
            if info.is_null() || (*info).device_name.is_null() {
                continue;
            }
            let video = (*info).media_types.is_null()
                || (0..(*info).nb_media_types.max(0) as usize)
                    .any(|kind| *(*info).media_types.add(kind) == ffmpeg::sys::AVMediaType::VIDEO);
            if !video {
                continue;
            }
            let id = CStr::from_ptr((*info).device_name)
                .to_string_lossy()
                .into_owned();
            let label = if (*info).device_description.is_null() {
                id.clone()
            } else {
                CStr::from_ptr((*info).device_description)
                    .to_string_lossy()
                    .into_owned()
            };
            found.push(Camera { id, label });
        }
        ffmpeg::sys::avdevice_free_list_devices(&mut list);
    }
    found
}

/// One picture of the whole screen: what the area picker draws on.
pub fn snapshot() -> Result<Frame> {
    let mut grabber = Grabber::screen(None, 30)?;
    grabber.grab()
}

/// A live device, read a picture at a time.
pub struct Grabber {
    input: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    stream: usize,
    scaler: Option<((Pixel, u32, u32), scaling::Context)>,
    /// The part of each picture kept, or all of it (to even sides).
    area: Option<Area>,
    /// The largest picture handed out; a bigger one is scaled down to fit.
    limit: Option<(u32, u32)>,
    /// What the device is called, for errors.
    label: PathBuf,
}

// SAFETY: a Grabber is moved to the thread that records with it and used
// there alone; FFmpeg's contexts are safe to hand between threads so long
// as two never touch one at once, which ownership guarantees.
unsafe impl Send for Grabber {}

impl Grabber {
    /// The screen, at `fps` pictures a second, cut to `area` when given.
    pub fn screen(area: Option<Area>, fps: u32) -> Result<Self> {
        let fps = fps.clamp(1, 120).to_string();
        #[cfg(target_os = "windows")]
        let (url, options) = (
            "desktop".to_owned(),
            vec![("framerate", fps), ("draw_mouse", "1".to_owned())],
        );
        #[cfg(target_os = "macos")]
        let (url, options) = (
            "Capture screen 0".to_owned(),
            vec![
                ("framerate", fps),
                ("capture_cursor", "1".to_owned()),
                ("pixel_format", "bgr0".to_owned()),
            ],
        );
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let (url, options) = (
            std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_owned()),
            vec![("framerate", fps), ("draw_mouse", "1".to_owned())],
        );
        let mut grabber = Self::open(SCREEN_FORMAT, &url, &options)?;
        grabber.area = area;
        Ok(grabber)
    }

    /// A camera, by the id [`cameras`] gave it.
    pub fn camera(id: &str) -> Result<Self> {
        #[cfg(target_os = "windows")]
        let (url, options) = (
            format!("video={id}"),
            // DirectShow drops pictures once its buffer fills, and a camera
            // at 1080p fills the default 3 MB in a few frames.
            vec![("rtbufsize", "256M".to_owned())],
        );
        #[cfg(target_os = "macos")]
        let (url, options) = (id.to_owned(), vec![("framerate", "30".to_owned())]);
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let (url, options) = (id.to_owned(), Vec::new());
        Self::open(CAMERA_FORMAT, &url, &options)
    }

    fn open(format: &str, url: &str, options: &[(&str, String)]) -> Result<Self> {
        ffi::init();
        let label = PathBuf::from(format!("{format}:{url}"));
        let failed =
            |operation: &'static str, error: ffmpeg::Error| ffi::fail(operation, &label, error);
        let format_name =
            CString::new(format).map_err(|_| unusable(&label, "bad device format"))?;
        let url_c = CString::new(url).map_err(|_| unusable(&label, "bad device name"))?;
        // SAFETY: plain FFmpeg calls in the order its documentation gives:
        // the dictionary is freed whatever happens, the context is closed
        // on every failure after it opened, and is handed to the safe
        // wrapper only once it is whole.
        let input = unsafe {
            let device = ffmpeg::sys::av_find_input_format(format_name.as_ptr());
            if device.is_null() {
                return Err(unusable(&label, "this FFmpeg was built without the device"));
            }
            let mut dictionary: *mut ffmpeg::sys::AVDictionary = ptr::null_mut();
            for (key, value) in options {
                let key = CString::new(*key).expect("option names are plain");
                let value = CString::new(value.as_str()).expect("option values are plain");
                ffmpeg::sys::av_dict_set(&mut dictionary, key.as_ptr(), value.as_ptr(), 0);
            }
            let mut context = ptr::null_mut();
            let opened = ffmpeg::sys::avformat_open_input(
                &mut context,
                url_c.as_ptr(),
                device,
                &mut dictionary,
            );
            ffmpeg::sys::av_dict_free(&mut dictionary);
            if opened < 0 {
                return Err(failed("open device", ffmpeg::Error::from(opened)));
            }
            let info = ffmpeg::sys::avformat_find_stream_info(context, ptr::null_mut());
            if info < 0 {
                ffmpeg::sys::avformat_close_input(&mut context);
                return Err(failed("read device", ffmpeg::Error::from(info)));
            }
            ffmpeg::format::context::Input::wrap(context)
        };
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or_else(|| Error::NoVideoStream {
                path: label.clone(),
            })?;
        let index = stream.index();
        let decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .and_then(|context| context.decoder().video())
            .map_err(|error| failed("open decoder", error))?;
        Ok(Self {
            input,
            decoder,
            stream: index,
            scaler: None,
            area: None,
            limit: None,
            label,
        })
    }

    /// Hands out pictures no bigger than `width` by `height`, scaled down
    /// with their shape kept: a camera shown in a corner needs no 1080p.
    pub fn limit(mut self, width: u32, height: u32) -> Self {
        self.limit = Some((width.max(2), height.max(2)));
        self
    }

    /// The size a `width` by `height` picture is scaled to: within the
    /// limit, shape kept, even-sided.
    fn scaled(&self, width: u32, height: u32) -> (u32, u32) {
        match self.limit {
            Some((most_w, most_h)) if width > most_w || height > most_h => {
                let factor = (f64::from(most_w) / f64::from(width))
                    .min(f64::from(most_h) / f64::from(height));
                let even = |side: u32| ((f64::from(side) * factor) as u32 & !1).max(2);
                (even(width), even(height))
            }
            _ => (width, height),
        }
    }

    /// The next picture from the device, waiting for it.
    pub fn grab(&mut self) -> Result<Frame> {
        let mut decoded = Video::empty();
        loop {
            if self.decoder.receive_frame(&mut decoded).is_ok() {
                return self.convert(&decoded);
            }
            let mut packet = ffmpeg::Packet::empty();
            match packet.read(&mut self.input) {
                Ok(()) => {}
                // A device with nothing yet says so; ask again.
                Err(error) if ffi::is_again(&error) => continue,
                Err(error) => return Err(ffi::fail("read device", &self.label, error)),
            }
            if packet.stream() != self.stream {
                continue;
            }
            if let Err(error) = self.decoder.send_packet(&packet)
                && !ffi::is_again(&error)
            {
                return Err(ffi::fail("decode device", &self.label, error));
            }
        }
    }

    /// One decoded picture as RGBA, cut to the area, even-sided.
    fn convert(&mut self, source: &Video) -> Result<Frame> {
        let key = (source.format(), source.width(), source.height());
        if self.scaler.as_ref().is_none_or(|(have, _)| *have != key) {
            let (width, height) = self.scaled(key.1, key.2);
            let flags = if (width, height) == (key.1, key.2) {
                scaling::Flags::POINT
            } else {
                scaling::Flags::BILINEAR
            };
            let context =
                scaling::Context::get(key.0, key.1, key.2, Pixel::RGBA, width, height, flags)
                    .map_err(|error| ffi::fail("convert device picture", &self.label, error))?;
            self.scaler = Some((key, context));
        }
        let (_, scaler) = self.scaler.as_mut().expect("just made");
        let mut rgba = Video::empty();
        scaler
            .run(source, &mut rgba)
            .map_err(|error| ffi::fail("convert device picture", &self.label, error))?;
        let (width, height) = (rgba.width(), rgba.height());
        let whole = Area {
            x: 0,
            y: 0,
            width,
            height,
        };
        let area = self
            .area
            .unwrap_or(whole)
            .within(width, height)
            .ok_or_else(|| unusable(&self.label, "the area is outside the screen"))?;
        let stride = rgba.stride(0);
        let data = rgba.data(0);
        let row = area.width as usize * 4;
        let mut pixels = Vec::with_capacity(row * area.height as usize);
        for y in area.y as usize..(area.y + area.height) as usize {
            let start = y * stride + area.x as usize * 4;
            pixels.extend_from_slice(&data[start..start + row]);
        }
        // Screens hand over BGRA/BGR0 with the fourth byte unset; the
        // picture is opaque whatever it says.
        for alpha in pixels.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }
        Frame::from_rgba(area.width, area.height, pixels)
            .ok_or_else(|| unusable(&self.label, "the picture came out the wrong size"))
    }
}

fn unusable(label: &Path, detail: &str) -> Error {
    Error::Probe {
        path: label.to_path_buf(),
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_area_is_kept_inside_the_picture_with_even_sides() {
        let area = Area {
            x: 100,
            y: 50,
            width: 301,
            height: 2000,
        };
        assert_eq!(
            area.within(1920, 1080),
            Some(Area {
                x: 100,
                y: 50,
                width: 300,
                height: 1030
            })
        );
    }

    /// Reads the real screen and the first camera; run by hand on a desk:
    /// `cargo test -p concat-media capture -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a screen and a camera"]
    fn grabs_the_screen_and_a_camera() {
        let screen = snapshot().expect("the screen");
        println!("screen {}x{}", screen.width(), screen.height());
        let area = Area {
            x: 10,
            y: 10,
            width: 641,
            height: 361,
        };
        let mut grabber = Grabber::screen(Some(area), 30).expect("opens");
        let cut = grabber.grab().expect("a picture");
        assert_eq!((cut.width(), cut.height()), (640, 360));
        let found = cameras();
        println!("cameras {found:?}");
        if let Some(camera) = found.first() {
            let mut grabber = Grabber::camera(&camera.id).expect("the camera opens");
            let picture = grabber.grab().expect("a picture");
            println!("camera {}x{}", picture.width(), picture.height());
        }
    }

    #[test]
    fn an_area_off_the_picture_is_nothing() {
        let area = Area {
            x: 4000,
            y: 0,
            width: 100,
            height: 100,
        };
        assert_eq!(area.within(1920, 1080), None);
    }
}
