mod camera;
mod config;
mod cv;
mod engine;
mod eventlog;
mod silo_alert;
mod stats;
mod vision;

use chrono::{DateTime, Local, TimeZone};
use config::{Config, RoiCfg};
use cv::{DropDetector, LevelDetector, LevelResult};
use eframe::egui::{self, Color32, Pos2, RichText, Sense, Vec2};
use engine::{AlarmLevel, LevelState, MonitorEngine, Snapshot};
use eventlog::EventLog;
use silo_alert::SiloAlert;
use stats::Stats;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use vision::{Model, Prediction};

#[allow(dead_code)]
const CHECK_EVERY: Duration = Duration::from_secs(10);
const LOG_MAX: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Monitor,
    Train,
    Dataset,
    Model,
}

/// Which ROI the user is about to draw on the live image.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RoiTool {
    None,
    Level,
    Drop,
}

struct LogLine {
    unix: u64,
    text: String,
}

struct App {
    cam: Option<camera::Cam>,
    live_tex: Option<egui::TextureHandle>,
    snap_tex: Option<egui::TextureHandle>,
    logo_tex: Option<egui::TextureHandle>,
    dataset_tex: Option<egui::TextureHandle>,
    feat_tex: Option<egui::TextureHandle>,
    last_rgb: Option<(u32, u32, Vec<u8>)>,
    model: Option<Model>,
    model_name_draft: String,
    alert: Option<SiloAlert>,
    stats: Stats,
    boot: Instant,
    page: Page,
    monitor: bool,
    #[allow(dead_code)]
    last_check: Instant,
    #[allow(dead_code)]
    last_pred: Option<Prediction>,
    status: String,
    log: VecDeque<LogLine>,
    samples: Vec<vision::SampleMeta>,
    sample_idx: Option<usize>,
    loaded_sample_idx: Option<usize>,
    sample_view: Option<vision::SampleAnalysis>,
    dataset_dirty: bool,
    // Unified CV pipeline
    cfg: Config,
    level_det: LevelDetector,
    drop_det: DropDetector,
    engine: MonitorEngine,
    events: EventLog,
    last_level: LevelResult,
    last_snap: Option<Snapshot>,
    /// User draws LEVEL / DROP boxes on the live camera.
    roi_tool: RoiTool,
    roi_drag_start: Option<(i32, i32)>,
    roi_drag_cur: Option<(i32, i32)>,
}

impl App {
    fn new(cam: Option<camera::Cam>) -> Self {
        vision::ensure_dirs();
        let cfg = Config::load();
        let model = Model::load(vision::MODEL_PATH);
        let model_name_draft = model
            .as_ref()
            .map(|m| m.display_name().to_string())
            .unwrap_or_else(|| "Spectr Silo".into());
        let mut stats = Stats::load();
        let (ne, nf) = vision::count_labels();
        stats.labels_empty = ne as u64;
        stats.labels_full = nf as u64;

        let alert = match SiloAlert::new() {
            Ok(mut a) => {
                a.restore_last_alert_unix(stats.last_alert_unix);
                eprintln!("GPIO alert ready.");
                Some(a)
            }
            Err(e) => {
                eprintln!("GPIO init failed (preview/label/train still work): {e}");
                None
            }
        };

        let level_det = LevelDetector::new(
            cfg.level_roi,
            cfg.level_full_at_top,
            cfg.level.clone(),
        );
        let drop_det = DropDetector::new(cfg.drop_roi, cfg.drop.clone());
        let engine = MonitorEngine::new(cfg.level.clone(), cfg.alarm.clone());
        let events = EventLog::open(
            cfg.resolve_db_path().with_extension("jsonl"),
            cfg.storage.level_log_interval_seconds,
        );

        let status = if cam.is_some() {
            "Ready — Start monitoring for level + drops".into()
        } else {
            "No camera — UI works; plug in a camera and restart for live video".into()
        };

        let mut app = Self {
            cam,
            live_tex: None,
            snap_tex: None,
            logo_tex: None,
            dataset_tex: None,
            feat_tex: None,
            last_rgb: None,
            model,
            model_name_draft,
            alert,
            stats,
            boot: Instant::now(),
            page: Page::Monitor,
            monitor: false,
            last_check: Instant::now() - CHECK_EVERY,
            last_pred: None,
            status,
            log: VecDeque::new(),
            samples: Vec::new(),
            sample_idx: None,
            loaded_sample_idx: None,
            sample_view: None,
            dataset_dirty: true,
            cfg,
            level_det,
            drop_det,
            engine,
            events,
            last_level: LevelResult {
                level_y: None,
                level_percent: None,
                confidence: 0.0,
                detected: false,
            },
            last_snap: None,
            roi_tool: RoiTool::None,
            roi_drag_start: None,
            roi_drag_cur: None,
        };
        app.push_log("System started (unified UI + CV)");
        if app.cam.is_none() {
            app.push_log("No camera detected — live preview unavailable until restart with camera");
        } else {
            app.push_log("Draw LEVEL / DROP boxes on the live image (Monitor → Regions)");
        }
        if app.model.is_some() {
            app.push_log("Optional ML model loaded (Model tab)");
        }
        app
    }

    fn ensure_logo(&mut self, ctx: &egui::Context) {
        if self.logo_tex.is_some() {
            return;
        }
        // Official Spectr mark from spectr.no (white for dark chrome)
        const LOGO: &[u8] = include_bytes!("../assets/spectr-logo-white.png");
        let Ok(img) = image::load_from_memory(LOGO) else {
            return;
        };
        let rgba = img.to_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
        self.logo_tex = Some(ctx.load_texture("spectr_logo", color, Default::default()));
    }

    fn load_snap_tex(&mut self, ctx: &egui::Context) {
        if self.snap_tex.is_some() {
            return;
        }
        let Ok(img) = image::open(vision::LAST_CHECK_PATH) else {
            return;
        };
        let rgb = img.to_rgb8();
        let size = [rgb.width() as usize, rgb.height() as usize];
        let color = egui::ColorImage::from_rgb(size, rgb.as_raw());
        self.snap_tex = Some(ctx.load_texture("snap", color, Default::default()));
    }

