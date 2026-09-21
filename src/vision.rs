use image::{imageops::FilterType, ImageBuffer, Rgb, RgbImage};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SIZE: u32 = 32;
pub const EMPTY_DIR: &str = "data/empty";
/// The black-and-white version of each reference photo.
pub const BW_DIR: &str = "data/empty_bw";
pub const REGION_PATH: &str = "data/region.json";
pub const REFERENCE_PATH: &str = "data/reference.json";

/// A marked region, stored as fractions of the frame so it survives a
/// camera resolution change.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoxNorm {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl BoxNorm {
    pub fn from_corners(ax: f32, ay: f32, bx: f32, by: f32) -> Option<Self> {
        let x1 = ax.min(bx).clamp(0.0, 1.0);
        let y1 = ay.min(by).clamp(0.0, 1.0);
        let x2 = ax.max(bx).clamp(0.0, 1.0);
        let y2 = ay.max(by).clamp(0.0, 1.0);
        // Ignore stray clicks.
        if x2 - x1 < 0.02 || y2 - y1 < 0.02 {
            return None;
        }
        Some(Self { x1, y1, x2, y2 })
    }

    /// Pixel rect inside a w×h frame, always at least 1 px each way.
    pub fn to_px(self, w: u32, h: u32) -> (u32, u32, u32, u32) {
        let x1 = (self.x1 * w as f32).round().clamp(0.0, (w - 1) as f32) as u32;
        let y1 = (self.y1 * h as f32).round().clamp(0.0, (h - 1) as f32) as u32;
        let x2 = (self.x2 * w as f32).round().clamp((x1 + 1) as f32, w as f32) as u32;
        let y2 = (self.y2 * h as f32).round().clamp((y1 + 1) as f32, h as f32) as u32;
        (x1, y1, x2, y2)
    }
}

/// The one region the app watches. The camera is fixed, so a single box
/// marked on the live view applies to every frame and every reference.
pub fn load_region() -> Option<BoxNorm> {
    fs::read_to_string(REGION_PATH)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

pub fn save_region(region: Option<BoxNorm>) {
    if let Some(parent) = Path::new(REGION_PATH).parent() {
        let _ = fs::create_dir_all(parent);
    }
    match region {
        Some(r) => {
            if let Ok(s) = serde_json::to_string_pretty(&r) {
                let _ = fs::write(REGION_PATH, s);
            }
        }
        None => {
            let _ = fs::remove_file(REGION_PATH);
        }
    }
}

/// What the app compares every frame against: a handful of pictures of the
/// silo when it is empty.
///
/// There is no second class. A frame either looks like the empty silo or it
/// does not, and "how alike" is a plain percentage the operator can read.
#[derive(Clone, Serialize, Deserialize)]
pub struct Reference {
    /// One feature vector per reference photo.
    pub frames: Vec<Vec<f32>>,
    /// The same photos in black and white, matched by index.
    #[serde(default)]
    pub frames_bw: Vec<Vec<f32>>,
    pub roi: Option<BoxNorm>,
    /// Lowest similarity between two reference photos — how much the empty
    /// silo varies from one picture to the next.
    pub cohesion: f32,
    pub built_at_unix: u64,
}

impl Reference {
    pub fn load() -> Option<Self> {
        let s = fs::read_to_string(REFERENCE_PATH).ok()?;
        serde_json::from_str(&s).ok()
    }

    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = Path::new(REFERENCE_PATH).parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let s = serde_json::to_string(self).map_err(|e| e.to_string())?;
        fs::write(REFERENCE_PATH, s).map_err(|e| e.to_string())
    }

    pub fn count(&self) -> usize {
        self.frames.len()
    }

    /// 0..1, how closely this frame matches the closest reference.
    ///
    /// The plain picture and its black-and-white version are weighed equally:
    /// one carries brightness and texture, the other shape and shadow.
    pub fn similarity_pair(&self, feat: &[f32], feat_bw: &[f32]) -> f32 {
        let mut best = 0.0f32;
        for (i, r) in self.frames.iter().enumerate() {
            let gray = similarity(feat, r);
            let score = match self.frames_bw.get(i) {
                Some(rbw) => 0.5 * gray + 0.5 * similarity(feat_bw, rbw),
                None => gray,
            };
            best = best.max(score);
        }
        best
    }

    /// Match on the plain picture only, for stored photos shown in the UI.
    pub fn similarity(&self, feat: &[f32]) -> f32 {
        self.frames
            .iter()
            .map(|r| similarity(feat, r))
            .fold(0.0f32, f32::max)
    }

    /// Where to draw the line, unless the operator overrides it.
    ///
    /// Reference photos taken seconds apart look almost identical, which
    /// would put the bar so high that nothing ever clears it. The margin is
    /// therefore never smaller than [`MIN_MARGIN`], whatever the photos say.
    pub fn suggested_threshold(&self) -> f32 {
        const MIN_MARGIN: f32 = 0.10;
        let spread = 1.0 - self.cohesion;
        let margin = (spread * 3.0).max(MIN_MARGIN);
        (self.cohesion - margin).clamp(0.50, 0.95)
    }
}

