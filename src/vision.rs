use image::{imageops::FilterType, ImageBuffer, Rgb, RgbImage};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SIZE: u32 = 32;
pub const MODEL_PATH: &str = "data/model.json";
pub const EMPTY_DIR: &str = "data/empty";
pub const FULL_DIR: &str = "data/full";
pub const LAST_CHECK_PATH: &str = "data/last_check.jpg";

fn default_model_name() -> String {
    "Spectr Silo".into()
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Model {
    #[serde(default = "default_model_name")]
    pub name: String,
    pub w: u32,
    pub h: u32,
    pub empty: Vec<f32>,
    pub full: Vec<f32>,
    pub train_accuracy: f32,
    pub trained_at_unix: u64,
    #[serde(default)]
    pub n_empty: u32,
    #[serde(default)]
    pub n_full: u32,
}

#[derive(Clone, Copy)]
pub struct Prediction {
    /// true = silo empty
    pub empty: bool,
    pub dist_empty: f32,
    pub dist_full: f32,
}

impl Prediction {
    /// 0..1 how clearly closer to the winning class
    pub fn confidence(self) -> f32 {
        let sum = self.dist_empty + self.dist_full;
        if sum <= f32::EPSILON {
            return 0.5;
        }
        let winner = if self.empty {
            self.dist_full
        } else {
            self.dist_empty
        };
        (winner / sum).clamp(0.0, 1.0)
    }
}

impl Model {
    pub fn load(path: &str) -> Option<Self> {
        let s = fs::read_to_string(path).ok()?;
        serde_json::from_str(&s).ok()
    }

    pub fn save(&self, path: &str) -> Result<(), String> {
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, s).map_err(|e| e.to_string())
    }

    pub fn predict(&self, feat: &[f32]) -> Prediction {
        let dist_empty = dist(feat, &self.empty);
        let dist_full = dist(feat, &self.full);
        Prediction {
            empty: dist_empty <= dist_full,
            dist_empty,
            dist_full,
        }
    }

    pub fn mean_brightness_empty(&self) -> f32 {
        mean(&self.empty)
    }

    pub fn mean_brightness_full(&self) -> f32 {
        mean(&self.full)
    }

    pub fn separation(&self) -> f32 {
        dist(&self.empty, &self.full)
    }

    pub fn display_name(&self) -> &str {
        let t = self.name.trim();
        if t.is_empty() {
            "Spectr Silo"
        } else {
            t
        }
    }

    /// 0..=100 composite score from accuracy, class separation, and sample size.
    pub fn quality_score(&self) -> u8 {
        let acc = (self.train_accuracy.clamp(0.0, 1.0) * 55.0) as f32;
        let sep = (self.separation() / (self.separation() + 2.0)).clamp(0.0, 1.0) * 25.0;
        let n = (self.n_empty + self.n_full) as f32;
        let samples = ((n / (n + 8.0)) * 20.0).clamp(0.0, 20.0);
        (acc + sep + samples).round().clamp(0.0, 100.0) as u8
    }

    /// Human-readable grade for the Model page.
    pub fn quality_label(&self) -> &'static str {
        match self.quality_score() {
            90..=100 => "Excellent",
            75..=89 => "Good",
            55..=74 => "Fair",
            35..=54 => "Weak",
            _ => "Poor",
        }
    }

    pub fn quality_blurb(&self) -> String {
        let score = self.quality_score();
        let label = self.quality_label();
        let n = self.n_empty + self.n_full;
        match label {
            "Excellent" => format!(
                "{label} ({score}/100) — strong separation and {n} training images; ready for production use."
            ),
            "Good" => format!(
                "{label} ({score}/100) — reliable on the current labels. More varied Empty/Full frames will harden it further."
            ),
            "Fair" => format!(
                "{label} ({score}/100) — usable, but expect occasional mismatches. Add more labels and retrain."
            ),
            "Weak" => format!(
                "{label} ({score}/100) — classes look similar or the set is thin. Label clearer Empty vs Full frames."
            ),
            _ => format!(
                "{label} ({score}/100) — not ready. Label several Empty and Full images, then Train again."
            ),
        }
    }
}