    fn apply_roi(&mut self, which: RoiTool, roi: RoiCfg) {
        let (w, h) = self
            .last_rgb
            .as_ref()
            .map(|(w, h, _)| (*w, *h))
            .unwrap_or((640, 480));
        let roi = roi.clamp_i32(w, h);
        match which {
            RoiTool::Level => {
                self.cfg.level_roi = roi;
                self.level_det.set_roi(roi);
                self.push_log(format!(
                    "LEVEL ROI set · ({},{})–({},{})",
                    roi.x1, roi.y1, roi.x2, roi.y2
                ));
                self.status = "LEVEL region saved — fill area for level tracking".into();
            }
            RoiTool::Drop => {
                self.cfg.drop_roi = roi;
                self.drop_det.set_roi(roi);
                self.push_log(format!(
                    "DROP ROI set · ({},{})–({},{})",
                    roi.x1, roi.y1, roi.x2, roi.y2
                ));
                self.status = "DROP region saved — where granules fall".into();
            }
            RoiTool::None => return,
        }
        if let Err(e) = self.cfg.save_rois() {
            self.status = format!("ROI set, but save failed: {e}");
            self.push_log(format!("ROI save failed: {e}"));
        }
        self.roi_tool = RoiTool::None;
        self.roi_drag_start = None;
        self.roi_drag_cur = None;
    }

    fn handle_roi_drag(&mut self, response: &egui::Response, img_rect: egui::Rect) {
        if self.roi_tool == RoiTool::None {
            return;
        }
        let Some((w, h, _)) = &self.last_rgb else {
            return;
        };
        let (w, h) = (*w, *h);

        let to_img = |pos: Pos2| -> Option<(i32, i32)> {
            if !img_rect.contains(pos) {
                return None;
            }
            let u = ((pos.x - img_rect.min.x) / img_rect.width()).clamp(0.0, 1.0);
            let v = ((pos.y - img_rect.min.y) / img_rect.height()).clamp(0.0, 1.0);
            Some(((u * w as f32) as i32, (v * h as f32) as i32))
        };

        if response.drag_started() {
            if let Some(p) = response.interact_pointer_pos().and_then(to_img) {
                self.roi_drag_start = Some(p);
                self.roi_drag_cur = Some(p);
            }
        }
        if response.dragged() {
            if let Some(p) = response.interact_pointer_pos().and_then(to_img) {
                self.roi_drag_cur = Some(p);
            }
        }
        if response.drag_stopped() {
            if let (Some(a), Some(b)) = (self.roi_drag_start, self.roi_drag_cur) {
                if let Some(roi) = RoiCfg::from_drag(a.0, a.1, b.0, b.1) {
                    let tool = self.roi_tool;
                    self.apply_roi(tool, roi);
                } else {
                    self.status = "ROI too small — drag a larger box".into();
                    self.roi_drag_start = None;
                    self.roi_drag_cur = None;
                }
            }
        }
    }

    fn push_log(&mut self, text: impl Into<String>) {
        self.log.push_front(LogLine {
            unix: stats::now_unix(),
            text: text.into(),
        });
        while self.log.len() > LOG_MAX {
            self.log.pop_back();
        }
    }

