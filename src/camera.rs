//! Frame sources: an Ethernet/RTSP camera on site, USB for bench work.
//!
//! RTSP goes through a long-lived `ffmpeg` process that decodes straight to
//! raw RGB at a low frame rate. A Pi 4 handles that comfortably, and the
//! decode never blocks the UI because a worker thread owns the pipe.

use crate::config::CameraCfg;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::Camera;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

pub enum Cam {
    Usb(Box<Camera>),
    Rtsp(RtspCam),
}

impl Cam {
    pub fn open(cfg: &CameraCfg) -> Result<Self, String> {
        if cfg.is_rtsp() {
            let url = cfg.rtsp_url.trim();
            if url.is_empty() {
                return Err("camera source is rtsp but rtsp_url is empty".into());
            }
            RtspCam::open(url, cfg.width, cfg.height, cfg.fps).map(Cam::Rtsp)
        } else {
            Self::open_usb(cfg).map_err(|e| e.to_string())
        }
    }

    fn open_usb(cfg: &CameraCfg) -> Result<Self, nokhwa::NokhwaError> {
        // Asking for the absolute highest resolution overloads the Pi.
        let want = CameraFormat::new(
            Resolution::new(cfg.width.max(160), cfg.height.max(120)),
            FrameFormat::MJPEG,
            15,
        );
        let req = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(want));
        let mut cam = Camera::new(CameraIndex::Index(cfg.device), req)?;
        cam.open_stream()?;
        let fmt = cam.camera_format();
        eprintln!(
            "camera: usb {}x{} @ {} fps ({:?})",
            fmt.resolution().width(),
            fmt.resolution().height(),
            fmt.frame_rate(),
            fmt.format()
        );
        Ok(Cam::Usb(Box::new(cam)))
    }

    pub fn frame(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        match self {
            Cam::Usb(cam) => {
                let f = cam.frame().ok()?;
                let rgb = f.decode_image::<RgbFormat>().ok()?;
                Some((rgb.width(), rgb.height(), rgb.into_raw()))
            }
            Cam::Rtsp(cam) => cam.frame(),
        }
    }
}

pub struct RtspCam {
    width: u32,
    height: u32,
    latest: Arc<Mutex<Option<Vec<u8>>>>,
    alive: Arc<AtomicBool>,
    child: Arc<Mutex<Child>>,
}

impl RtspCam {
    fn open(url: &str, width: u32, height: u32, fps: u32) -> Result<Self, String> {
        let width = width.max(160);
        let height = height.max(120);
        let fps = fps.clamp(1, 15);

        let mut child = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "error",
                // TCP survives lossy links far better than the UDP default.
                "-rtsp_transport",
                "tcp",
                "-rtsp_flags",
                "prefer_tcp",
                // Drop stale frames instead of building latency.
                "-fflags",
                "nobuffer",
                "-flags",
                "low_delay",
                "-i",
                url,
                "-an",
                "-vf",
                &format!("fps={fps},scale={width}:{height}"),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    "ffmpeg not found — install it: sudo apt install -y ffmpeg".to_string()
                } else {
                    format!("start ffmpeg: {e}")
                }
            })?;

        let mut stdout = child.stdout.take().ok_or("ffmpeg stdout unavailable")?;
        let latest: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let alive = Arc::new(AtomicBool::new(true));

        let sink = latest.clone();
        let running = alive.clone();
        let frame_bytes = (width as usize) * (height as usize) * 3;
        thread::spawn(move || {
            let mut buf = vec![0u8; frame_bytes];
            loop {
                match stdout.read_exact(&mut buf) {
                    Ok(()) => {
                        if let Ok(mut slot) = sink.lock() {
                            *slot = Some(buf.clone());
                        }
                    }
                    Err(_) => {
                        // Stream ended: the supervisor in main reopens us.
                        running.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }
        });

        eprintln!("camera: rtsp {width}x{height} @ {fps} fps");
        Ok(Self {
            width,
            height,
            latest,
            alive,
            child: Arc::new(Mutex::new(child)),
        })
    }

    fn frame(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        if !self.alive.load(Ordering::SeqCst) {
            return None;
        }
        let rgb = self.latest.lock().ok()?.clone()?;
        Some((self.width, self.height, rgb))
    }
}

impl Drop for RtspCam {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