/// 1.0 means identical. Root-mean-square pixel difference, inverted, so the
/// number reads as a percentage without further explanation.
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mse: f32 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f32>()
        / a.len() as f32;
    (1.0 - mse.sqrt()).clamp(0.0, 1.0)
}

/// Read every reference photo and build the comparison set from them.
///
/// The black-and-white twin is regenerated here rather than read from disk,
/// so changing the region re-derives it without stale files lingering.
pub fn build_reference() -> Result<Reference, String> {
    ensure_dirs();
    let roi = load_region();

    let mut frames = Vec::new();
    let mut frames_bw = Vec::new();
    for meta in list_references() {
        let img = match image::open(&meta.path) {
            Ok(i) => i.to_rgb8(),
            Err(_) => continue,
        };
        let (w, h) = img.dimensions();
        let rgb = img.into_raw();
        frames.push(features_in_roi(w, h, &rgb, roi));
        frames_bw.push(bw_features(w, h, &rgb, roi));

        // Keep the saved black-and-white picture in step with the region.
        let (bw, bh, pixels) = bw_from_rgb(w, h, &rgb, roi);
        let bw_path = bw_path_for(&meta.file_name);
        let _ = save_rgb_jpeg(
            bw_path.to_str().unwrap_or("data/tmp_bw.jpg"),
            bw,
            bh,
            &pixels,
        );
    }
    if frames.is_empty() {
        return Err("add at least one photo of the empty silo".into());
    }

    // How alike are the references themselves? That sets the bar for what
    // counts as a match, without needing any "not empty" examples.
    let cohesion = if frames.len() < 2 {
        0.97
    } else {
        let mut worst = 1.0f32;
        for (i, a) in frames.iter().enumerate() {
            let mut best = 0.0f32;
            for (j, b) in frames.iter().enumerate() {
                if i != j {
                    best = best.max(similarity(a, b));
                }
            }
            worst = worst.min(best);
        }
        worst
    };

    let reference = Reference {
        frames,
        frames_bw,
        roi,
        cohesion,
        built_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    reference.save()?;
    Ok(reference)
}

pub fn ensure_dirs() {
    let _ = fs::create_dir_all(EMPTY_DIR);
    let _ = fs::create_dir_all(BW_DIR);
}

/// Split the picture into black and white at the threshold that best
/// separates the two, so shadow and shape stand out instead of shading.
///
/// The split point comes from the picture itself (Otsu), so there is nothing
/// to tune and it holds as the view changes.
fn otsu_split(gray: &[u8]) -> u8 {
    let mut hist = [0u32; 256];
    for &g in gray {
        hist[g as usize] += 1;
    }
    let total = gray.len() as f32;
    let sum: f32 = (0..256).map(|i| i as f32 * hist[i] as f32).sum();
    let (mut sum_b, mut w_b, mut best_var, mut best_t) = (0.0f32, 0.0f32, -1.0f32, 128u8);
    for t in 0..256 {
        w_b += hist[t] as f32;
        if w_b == 0.0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f == 0.0 {
            break;
        }
        sum_b += t as f32 * hist[t] as f32;
        let m_b = sum_b / w_b;
        let m_f = (sum - sum_b) / w_f;
        let var = w_b * w_f * (m_b - m_f) * (m_b - m_f);
        if var > best_var {
            best_var = var;
            best_t = t as u8;
        }
    }
    best_t
}

/// The high-contrast view of a frame: cropped to the region, pushed to pure
/// black and white. Returned at crop resolution so it can be shown and saved.
pub fn bw_from_rgb(w: u32, h: u32, rgb: &[u8], roi: Option<BoxNorm>) -> (u32, u32, Vec<u8>) {
    let img: RgbImage = ImageBuffer::from_raw(w, h, rgb.to_vec())
        .unwrap_or_else(|| RgbImage::new(w.max(1), h.max(1)));
    let view = match roi {
        Some(b) if w > 1 && h > 1 => {
            let (x1, y1, x2, y2) = b.to_px(w, h);
            image::imageops::crop_imm(&img, x1, y1, x2 - x1, y2 - y1).to_image()
        }
        _ => img,
    };
    let (vw, vh) = view.dimensions();
    let gray: Vec<u8> = view
        .pixels()
        .map(|Rgb([r, g, b])| {
            (0.299 * *r as f32 + 0.587 * *g as f32 + 0.114 * *b as f32) as u8
        })
        .collect();
    let t = otsu_split(&gray);
    let mut out = Vec::with_capacity(gray.len() * 3);
    for g in gray {
        let v = if g > t { 255u8 } else { 0u8 };
        out.extend_from_slice(&[v, v, v]);
    }
    (vw, vh, out)
}

/// A small copy of what the app just looked at, for the terminal.
pub fn thumb_from_rgb(
    w: u32,
    h: u32,
    rgb: &[u8],
    roi: Option<BoxNorm>,
    max_side: u32,
) -> (u32, u32, Vec<u8>) {
    let img: RgbImage = ImageBuffer::from_raw(w, h, rgb.to_vec())
        .unwrap_or_else(|| RgbImage::new(w.max(1), h.max(1)));
    let view = match roi {
        Some(b) if w > 1 && h > 1 => {
            let (x1, y1, x2, y2) = b.to_px(w, h);
            image::imageops::crop_imm(&img, x1, y1, x2 - x1, y2 - y1).to_image()
        }
        _ => img,
    };
    let (vw, vh) = view.dimensions();
    let long = vw.max(vh).max(1);
    if long <= max_side {
        return (vw, vh, view.into_raw());
    }
    let scale = max_side as f32 / long as f32;
    let nw = ((vw as f32) * scale).round().max(1.0) as u32;
    let nh = ((vh as f32) * scale).round().max(1.0) as u32;
    let small = image::imageops::resize(&view, nw, nh, FilterType::Triangle);
    (nw, nh, small.into_raw())
}

/// The black-and-white view reduced to the 32×32 signature used for matching.
pub fn bw_features(w: u32, h: u32, rgb: &[u8], roi: Option<BoxNorm>) -> Vec<f32> {
    let (bw, bh, pixels) = bw_from_rgb(w, h, rgb, roi);
    // No roi here: bw_from_rgb already cropped.
    features_in_roi(bw, bh, &pixels, None)
}
pub fn features_in_roi(w: u32, h: u32, rgb: &[u8], roi: Option<BoxNorm>) -> Vec<f32> {
    let img: RgbImage = ImageBuffer::from_raw(w, h, rgb.to_vec())
        .unwrap_or_else(|| RgbImage::new(w.max(1), h.max(1)));
    let view = match roi {
        Some(b) if w > 1 && h > 1 => {
            let (x1, y1, x2, y2) = b.to_px(w, h);
            image::imageops::crop_imm(&img, x1, y1, x2 - x1, y2 - y1).to_image()
        }
        _ => img,
    };
    let small = image::imageops::resize(&view, SIZE, SIZE, FilterType::Triangle);
    let mut out = Vec::with_capacity((SIZE * SIZE) as usize);
    for p in small.pixels() {
        let Rgb([r, g, b]) = *p;
        out.push((0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0);
    }
    out
}

pub fn save_rgb_jpeg(path: &str, w: u32, h: u32, rgb: &[u8]) -> Result<(), String> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let img: RgbImage =
        ImageBuffer::from_raw(w, h, rgb.to_vec()).ok_or("bad rgb buffer")?;
    img.save(path).map_err(|e| e.to_string())
}
/// Store the current frame as a reference of the empty silo: the picture as
/// the camera saw it, plus its black-and-white version.
pub fn save_reference_photo(w: u32, h: u32, rgb: &[u8]) -> Result<PathBuf, String> {
    ensure_dirs();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("{ts}.jpg");

    let path = PathBuf::from(EMPTY_DIR).join(&name);
    save_rgb_jpeg(path.to_str().unwrap_or("data/tmp.jpg"), w, h, rgb)?;

    let roi = load_region();
    let (bw, bh, pixels) = bw_from_rgb(w, h, rgb, roi);
    let bw_path = bw_path_for(&name);
    save_rgb_jpeg(
        bw_path.to_str().unwrap_or("data/tmp_bw.jpg"),
        bw,
        bh,
        &pixels,
    )?;

    Ok(path)
}

/// The black-and-white twin of a stored reference photo.
pub fn bw_path_for(file_name: &str) -> PathBuf {
    PathBuf::from(BW_DIR).join(file_name)
}
#[derive(Clone)]
pub struct SampleMeta {
    pub path: PathBuf,
    /// `None` = unlabeled inbox, `Some(true)` = empty, `Some(false)` = full.
    pub label_empty: Option<bool>,
    pub file_name: String,
    pub bytes: u64,
    pub captured_ms: Option<u64>,
}

/// Every reference photo on disk, newest first.
pub fn list_references() -> Vec<SampleMeta> {
    ensure_dirs();
    let mut out = samples_in(EMPTY_DIR, Some(true));
    out.sort_by(|a, b| {
        b.captured_ms
            .cmp(&a.captured_ms)
            .then(b.file_name.cmp(&a.file_name))
    });
    out
}

#[derive(Clone)]
pub struct SampleAnalysis {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    /// How closely this photo matches the reference set, when one exists.
    pub match_score: Option<f32>,
}
pub fn delete_sample(path: &Path) -> Result<(), String> {
    let canon = path
        .canonicalize()
        .map_err(|e| format!("resolve image: {e}"))?;
    let mut ok = false;
    for dir in [EMPTY_DIR] {
        if let Ok(root) = Path::new(dir).canonicalize() {
            if canon.starts_with(&root) {
                ok = true;
                break;
            }
        }
    }
    if !ok {
        return Err("refusing to delete outside dataset folders".into());
    }
    let ext = canon
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext != "jpg" && ext != "jpeg" {
        return Err("not a jpeg training image".into());
    }
    if let Some(name) = canon.file_name().and_then(|n| n.to_str()) {
        let _ = fs::remove_file(bw_path_for(name));
    }
    fs::remove_file(&canon).map_err(|e| format!("delete failed: {e}"))
}
fn samples_in(dir: &str, label_empty: Option<bool>) -> Vec<SampleMeta> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.filter_map(|e| e.ok()) {
        let path = e.path();
        let ext = path
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext != "jpg" && ext != "jpeg" {
            continue;
        }
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let captured_ms = file_name
            .trim_end_matches(".jpg")
            .trim_end_matches(".jpeg")
            .trim_end_matches(".JPG")
            .parse::<u64>()
            .ok();
        let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(SampleMeta {
            path,
            label_empty,
            file_name,
            bytes,
            captured_ms,
        });
    }
    out
}