    fn save_label(&mut self, empty: bool) {
        let Some((w, h, rgb)) = self.last_rgb.clone() else {
            self.status = "No camera frame yet".into();
            return;
        };
        match vision::save_label(empty, w, h, &rgb) {
            Ok(p) => {
                if empty {
                    self.stats.labels_empty += 1;
                } else {
                    self.stats.labels_full += 1;
                }
                self.stats.save();
                let kind = if empty { "EMPTY" } else { "FULL" };
                self.status = format!("Labeled {kind} → {}", p.display());
                self.push_log(format!("Labeled {kind} ({})", p.file_name().unwrap_or_default().to_string_lossy()));
                self.dataset_dirty = true;
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }

    fn do_train(&mut self) {
        match vision::train() {
            Ok(m) => {
                self.stats.last_train_unix = Some(m.trained_at_unix);
                self.stats.last_train_accuracy = Some(m.train_accuracy);
                self.stats.labels_empty = m.n_empty as u64;
                self.stats.labels_full = m.n_full as u64;
                self.stats.save();
                self.status = format!(
                    "Trained — {:.0}% on {} images",
                    m.train_accuracy * 100.0,
                    m.n_empty + m.n_full
                );
                self.push_log(format!(
                    "Model trained · {:.0}% accuracy · {} empty / {} full",
                    m.train_accuracy * 100.0, m.n_empty, m.n_full
                ));
                self.model_name_draft = m.display_name().to_string();
                self.model = Some(m);
                self.dataset_dirty = true;
            }
            Err(e) => {
                self.status = format!("Train failed: {e}");
                self.push_log(format!("Train failed: {e}"));
            }
        }
    }

    fn maybe_check(&mut self, _ctx: &egui::Context) {
        // Legacy 10s ML check disabled — continuous CV in run_cv() while monitoring.
    }

    fn pull_frame(&mut self, ctx: &egui::Context) {
        let Some(cam) = self.cam.as_mut() else {
            return;
        };
        if let Some((w, h, rgb)) = cam.frame() {
            let mut vis = rgb.clone();
            if self.monitor {
                self.run_cv(w, h, &rgb);
            }
            draw_roi_rect(
                &mut vis,
                w,
                self.cfg.level_roi.x1,
                self.cfg.level_roi.y1,
                self.cfg.level_roi.x2,
                self.cfg.level_roi.y2,
                [0, 220, 80],
            );
            draw_roi_rect(
                &mut vis,
                w,
                self.cfg.drop_roi.x1,
                self.cfg.drop_roi.y1,
                self.cfg.drop_roi.x2,
                self.cfg.drop_roi.y2,
                [0, 140, 255],
            );
            if let Some(y) = self.last_level.level_y {
                draw_hline(
                    &mut vis,
                    w,
                    h,
                    self.cfg.level_roi.x1,
                    self.cfg.level_roi.x2,
                    y.round() as i32,
                    [0, 255, 255],
                );
            }
            if let Some((x, y, bw, bh)) = self.drop_det.last_box {
                draw_roi_rect(
                    &mut vis,
                    w,
                    x as i32,
                    y as i32,
                    (x + bw) as i32,
                    (y + bh) as i32,
                    [255, 40, 40],
                );
            }
            // In-progress user drag (dashed feel via bright yellow)
            if let (Some(a), Some(b)) = (self.roi_drag_start, self.roi_drag_cur) {
                draw_roi_rect(
                    &mut vis,
                    w,
                    a.0,
                    a.1,
                    b.0,
                    b.1,
                    [255, 220, 40],
                );
            }

            let img = egui::ColorImage::from_rgb([w as usize, h as usize], &vis);
            match &mut self.live_tex {
                Some(t) => t.set(img, Default::default()),
                None => {
                    self.live_tex = Some(ctx.load_texture("live", img, Default::default()));
                }
            }
            self.last_rgb = Some((w, h, rgb));
        }
    }

    fn run_cv(&mut self, w: u32, h: u32, rgb: &[u8]) {
        let unix = eventlog::now_unix();
        let now_ms = eventlog::now_ms();
        let lr = self.level_det.update(w, h, rgb);
        self.last_level = lr;

        if let Some(ev) = self.drop_det.update(w, h, rgb, now_ms) {
            self.engine.register_drop(ev, unix);
            self.events.drop_ev(unix, ev.confidence);
            self.push_log(format!("Drop #{} (conf {:.0}%)", self.engine.drop_count, ev.confidence * 100.0));
        }

        if let Some((old, new)) = self.engine.update_level(&lr, unix) {
            self.events
                .state(unix, "level_state", old.as_str(), new.as_str(), "");
            self.push_log(format!("Level {} → {}", old.as_str(), new.as_str()));
        }

        if let Some((old, new, msg)) = self.engine.update_alarm(&lr, unix) {
            self.events.alarm(unix, new.as_str(), &msg);
            self.events
                .state(unix, "alarm", old.as_str(), new.as_str(), &msg);
            self.push_log(format!("Alarm {} → {}: {msg}", old.as_str(), new.as_str()));
        }

        let snap = self.engine.snapshot(&lr, unix);
        self.events.level(
            unix,
            lr.level_percent,
            lr.level_y,
            lr.confidence,
            lr.detected,
            false,
        );

        // Radio TX when EMPTY confirmed
        let empty = self.engine.level_state == LevelState::Empty;
        if let Some(alert) = &mut self.alert {
            if alert.update(empty) {
                self.stats.alerts_sent += 1;
                self.stats.last_alert_unix = alert.last_alert_unix();
                self.stats.save();
                self.push_log("ALERT transmitted — silo empty");
            }
        }

        if let Some(pct) = lr.level_percent {
            self.status = format!(
                "{} · {:.1}% · {} · drops {} · {}",
                snap.level_state.as_str(),
                pct,
                snap.alarm.as_str(),
                snap.drop_count,
                snap.alarm_message
            );
        }
        self.last_snap = Some(snap);

        // Keep legacy stats roughly in sync
        self.stats.last_check_unix = Some(unix);
        self.stats.last_check_empty = Some(empty);
        if empty {
            self.stats.empty_hits += 1;
        }
        self.stats.checks = self.stats.checks.saturating_add(1);
        if self.stats.checks % 30 == 0 {
            self.stats.save();
        }
    }

    fn refresh_dataset_list(&mut self) {
        self.samples = vision::list_samples();
        self.loaded_sample_idx = None;
        self.sample_view = None;
        if self.samples.is_empty() {
            self.sample_idx = None;
            self.dataset_tex = None;
            self.feat_tex = None;
        } else if self.sample_idx.map(|i| i >= self.samples.len()).unwrap_or(true) {
            self.sample_idx = Some(0);
        }
        self.dataset_dirty = false;
    }

    fn select_sample(&mut self, ctx: &egui::Context, idx: usize) {
        if idx >= self.samples.len() {
            return;
        }
        self.sample_idx = Some(idx);
        let meta = self.samples[idx].clone();
        match vision::analyze_sample(&meta.path, meta.label_empty, self.model.as_ref()) {
            Ok(view) => {
                let img = egui::ColorImage::from_rgb(
                    [view.width as usize, view.height as usize],
                    &view.rgb,
                );
                match &mut self.dataset_tex {
                    Some(t) => t.set(img, Default::default()),
                    None => {
                        self.dataset_tex =
                            Some(ctx.load_texture("dataset", img, Default::default()));
                    }
                }
                let mut pix = Vec::with_capacity((vision::SIZE * vision::SIZE * 3) as usize);
                for v in &view.features {
                    let g = (v.clamp(0.0, 1.0) * 255.0) as u8;
                    pix.extend_from_slice(&[g, g, g]);
                }
                let feat_img = egui::ColorImage::from_rgb(
                    [vision::SIZE as usize, vision::SIZE as usize],
                    &pix,
                );
                match &mut self.feat_tex {
                    Some(t) => t.set(feat_img, Default::default()),
                    None => {
                        self.feat_tex =
                            Some(ctx.load_texture("feat", feat_img, Default::default()));
                    }
                }
                self.sample_view = Some(view);
                self.loaded_sample_idx = Some(idx);
                self.status = format!("Inspecting {}", meta.file_name);
            }
            Err(e) => {
                self.sample_view = None;
                self.loaded_sample_idx = None;
                self.status = format!("Open failed: {e}");
            }
        }
    }

    fn delete_selected_sample(&mut self) {
        let Some(idx) = self.sample_idx.filter(|i| *i < self.samples.len()) else {
            self.status = "No image selected to delete".into();
            return;
        };
        let meta = self.samples[idx].clone();
        match vision::delete_sample(&meta.path) {
            Ok(()) => {
                if meta.label_empty {
                    self.stats.labels_empty = self.stats.labels_empty.saturating_sub(1);
                } else {
                    self.stats.labels_full = self.stats.labels_full.saturating_sub(1);
                }
                self.stats.save();
                let next = if idx + 1 < self.samples.len() {
                    Some(idx)
                } else if idx > 0 {
                    Some(idx - 1)
                } else {
                    None
                };
                self.sample_idx = next;
                self.loaded_sample_idx = None;
                self.sample_view = None;
                self.dataset_tex = None;
                self.feat_tex = None;
                self.dataset_dirty = true;
                self.status = format!("Deleted {}", meta.file_name);
                self.push_log(format!("Deleted {}", meta.file_name));
            }
            Err(e) => {
                self.status = format!("Delete failed: {e}");
                self.push_log(format!("Delete failed: {e}"));
            }
        }
    }
}

fn fmt_dt(unix: u64) -> String {
    match Local.timestamp_opt(unix as i64, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => unix.to_string(),
    }
}

fn fmt_time(unix: u64) -> String {
    match Local.timestamp_opt(unix as i64, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%H:%M:%S").to_string(),
        _ => unix.to_string(),
    }
}

fn fmt_opt_dt(unix: Option<u64>) -> String {
    unix.map(fmt_dt).unwrap_or_else(|| "— never —".into())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ensure_logo(ctx);
        self.load_snap_tex(ctx);
        self.pull_frame(ctx);
        self.maybe_check(ctx);

        let now: DateTime<Local> = Local::now();
        let date = now.format("%Y-%m-%d").to_string();
        let clock = now.format("%H:%M:%S").to_string();

        egui::TopBottomPanel::top("top")
            .exact_height(52.0)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(22, 28, 36))
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .stroke((1.0, Color32::from_rgb(40, 50, 64))),
            )
            .show(ctx, |ui| {
                egui::Sides::new().height(36.0).spacing(16.0).show(
                    ui,
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if let Some(logo) = &self.logo_tex {
                                ui.add(
                                    egui::Image::new(logo)
                                        .fit_to_exact_size(Vec2::splat(28.0))
                                        .bg_fill(Color32::TRANSPARENT),
                                );
                            }
                            ui.label(
                                RichText::new("Spectr")
                                    .strong()
                                    .size(18.0)
                                    .color(Color32::from_rgb(245, 246, 248)),
                            );
                            ui.label(
                                RichText::new("Vision")
                                    .size(18.0)
                                    .color(Color32::from_rgb(160, 175, 195)),
                            );
                        });
                        ui.add_space(12.0);
                        page_tab(ui, &mut self.page, Page::Monitor, "Monitor");
                        ui.add_space(4.0);
                        page_tab(ui, &mut self.page, Page::Train, "Training");
                        ui.add_space(4.0);
                        page_tab(ui, &mut self.page, Page::Dataset, "Dataset");
                        ui.add_space(4.0);
                        page_tab(ui, &mut self.page, Page::Model, "Model");
                    },
                    |ui| {
                        ui.label(
                            RichText::new(format!("{date}  {clock}"))
                                .monospace()
                                .size(13.0)
                                .strong(),
                        );
                    },
                );
            });

        egui::TopBottomPanel::bottom("status").exact_height(28.0).show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(8.0);
                ui.label(RichText::new(&self.status).size(13.0));
            });
        });

        match self.page {
            Page::Monitor => self.ui_monitor(ctx),
            Page::Train => self.ui_train(ctx),
            Page::Dataset => self.ui_dataset(ctx),
            Page::Model => self.ui_model(ctx),
        }

        ctx.request_repaint_after(Duration::from_millis(66));
    }
}

