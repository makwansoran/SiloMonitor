use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::Camera;

pub struct Cam {
    cam: Camera,
}

impl Cam {
    pub fn open() -> Result<Self, nokhwa::NokhwaError> {
        // BRIO can do 1080p; AbsoluteHighest* overloads the Pi and the app dies.
        let want = CameraFormat::new(
            Resolution::new(640, 480),
            FrameFormat::MJPEG,
            15,
        );
        let req = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(want));
        let mut cam = Camera::new(CameraIndex::Index(0), req)?;
        cam.open_stream()?;
        let fmt = cam.camera_format();
        eprintln!(
            "camera: {}x{} @ {} fps ({:?})",
            fmt.resolution().width(),
            fmt.resolution().height(),
            fmt.frame_rate(),
            fmt.format()
        );
        Ok(Self { cam })
    }

    pub fn frame(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        let f = self.cam.frame().ok()?;
        let rgb = f.decode_image::<RgbFormat>().ok()?;
        Some((rgb.width(), rgb.height(), rgb.into_raw()))
    }
}