/// Small RGBA thumb for the Library grid.
pub fn load_thumb_rgba(path: &Path, max_side: u32) -> Result<(u32, u32, Vec<u8>), String> {
    let img = image::open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .to_rgba8();
    let (w, h) = img.dimensions();
    let long = w.max(h).max(1);
    if long <= max_side {
        return Ok((w, h, img.into_raw()));
    }
    let scale = max_side as f32 / long as f32;
    let nw = ((w as f32) * scale).round().max(1.0) as u32;
    let nh = ((h as f32) * scale).round().max(1.0) as u32;
    let small = image::imageops::resize(&img, nw, nh, FilterType::Triangle);
    Ok((nw, nh, small.into_raw()))
}
pub fn analyze_sample(
    path: &Path,
    reference: Option<&Reference>,
) -> Result<SampleAnalysis, String> {
    let img = image::open(path)
        .map_err(|e| format!("open image: {e}"))?
        .to_rgb8();
    let width = img.width();
    let height = img.height();
    let rgb = img.into_raw();
    let roi = reference.and_then(|r| r.roi).or_else(load_region);
    let features = features_in_roi(width, height, &rgb, roi);
    let match_score = reference.map(|r| r.similarity(&features));

    Ok(SampleAnalysis {
        width,
        height,
        rgb,
        match_score,
    })
}