fn page_tab(ui: &mut egui::Ui, page: &mut Page, target: Page, label: &str) {
    let selected = *page == target;
    let fill = if selected {
        Color32::from_rgb(45, 70, 105)
    } else {
        Color32::TRANSPARENT
    };
    let text = if selected {
        RichText::new(label).strong().size(14.0).color(Color32::WHITE)
    } else {
        RichText::new(label)
            .size(14.0)
            .color(Color32::from_rgb(150, 160, 175))
    };
    let btn = egui::Button::new(text)
        .fill(fill)
        .corner_radius(4.0)
        .min_size(Vec2::new(88.0, 28.0));
    if ui.add(btn).clicked() {
        *page = target;
    }
}

impl App {
    fn ui_monitor(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("mon_side")
            .exact_width(300.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new("Operations").strong().size(16.0));
                ui.separator();

                let mon_label = if self.monitor {
                    "Stop monitoring"
                } else {
                    "Start monitoring"
                };
                if ui
                    .add_sized([ui.available_width(), 36.0], egui::Button::new(mon_label))
                    .clicked()
                {
                    self.monitor = !self.monitor;
                    if self.monitor {
                        self.status = "Monitoring on — level + drops".into();
                        self.push_log("Monitoring started");
                    } else {
                        self.status = "Monitoring stopped".into();
                        self.push_log("Monitoring stopped");
                    }
                }

                ui.add_space(8.0);
                section(ui, "Level");
                if let Some(snap) = &self.last_snap {
                    ui.monospace(format!("state   {}", snap.level_state.as_str()));
                    ui.monospace(format!(
                        "level   {}",
                        snap.level_percent
                            .map(|p| format!("{p:.1}%"))
                            .unwrap_or_else(|| "—".into())
                    ));
                    ui.monospace(format!("conf    {:.0}%", snap.level_confidence * 100.0));
                    ui.monospace(format!("alarm   {}", snap.alarm.as_str()));
                    if !snap.alarm_message.is_empty() {
                        ui.label(
                            RichText::new(&snap.alarm_message)
                                .size(12.0)
                                .color(match snap.alarm {
                                    AlarmLevel::Ok => Color32::from_rgb(140, 200, 140),
                                    AlarmLevel::Warning => Color32::from_rgb(230, 180, 80),
                                    AlarmLevel::Critical => Color32::from_rgb(230, 90, 90),
                                }),
                        );
                    }
                } else {
                    ui.label("Start monitoring to measure level");
                }

                ui.add_space(6.0);
                section(ui, "Drops");
                if let Some(snap) = &self.last_snap {
                    ui.monospace(format!("total     {}", snap.drop_count));
                    ui.monospace(format!("per min   {:.1}", snap.drops_per_min));
                    ui.monospace(format!("last      {}", fmt_opt_dt(snap.last_drop_unix)));
                    ui.monospace(format!(
                        "since     {}",
                        snap.secs_since_drop
                            .map(|s| format!("{s}s"))
                            .unwrap_or_else(|| "—".into())
                    ));
                    ui.monospace(format!("detector  {}", self.drop_det.state_name()));
                } else {
                    ui.monospace("—");
                }

                ui.add_space(6.0);
                section(ui, "Radio / GPIO");
                ui.monospace(fmt_opt_dt(self.stats.last_alert_unix));
                if let Some(a) = &self.alert {
                    ui.monospace(format!(
                        "gpio empty: {}",
                        if a.silo_empty() { "yes" } else { "no" }
                    ));
                } else {
                    ui.monospace("gpio: unavailable");
                }
                ui.monospace(format!("alerts sent {}", self.stats.alerts_sent));

                ui.add_space(6.0);
                section(ui, "Regions (you draw)");
                ui.label(
                    RichText::new("Tell the system where level and drops are.")
                        .size(11.0)
                        .color(Color32::GRAY),
                );
                let level_sel = self.roi_tool == RoiTool::Level;
                let drop_sel = self.roi_tool == RoiTool::Drop;
                if ui
                    .add_sized(
                        [ui.available_width(), 30.0],
                        egui::Button::new(if level_sel {
                            "Drawing LEVEL… drag on image"
                        } else {
                            "Draw LEVEL box"
                        })
                        .selected(level_sel),
                    )
                    .clicked()
                {
                    self.roi_tool = if level_sel {
                        RoiTool::None
                    } else {
                        RoiTool::Level
                    };
                    self.roi_drag_start = None;
                    self.roi_drag_cur = None;
                    if self.roi_tool == RoiTool::Level {
                        self.status = "Drag a box on the silo fill area (LEVEL)".into();
                    }
                }
                if ui
                    .add_sized(
                        [ui.available_width(), 30.0],
                        egui::Button::new(if drop_sel {
                            "Drawing DROP… drag on image"
                        } else {
                            "Draw DROP box"
                        })
                        .selected(drop_sel),
                    )
                    .clicked()
                {
                    self.roi_tool = if drop_sel {
                        RoiTool::None
                    } else {
                        RoiTool::Drop
                    };
                    self.roi_drag_start = None;
                    self.roi_drag_cur = None;
                    if self.roi_tool == RoiTool::Drop {
                        self.status = "Drag a box where granules fall in (DROP)".into();
                    }
                }
                ui.monospace(format!(
                    "LEVEL  ({},{})–({},{})",
                    self.cfg.level_roi.x1,
                    self.cfg.level_roi.y1,
                    self.cfg.level_roi.x2,
                    self.cfg.level_roi.y2
                ));
                ui.monospace(format!(
                    "DROP   ({},{})–({},{})",
                    self.cfg.drop_roi.x1,
                    self.cfg.drop_roi.y1,
                    self.cfg.drop_roi.x2,
                    self.cfg.drop_roi.y2
                ));
                ui.label(
                    RichText::new("green = level · blue = drop")
                        .size(11.0)
                        .color(Color32::GRAY),
                );

                ui.add_space(6.0);
                section(ui, "System");
                ui.monospace(format!("uptime  {}s", self.boot.elapsed().as_secs()));
                ui.label(
                    RichText::new("Training tab = optional ML labels")
                        .size(11.0)
                        .color(Color32::GRAY),
                );
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let hint = match self.roi_tool {
                RoiTool::Level => "Drag on image to set LEVEL region",
                RoiTool::Drop => "Drag on image to set DROP region",
                RoiTool::None => "Live camera · draw regions in the side panel",
            };
            ui.label(RichText::new(hint).strong());
            let h = (ui.available_height() - 180.0).max(280.0);
            let sense = if self.roi_tool != RoiTool::None {
                Sense::click_and_drag()
            } else {
                Sense::hover()
            };
            let (resp, img_rect) = frame_box(ui, &self.live_tex, h, sense);
            self.handle_roi_drag(&resp, img_rect);

            ui.add_space(8.0);
            ui.label(RichText::new("Activity").strong().size(15.0));
            ui.separator();
            let term_h = 140.0;
            let term_w = ui.available_width();
            let (term_rect, _) =
                ui.allocate_exact_size(Vec2::new(term_w, term_h), Sense::hover());
            ui.painter()
                .rect_filled(term_rect, 4.0, Color32::from_rgb(8, 12, 10));
            ui.painter().rect_stroke(
                term_rect,
                4.0,
                (1.0, Color32::from_rgb(40, 70, 50)),
                egui::StrokeKind::Inside,
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(term_rect), |ui| {
                ui.set_clip_rect(term_rect);
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if self.log.is_empty() {
                                    ui.label(
                                        RichText::new("$ waiting for events…")
                                            .monospace()
                                            .color(Color32::from_rgb(80, 140, 90)),
                                    );
                                }
                                for line in &self.log {
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 6.0;
                                        ui.label(
                                            RichText::new(format!("[{}]", fmt_time(line.unix)))
                                                .monospace()
                                                .color(Color32::from_rgb(60, 160, 90)),
                                        );
                                        ui.label(
                                            RichText::new(&line.text)
                                                .monospace()
                                                .color(Color32::from_rgb(180, 230, 180)),
                                        );
                                    });
                                }
                            });
                    });
            });
        });
    }

    fn ui_train(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("train_side")
            .exact_width(300.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new("Training ground").strong().size(16.0));
                ui.label(
                    RichText::new("Point camera at silo, label frames, then train.")
                        .size(12.0)
                        .color(Color32::GRAY),
                );
                ui.separator();

                ui.label(RichText::new("Label current live frame").strong());
                ui.horizontal(|ui| {
                    if ui
                        .add_sized([120.0, 36.0], egui::Button::new("Empty"))
                        .clicked()
                    {
                        self.save_label(true);
                    }
                    if ui
                        .add_sized([120.0, 36.0], egui::Button::new("Not empty"))
                        .clicked()
                    {
                        self.save_label(false);
                    }
                });

                ui.add_space(8.0);
                if ui
                    .add_sized([ui.available_width(), 40.0], egui::Button::new("Train model"))
                    .clicked()
                {
                    self.do_train();
                }

                ui.add_space(10.0);
                section(ui, "Dataset");
                ui.monospace(format!("empty samples  {}", self.stats.labels_empty));
                ui.monospace(format!("full samples   {}", self.stats.labels_full));
                ui.monospace(format!(
                    "total          {}",
                    self.stats.labels_empty + self.stats.labels_full
                ));

                ui.add_space(8.0);
                section(ui, "Model parameters");
                if let Some(m) = &self.model {
                    ui.monospace(format!("name       {}", m.display_name()));
                    ui.monospace("status     ready");
                    ui.monospace(format!("quality    {} ({}/100)", m.quality_label(), m.quality_score()));
                    ui.monospace(format!("accuracy   {:.1}%", m.train_accuracy * 100.0));
                    ui.monospace(format!("trained    {}", fmt_dt(m.trained_at_unix)));
                    ui.label(
                        RichText::new("Open the Model tab for full stats and rename.")
                            .size(11.0)
                            .color(Color32::GRAY),
                    );
                } else {
                    ui.label("No model on disk yet.");
                    ui.label(
                        RichText::new("Label ≥1 empty and ≥1 full, then Train.")
                            .size(12.0)
                            .color(Color32::GRAY),
                    );
                }

                if let Some(p) = self.last_pred {
                    ui.add_space(8.0);
                    section(ui, "Live probe (last eval)");
                    ui.monospace(format!(
                        "class  {}",
                        if p.empty { "EMPTY" } else { "FULL" }
                    ));
                    ui.monospace(format!("conf   {:.0}%", p.confidence() * 100.0));
                    ui.monospace(format!("dEmpty {:.3}", p.dist_empty));
                    ui.monospace(format!("dFull  {:.3}", p.dist_full));
                }
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label(RichText::new("Live view for labeling").strong());
            let h = (ui.available_height() - 8.0).max(200.0);
            frame_box(ui, &self.live_tex, h, Sense::hover());
        });
    }

    fn ui_model(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("model_side")
            .exact_width(300.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new("Model").strong().size(16.0));
                ui.label(
                    RichText::new("Name, quality, and what the classifier knows.")
                        .size(12.0)
                        .color(Color32::GRAY),
                );
                ui.separator();

                section(ui, "Name");
                ui.add(
                    egui::TextEdit::singleline(&mut self.model_name_draft)
                        .desired_width(ui.available_width())
                        .hint_text("e.g. Spectr Silo A"),
                );
                ui.add_space(4.0);
                let can_save = self.model.is_some();
                if ui
                    .add_enabled(
                        can_save,
                        egui::Button::new("Save name").min_size(Vec2::new(ui.available_width(), 32.0)),
                    )
                    .clicked()
                {
                    self.save_model_name();
                }
                if !can_save {
                    ui.label(
                        RichText::new("Train a model first to set a name.")
                            .size(11.0)
                            .color(Color32::GRAY),
                    );
                }

                ui.add_space(10.0);
                if ui
                    .add_sized([ui.available_width(), 36.0], egui::Button::new("Train / retrain"))
                    .clicked()
                {
                    self.do_train();
                }
                if ui
                    .add_sized([ui.available_width(), 28.0], egui::Button::new("Reload from disk"))
                    .clicked()
                {
                    self.reload_model();
                }

                ui.add_space(10.0);
                section(ui, "Dataset on disk");
                ui.monospace(format!("empty  {}", self.stats.labels_empty));
                ui.monospace(format!("full   {}", self.stats.labels_full));
                ui.monospace(format!(
                    "total  {}",
                    self.stats.labels_empty + self.stats.labels_full
                ));
                ui.monospace(format!("file   {}", vision::MODEL_PATH));
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    if let Some(m) = &self.model {
                        let score = m.quality_score();
                        let label = m.quality_label();
                        let grade_col = match label {
                            "Excellent" => Color32::from_rgb(90, 200, 130),
                            "Good" => Color32::from_rgb(140, 200, 120),
                            "Fair" => Color32::from_rgb(220, 180, 80),
                            "Weak" => Color32::from_rgb(230, 140, 70),
                            _ => Color32::from_rgb(220, 90, 90),
                        };

                        ui.label(
                            RichText::new(m.display_name())
                                .strong()
                                .size(28.0)
                                .color(Color32::from_rgb(245, 246, 248)),
                        );
                        ui.label(
                            RichText::new("Nearest-centroid classifier · 32×32 grayscale")
                                .size(13.0)
                                .color(Color32::from_rgb(150, 165, 185)),
                        );
                        ui.add_space(10.0);

                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(label)
                                    .strong()
                                    .size(22.0)
                                    .color(grade_col),
                            );
                            ui.add_space(12.0);
                            ui.label(
                                RichText::new(format!("{score} / 100"))
                                    .strong()
                                    .size(22.0)
                                    .monospace(),
                            );
                        });
                        ui.add_space(4.0);
                        let blurb = m.quality_blurb();
                        ui.label(RichText::new(blurb).size(14.0));

                        ui.add_space(14.0);
                        section(ui, "How good it is");
                        ui.monospace(format!(
                            "train accuracy     {:.1}%  (fit on {} labeled images)",
                            m.train_accuracy * 100.0,
                            m.n_empty + m.n_full
                        ));
                        ui.monospace(format!("class separation   {:.3}", m.separation()));
                        ui.monospace(format!(
                            "brightness empty   {:.3}   full {:.3}",
                            m.mean_brightness_empty(),
                            m.mean_brightness_full()
                        ));
                        let bright_gap =
                            (m.mean_brightness_empty() - m.mean_brightness_full()).abs();
                        ui.monospace(format!("brightness gap     {:.3}", bright_gap));

                        ui.add_space(10.0);
                        section(ui, "Training recipe");
                        ui.monospace(format!("trained at         {}", fmt_dt(m.trained_at_unix)));
                        ui.monospace(format!("empty images used  {}", m.n_empty));
                        ui.monospace(format!("full images used   {}", m.n_full));
                        ui.monospace(format!(
                            "feature grid       {}×{} ({} values)",
                            m.w,
                            m.h,
                            m.empty.len()
                        ));
                        ui.monospace("algorithm          nearest centroid (EMPTY vs FULL)".to_string());

                        ui.add_space(10.0);
                        section(ui, "What this means");
                        ui.label(
                            RichText::new(
                                "Each frame is shrunk to 32×32 grayscale. The model stores the average \
                                 Empty look and the average Full look. A new frame is labeled by which \
                                 average it is closer to. Higher accuracy and separation = clearer Empty vs Full.",
                            )
                            .size(13.0),
                        );

                        if let Some(p) = self.last_pred {
                            ui.add_space(10.0);
                            section(ui, "Last live probe");
                            ui.monospace(format!(
                                "class       {}",
                                if p.empty { "EMPTY" } else { "FULL" }
                            ));
                            ui.monospace(format!("confidence  {:.0}%", p.confidence() * 100.0));
                            ui.monospace(format!("d → empty   {:.3}", p.dist_empty));
                            ui.monospace(format!("d → full    {:.3}", p.dist_full));
                        }

                        if self.stats.last_train_accuracy.is_some() {
                            ui.add_space(10.0);
                            section(ui, "Session stats");
                            if let Some(acc) = self.stats.last_train_accuracy {
                                ui.monospace(format!(
                                    "last train accuracy  {:.1}%",
                                    acc * 100.0
                                ));
                            }
                            ui.monospace(format!(
                                "last train time      {}",
                                fmt_opt_dt(self.stats.last_train_unix)
                            ));
                            ui.monospace(format!(
                                "labels empty (disk)  {}",
                                self.stats.labels_empty
                            ));
                            ui.monospace(format!(
                                "labels full (disk)   {}",
                                self.stats.labels_full
                            ));
                        }
                    } else {
                        ui.centered_and_justified(|ui| {
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new("No model yet")
                                        .strong()
                                        .size(22.0),
                                );
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(
                                        "Go to Training, label at least one Empty and one Full frame, then Train.",
                                    )
                                    .size(14.0)
                                    .color(Color32::GRAY),
                                );
                                ui.add_space(12.0);
                                ui.label(
                                    RichText::new("Default name when first trained: Spectr Silo")
                                        .size(12.0)
                                        .color(Color32::from_rgb(150, 165, 185)),
                                );
                            });
                        });
                    }
                });
        });
    }

    fn save_model_name(&mut self) {
        let Some(mut m) = self.model.clone() else {
            self.status = "No model to rename".into();
            return;
        };
        let name = self.model_name_draft.trim().to_string();
        if name.is_empty() {
            self.status = "Name cannot be empty".into();
            return;
        }
        m.name = name.clone();
        match m.save(vision::MODEL_PATH) {
            Ok(()) => {
                self.model_name_draft = name.clone();
                self.model = Some(m);
                self.status = format!("Model renamed to “{name}”");
                self.push_log(format!("Model name → {name}"));
            }
            Err(e) => {
                self.status = format!("Save name failed: {e}");
                self.push_log(format!("Save name failed: {e}"));
            }
        }
    }

    fn reload_model(&mut self) {
        match Model::load(vision::MODEL_PATH) {
            Some(m) => {
                self.model_name_draft = m.display_name().to_string();
                self.status = format!("Loaded model “{}”", m.display_name());
                self.push_log(format!("Reloaded model · {}", m.display_name()));
                self.model = Some(m);
                self.dataset_dirty = true;
            }
            None => {
                self.model = None;
                self.model_name_draft = "Spectr Silo".into();
                self.status = "No model file on disk".into();
                self.push_log("Reload model: none found");
            }
        }
    }

    fn ui_dataset(&mut self, ctx: &egui::Context) {
        if self.dataset_dirty {
            self.refresh_dataset_list();
        }

        let mut clicked = None;
        egui::SidePanel::left("dataset_list")
            .exact_width(260.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new("Training images").strong().size(16.0));
                ui.label(
                    RichText::new(format!("{} samples", self.samples.len()))
                        .size(12.0)
                        .color(Color32::GRAY),
                );
                if ui.button("Refresh").clicked() {
                    self.dataset_dirty = true;
                }
                ui.separator();

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.samples.is_empty() {
                            ui.label("No labeled images yet.");
                            ui.label("Use Training to add Empty / Not empty frames.");
                            return;
                        }
                        for (i, s) in self.samples.iter().enumerate() {
                            let label = if s.label_empty { "EMPTY" } else { "FULL" };
                            let selected = self.sample_idx == Some(i);
                            let text = format!("{label}  {}", s.file_name);
                            if ui.selectable_label(selected, text).clicked() {
                                clicked = Some(i);
                            }
                        }
                    });
            });

        if let Some(i) = clicked {
            self.select_sample(ctx, i);
        } else if self.sample_idx != self.loaded_sample_idx {
            if let Some(i) = self.sample_idx {
                self.select_sample(ctx, i);
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(idx) = self.sample_idx.filter(|i| *i < self.samples.len()) else {
                ui.centered_and_justified(|ui| {
                    ui.label("Select a training image on the left.");
                });
                return;
            };

            let meta = self.samples[idx].clone();
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    ui.label(RichText::new("Image & model reasoning").strong().size(16.0));
                    ui.separator();

                    // Fixed-height columns — nested verticals otherwise expand to
                    // the full scroll viewport and leave a huge gap under the images.
                    let gap = 8.0;
                    let img_h = 240.0;
                    let block_h = img_h + 22.0;
                    ui.horizontal_top(|ui| {
                        let col_w = ((ui.available_width() - gap) / 2.0).max(120.0);
                        ui.allocate_ui_with_layout(
                            Vec2::new(col_w, block_h),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                ui.label(RichText::new("Original").strong());
                                frame_box(ui, &self.dataset_tex, img_h, Sense::hover());
                            },
                        );
                        ui.add_space(gap);
                        ui.allocate_ui_with_layout(
                            Vec2::new(col_w, block_h),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                ui.label(RichText::new("What the model sees (32×32)").strong());
                                frame_box(ui, &self.feat_tex, img_h, Sense::hover());
                            },
                        );
                    });

                    ui.horizontal(|ui| {
                        let del = egui::Button::new(
                            RichText::new("Delete image")
                                .strong()
                                .color(Color32::from_rgb(255, 200, 200)),
                        )
                        .fill(Color32::from_rgb(120, 40, 40))
                        .min_size(Vec2::new(120.0, 28.0));
                        if ui.add(del).clicked() {
                            self.delete_selected_sample();
                        }
                        ui.label(
                            RichText::new("Removes this file from data/empty or data/full")
                                .size(12.0)
                                .color(Color32::GRAY),
                        );
                    });

                    section(ui, "Image data");
                    ui.monospace(format!("file     {}", meta.file_name));
                    ui.monospace(format!("path     {}", meta.path.display()));
                    ui.monospace(format!(
                        "label    {}",
                        if meta.label_empty {
                            "EMPTY"
                        } else {
                            "FULL / not empty"
                        }
                    ));
                    ui.monospace(format!("bytes    {}", meta.bytes));
                    if let Some(ms) = meta.captured_ms {
                        ui.monospace(format!("captured {}", fmt_dt(ms / 1000)));
                    }

                    if let Some(v) = &self.sample_view {
                        ui.monospace(format!("size     {}×{}", v.width, v.height));
                        ui.monospace(format!("mean Y   {:.3}", v.mean_brightness));
                        ui.monospace(format!("features {}", v.features.len()));

                        section(ui, "Model understanding");
                        if let Some(p) = v.pred {
                            let guess = if p.empty { "EMPTY" } else { "FULL" };
                            let (match_txt, col) = match v.agrees {
                                Some(true) => ("AGREES with label", Color32::from_rgb(100, 200, 120)),
                                Some(false) => (
                                    "DISAGREES with label",
                                    Color32::from_rgb(220, 100, 100),
                                ),
                                None => ("", Color32::GRAY),
                            };
                            ui.label(
                                RichText::new(format!(
                                    "Prediction: {guess}  ·  confidence {:.0}%  ·  {match_txt}",
                                    p.confidence() * 100.0
                                ))
                                .strong()
                                .color(col),
                            );
                            ui.monospace(format!(
                                "distance → EMPTY centroid  {:.3}",
                                p.dist_empty
                            ));
                            ui.monospace(format!(
                                "distance → FULL centroid   {:.3}",
                                p.dist_full
                            ));
                        } else {
                            ui.label("No model loaded — train first to see distances.");
                        }

                        section(ui, "Reasoning");
                        ui.label(RichText::new(&v.reasoning).size(13.0));
                    }
                });
        });
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).strong().color(Color32::from_rgb(160, 190, 220)));
}