pub fn ensure_dirs() {
    let _ = fs::create_dir_all(EMPTY_DIR);
    let _ = fs::create_dir_all(FULL_DIR);
}

pub fn features_from_rgb(w: u32, h: u32, rgb: &[u8]) -> Vec<f32> {
    let img: RgbImage = ImageBuffer::from_raw(w, h, rgb.to_vec())
        .unwrap_or_else(|| RgbImage::new(w.max(1), h.max(1)));
    let small = image::imageops::resize(&img, SIZE, SIZE, FilterType::Triangle);
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

pub fn save_label(empty: bool, w: u32, h: u32, rgb: &[u8]) -> Result<PathBuf, String> {
    ensure_dirs();
    let dir = if empty { EMPTY_DIR } else { FULL_DIR };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = PathBuf::from(dir).join(format!("{ts}.jpg"));
    save_rgb_jpeg(path.to_str().unwrap_or("data/tmp.jpg"), w, h, rgb)?;
    Ok(path)
}

pub fn count_labels() -> (usize, usize) {
    (count_jpgs(EMPTY_DIR), count_jpgs(FULL_DIR))
}

#[derive(Clone)]
pub struct SampleMeta {
    pub path: PathBuf,
    pub label_empty: bool,
    pub file_name: String,
    pub bytes: u64,
    pub captured_ms: Option<u64>,
}

#[derive(Clone)]
pub struct SampleAnalysis {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    pub features: Vec<f32>,
    pub mean_brightness: f32,
    pub pred: Option<Prediction>,
    pub agrees: Option<bool>,
    pub reasoning: String,
}

pub fn list_samples() -> Vec<SampleMeta> {
    ensure_dirs();
    let mut out = Vec::new();
    out.extend(samples_in(EMPTY_DIR, true));
    out.extend(samples_in(FULL_DIR, false));
    out.sort_by(|a, b| b.captured_ms.cmp(&a.captured_ms).then(b.file_name.cmp(&a.file_name)));
    out
}

/// Delete one labeled training image. Path must be under data/empty or data/full.
pub fn delete_sample(path: &Path) -> Result<(), String> {
    let canon = path
        .canonicalize()
        .map_err(|e| format!("resolve {}: {e}", path.display()))?;
    let empty_root = Path::new(EMPTY_DIR)
        .canonicalize()
        .map_err(|e| format!("{EMPTY_DIR}: {e}"))?;
    let full_root = Path::new(FULL_DIR)
        .canonicalize()
        .map_err(|e| format!("{FULL_DIR}: {e}"))?;
    if !(canon.starts_with(&empty_root) || canon.starts_with(&full_root)) {
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
    fs::remove_file(&canon).map_err(|e| format!("delete {}: {e}", canon.display()))
}

fn samples_in(dir: &str, label_empty: bool) -> Vec<SampleMeta> {
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

pub fn analyze_sample(path: &Path, label_empty: bool, model: Option<&Model>) -> Result<SampleAnalysis, String> {
    let img = image::open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .to_rgb8();
    let width = img.width();
    let height = img.height();
    let rgb = img.into_raw();
    let features = features_from_rgb(width, height, &rgb);
    let mean_brightness = mean(&features);

    let (pred, agrees, reasoning) = if let Some(m) = model {
        let p = m.predict(&features);
        let agrees = p.empty == label_empty;
        let label = if label_empty { "EMPTY" } else { "FULL" };
        let guessed = if p.empty { "EMPTY" } else { "FULL" };
        let closer = if p.empty { "EMPTY" } else { "FULL" };
        let margin = (p.dist_empty - p.dist_full).abs();
        let reason = format!(
            "Human label: {label}. \
             Model shrinks the photo to {SIZE}×{SIZE} grayscale ({feat} numbers), \
             then measures distance to the EMPTY centroid ({de:.3}) and FULL centroid ({df:.3}). \
             Closer to {closer} by margin {margin:.3} → predicts {guessed} at {conf:.0}% confidence. \
             Image mean brightness {mb:.3} vs EMPTY centroid {me:.3} / FULL centroid {mf:.3}. \
             {match_txt}",
            feat = features.len(),
            de = p.dist_empty,
            df = p.dist_full,
            conf = p.confidence() * 100.0,
            mb = mean_brightness,
            me = m.mean_brightness_empty(),
            mf = m.mean_brightness_full(),
            match_txt = if agrees {
                "Label and model agree."
            } else {
                "Mismatch — model disagrees with the human label."
            }
        );
        (Some(p), Some(agrees), reason)
    } else {
        (
            None,
            None,
            format!(
                "Human label: {}. No trained model yet — distances unavailable. \
                 Image is {}×{}, mean brightness {:.3}. Train a model to see reasoning.",
                if label_empty { "EMPTY" } else { "FULL" },
                width,
                height,
                mean_brightness
            ),
        )
    };

    Ok(SampleAnalysis {
        width,
        height,
        rgb,
        features,
        mean_brightness,
        pred,
        agrees,
        reasoning,
    })
}

fn count_jpgs(dir: &str) -> usize {
    fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| {
                    e.path()
                        .extension()
                        .and_then(|x| x.to_str())
                        .map(|x| x.eq_ignore_ascii_case("jpg") || x.eq_ignore_ascii_case("jpeg"))
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

fn load_feats(dir: &str) -> Result<Vec<Vec<f32>>, String> {
    let mut feats = Vec::new();
    let rd = fs::read_dir(dir).map_err(|e| format!("{dir}: {e}"))?;
    for e in rd.filter_map(|e| e.ok()) {
        let p = e.path();
        let ext = p
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext != "jpg" && ext != "jpeg" {
            continue;
        }
        let img = image::open(&p)
            .map_err(|e| format!("{}: {e}", p.display()))?
            .to_rgb8();
        feats.push(features_from_rgb(img.width(), img.height(), img.as_raw()));
    }
    Ok(feats)
}

fn centroid(feats: &[Vec<f32>]) -> Vec<f32> {
    let n = feats.len() as f32;
    let dim = feats[0].len();
    let mut c = vec![0.0f32; dim];
    for f in feats {
        for (i, v) in f.iter().enumerate() {
            c[i] += *v;
        }
    }
    for v in &mut c {
        *v /= n;
    }
    c
}

fn dist(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

fn mean(v: &[f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().sum::<f32>() / v.len() as f32
}

pub fn train() -> Result<Model, String> {
    ensure_dirs();
    let empty_feats = load_feats(EMPTY_DIR)?;
    let full_feats = load_feats(FULL_DIR)?;
    if empty_feats.is_empty() {
        return Err("need ≥1 image in data/empty".into());
    }
    if full_feats.is_empty() {
        return Err("need ≥1 image in data/full".into());
    }

    let empty_c = centroid(&empty_feats);
    let full_c = centroid(&full_feats);

    let mut ok = 0usize;
    let mut total = 0usize;
    for f in &empty_feats {
        total += 1;
        if dist(f, &empty_c) <= dist(f, &full_c) {
            ok += 1;
        }
    }
    for f in &full_feats {
        total += 1;
        if dist(f, &full_c) < dist(f, &empty_c) {
            ok += 1;
        }
    }

    let name = Model::load(MODEL_PATH)
        .map(|m| {
            let t = m.name.trim().to_string();
            if t.is_empty() {
                default_model_name()
            } else {
                t
            }
        })
        .unwrap_or_else(default_model_name);

    let model = Model {
        name,
        w: SIZE,
        h: SIZE,
        empty: empty_c,
        full: full_c,
        train_accuracy: ok as f32 / total as f32,
        trained_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        n_empty: empty_feats.len() as u32,
        n_full: full_feats.len() as u32,
    };
    model.save(MODEL_PATH)?;
    Ok(model)
}