fn frame_box(
    ui: &mut egui::Ui,
    tex: &Option<egui::TextureHandle>,
    height: f32,
    sense: Sense,
) -> (egui::Response, egui::Rect) {
    let w = ui.available_width();
    let size = Vec2::new(w, height);
    let (rect, response) = ui.allocate_exact_size(size, sense);
    ui.painter()
        .rect_filled(rect, 4.0, Color32::from_rgb(18, 22, 28));
    let mut img_rect = rect;
    if let Some(t) = tex {
        let img_size = t.size_vec2();
        let scale = (rect.width() / img_size.x).min(rect.height() / img_size.y);
        let draw = img_size * scale;
        img_rect = egui::Rect::from_center_size(rect.center(), draw);
        ui.painter().image(
            t.id(),
            img_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "waiting for camera…",
            egui::FontId::proportional(14.0),
            Color32::GRAY,
        );
    }
    ui.painter()
        .rect_stroke(rect, 4.0, (1.0, Color32::from_rgb(50, 60, 75)), egui::StrokeKind::Inside);
    (response, img_rect)
}

fn draw_roi_rect(rgb: &mut [u8], w: u32, x1: i32, y1: i32, x2: i32, y2: i32, color: [u8; 3]) {
    let h = (rgb.len() as u32 / (w * 3)).max(1);
    let x1 = x1.clamp(0, w as i32 - 1) as u32;
    let x2 = x2.clamp(0, w as i32) as u32;
    let y1 = y1.clamp(0, h as i32 - 1) as u32;
    let y2 = y2.clamp(0, h as i32) as u32;
    for x in x1..x2 {
        put_px(rgb, w, x, y1, color);
        if y2 > 0 {
            put_px(rgb, w, x, y2 - 1, color);
        }
    }
    for y in y1..y2 {
        put_px(rgb, w, x1, y, color);
        if x2 > 0 {
            put_px(rgb, w, x2 - 1, y, color);
        }
    }
}

fn draw_hline(rgb: &mut [u8], w: u32, h: u32, x1: i32, x2: i32, y: i32, color: [u8; 3]) {
    if y < 0 || y as u32 >= h {
        return;
    }
    let y = y as u32;
    let x1 = x1.clamp(0, w as i32 - 1) as u32;
    let x2 = x2.clamp(0, w as i32) as u32;
    for x in x1..x2 {
        put_px(rgb, w, x, y, color);
        if y + 1 < h {
            put_px(rgb, w, x, y + 1, color);
        }
    }
}

fn put_px(rgb: &mut [u8], w: u32, x: u32, y: u32, color: [u8; 3]) {
    let i = ((y * w + x) * 3) as usize;
    if i + 2 < rgb.len() {
        rgb[i] = color[0];
        rgb[i + 1] = color[1];
        rgb[i + 2] = color[2];
    }
}

fn main() -> eframe::Result {
    if std::env::var_os("WINIT_UNIX_BACKEND").is_none() {
        // Safety: single-threaded before any other threads touch env.
        unsafe { std::env::set_var("WINIT_UNIX_BACKEND", "x11") };
    }

    let cam = match camera::Cam::open() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("camera open failed (continuing without camera): {e}");
            None
        }
    };
    eprintln!("opening UI window — look on the Pi desktop (close window to quit)");

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 720.0])
            .with_title("Spectr Vision"),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    let result = eframe::run_native(
        "Spectr Vision",
        opts,
        Box::new(move |_cc| Ok(Box::new(App::new(cam)))),
    );
    eprintln!("UI closed.");
    result
}
