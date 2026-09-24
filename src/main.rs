mod camera;
mod config;
mod eventlog;
mod peltor;
mod sa828;
mod silo_alert;
mod stats;
mod supabase;
mod vision;

use chrono::{Local, TimeZone};
use config::Config;
use eframe::egui::{self, Color32, CornerRadius, Pos2, RichText, Sense, Stroke, Vec2};
use eventlog::EventLog;
use silo_alert::SiloAlert;
use stats::Stats;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use supabase::Supabase;
use vision::{BoxNorm, Reference, SampleAnalysis, SampleMeta};

const BG: Color32 = Color32::from_rgb(0, 0, 0);
const CARD: Color32 = Color32::from_rgb(28, 28, 30);
const TEXT: Color32 = Color32::from_rgb(245, 245, 247);
const MUTED: Color32 = Color32::from_rgb(142, 142, 147);
const BLUE: Color32 = Color32::from_rgb(10, 132, 255);
const RED: Color32 = Color32::from_rgb(255, 69, 58);
const FILL: Color32 = Color32::from_rgb(22, 48, 84);
const BTN: Color32 = Color32::from_rgb(44, 44, 46);
const VIDEO_BG: Color32 = Color32::from_rgb(10, 10, 12);
const HAIRLINE: Color32 = Color32::from_rgb(58, 58, 60);
const NAV_H: f32 = 64.0;
const LOGO_CELL: f32 = 72.0;
const LOGO_MARK: f32 = 34.0;
const TERM_MAX: usize = 48;
const TERM_BAR_H: f32 = 32.0;
/// Frames may pause briefly while an RTSP stream reconnects; only call the
/// camera down after this much silence.
const CAM_GRACE: Duration = Duration::from_secs(12);
const CAM_RETRY_EVERY: Duration = Duration::from_secs(10);
/// Rotate the text log past this size (2 MB) so months of uptime stay bounded.
const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Live,
    Model,
    Stats,
    Config,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfigTab {
    Detection,
    Camera,
    Radio,
    App,
}

/// One terminal line: text, plus the frame it describes when there is one.
struct TermEntry {
    text: String,
    thumb: Option<egui::TextureHandle>,
}

struct App {
    cam: Option<camera::Cam>,
    live_tex: Option<egui::TextureHandle>,
    last_rgb: Option<(u32, u32, Vec<u8>)>,
    /// Photos of the empty silo, and how well the last frame matched them.
    reference: Option<Reference>,
    last_match: Option<f32>,
    alert: Option<SiloAlert>,
    stats: Stats,
    monitor: bool,
    page: Page,
    cfg: Config,
    events: EventLog,
    empty_alerts: Vec<u64>,
    empty_since: Option<u64>,
    empty: bool,
    /// Consecutive non-empty checks while sticky-empty (filled confirm).
    full_streak: u32,
    full_since: Option<u64>,
    note: String,
    samples: Vec<SampleMeta>,
    sample_idx: Option<usize>,
    sample_tex: Option<egui::TextureHandle>,
    sample_view: Option<SampleAnalysis>,
    dataset_dirty: bool,
    thumb_tex: std::collections::HashMap<String, egui::TextureHandle>,
    logo_tex: Option<egui::TextureHandle>,
    radio_freq: String,
    cloud: Option<Supabase>,
    /// Last successful Stats pull from Supabase.
    last_cloud_pull: Option<Instant>,
    /// Last time we compared a frame against the reference.
    last_check_at: Option<Instant>,
    /// Short process log (ring buffer), each line optionally showing the
    /// picture the comparison ran on.
    terminal: VecDeque<TermEntry>,
    /// Camera health — last good frame, downtime, next reopen attempt.
    last_frame_at: Option<Instant>,
    cam_down_since: Option<Instant>,
    cam_retry_at: Option<Instant>,
    warned_cam_open: bool,
    started_at: Instant,
    config_tab: ConfigTab,
    /// Second Live pane showing what the comparison actually works on.
    show_model_view: bool,
    model_tex: Option<egui::TextureHandle>,
    /// The one watched region, marked on Live, plus the in-progress drag.
    region: Option<BoxNorm>,
    marking_region: bool,
    box_drag_start: Option<(f32, f32)>,
    box_drag_now: Option<(f32, f32)>,
    /// Logged once per state change so the terminal never repeats itself.
    warned_no_reference: bool,
    /// Rolling on-disk copy of the terminal, for after-the-fact debugging.
    log_path: std::path::PathBuf,
    /// Operator must arm after setup. Pause is separate and persisted.
    armed: bool,
    empty_streak: u32,
    radio_fault: bool,
    last_heartbeat: Option<Instant>,
    evidence_tex: Option<egui::TextureHandle>,
    evidence_path: Option<std::path::PathBuf>,
}

impl App {
    fn new(cam: Option<camera::Cam>) -> Self {
        vision::ensure_dirs();
        let cfg = Config::load();
        let mut stats = Stats::load();
        let reference = Reference::load();
        stats.labels_empty = vision::list_references().len() as u64;
        stats.labels_full = vision::list_full_samples().len() as u64;
        if reference.is_none() {
            stats.last_train_unix = None;
            stats.last_train_accuracy = None;
        }
        silo_alert::set_ptt_pin(cfg.radio.ptt_gpio);
        silo_alert::set_audio_device(&cfg.radio.audio_device);
        silo_alert::set_muted(cfg.radio.muted);
        let mut alert = SiloAlert::new();
        // Already-empty after a reboot must not look like a fresh empty.
        alert.restore(stats.empty_state, stats.last_alert_unix);
        let log_path = cfg.resolve_db_path().with_extension("jsonl");
        let text_log = cfg.resolve_db_path().with_extension("log");
        let mut empty_alerts = EventLog::load_empty_alerts(&log_path);
        let cloud = Supabase::from_cfg(&cfg.supabase);
        let mut last_cloud_pull = None;
        if let Some(ref sb) = cloud {
            eprintln!("supabase: enabled site={}", sb.site_id());
            match sb.fetch_empty_alerts(500) {
                Ok(remote) if !remote.is_empty() => {
                    empty_alerts = merge_alert_ts(remote, empty_alerts);
                    last_cloud_pull = Some(Instant::now());
                    eprintln!("supabase: loaded {} alerts from cloud", empty_alerts.len());
                }
                Ok(_) => eprintln!("supabase: no remote alerts yet"),
                Err(e) => eprintln!("supabase: pull {e}"),
            }
        } else {
            eprintln!("supabase: disabled (set supabase.enabled + key, or SUPABASE_KEY)");
        }
        let resume_empty = stats.empty_state;
        let resume_empty_since = stats.empty_since_unix;
        let armed = stats.armed;
        let monitor = !stats.monitor_paused;
        let radio_fault = !stats.last_radio_ok && stats.last_radio_err.is_some();
        if let Some(ref sb) = cloud {
            sb.push_event("app_start", Some(resume_empty), None);
        }
        // Channel is source of truth; heal frequency from nearest Peltor ch.
        let mut cfg = cfg;
        if cfg.radio.channel == 0 || cfg.radio.channel > 16 {
            cfg.radio.channel = peltor::channel_for_freq(&cfg.radio.frequency_mhz);
        }
        cfg.radio.channel = peltor::clamp_channel(cfg.radio.channel);
        cfg.radio.frequency_mhz = peltor::sa828_freq_for_channel(cfg.radio.channel);
        cfg.radio.ctcss = peltor::clamp_ctcss(cfg.radio.ctcss);
        let radio_freq = cfg.radio.frequency_mhz.clone();
        Self {
            cam,
            live_tex: None,
            last_rgb: None,
            reference,
            last_match: None,
            alert: Some(alert),
            stats,
            monitor,
            page: Page::Live,
            events: EventLog::open(log_path),
            empty_alerts,
            empty_since: resume_empty_since,
            empty: resume_empty,
            full_streak: 0,
            full_since: None,
            note: String::new(),
            samples: vision::list_references(),
            sample_idx: None,
            sample_tex: None,
            sample_view: None,
            dataset_dirty: false,
            thumb_tex: std::collections::HashMap::new(),
            logo_tex: None,
            radio_freq,
            cloud,
            last_cloud_pull,
            cfg,
            last_check_at: None,
            terminal: VecDeque::new(),
            last_frame_at: None,
            cam_down_since: None,
            cam_retry_at: None,
            warned_cam_open: false,
            started_at: Instant::now(),
            config_tab: ConfigTab::Detection,
            show_model_view: true,
            model_tex: None,
            region: vision::load_region(),
            marking_region: false,
            box_drag_start: None,
            box_drag_now: None,
            warned_no_reference: false,
            log_path: text_log,
            armed,
            empty_streak: 0,
            radio_fault,
            last_heartbeat: None,
            evidence_tex: None,
            evidence_path: vision::latest_alert_evidence(),
        }
    }

    fn ensure_logo(&mut self, ctx: &egui::Context) {
        if self.logo_tex.is_some() {
            return;
        }
        const LOGO: &[u8] = include_bytes!("brand/spectr-mark.jpg");
        let Ok(img) = image::load_from_memory(LOGO) else {
            return;
        };
        let mut rgba = img.to_rgba8();
        for p in rgba.pixels_mut() {
            let [r, g, b, _] = p.0;
            let a = ((0.299 * r as f32) + (0.587 * g as f32) + (0.114 * b as f32)).round() as u8;
            p.0 = [255, 255, 255, a];
        }
        let size = [rgba.width() as usize, rgba.height() as usize];
        let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
        self.logo_tex = Some(ctx.load_texture("spectr_mark", color, Default::default()));
    }

    /// Grab a frame, healing the camera if it dropped out, then run the
    /// scheduled check. Runs on every page — monitoring never pauses because
    /// an operator opened Train or Config.
    fn pull_frame(&mut self, ctx: &egui::Context) {
        match self.cam.as_mut().and_then(|c| c.frame()) {
            Some((w, h, rgb)) => {
                if self.cam_down_since.is_some() {
                    self.term_line(Self::term_now("camera  reconnected"));
                    if let Some(cloud) = &self.cloud {
                        cloud.push_event("camera_up", None, None);
                    }
                    // Do not restore EMPTY — confirm again from a live stream.
                    self.empty = false;
                    self.empty_since = None;
                    self.empty_streak = 0;
                    self.full_since = None;
                    self.full_streak = 0;
                    self.stats.empty_state = false;
                    self.stats.empty_since_unix = None;
                    self.stats.save();
                    // Do not clear radio announce latch — RTSP flaps must not re-TX.
                }
                self.last_frame_at = Some(Instant::now());
                self.cam_down_since = None;
                self.cam_retry_at = None;
                self.warned_cam_open = false;

                let img = egui::ColorImage::from_rgb([w as usize, h as usize], &rgb);
                match &mut self.live_tex {
                    Some(t) => t.set(img, Default::default()),
                    None => {
                        self.live_tex =
                            Some(ctx.load_texture("live", img, Default::default()))
                    }
                }
                self.last_rgb = Some((w, h, rgb));
            }
            None => self.handle_camera_loss(),
        }


        if !self.monitor {
            return;
        }
        let interval = self.cfg.level.check_interval_seconds.max(1.0);
        let due = self
            .last_check_at
            .map(|t| t.elapsed().as_secs_f32() >= interval)
            .unwrap_or(true);
        if !due {
            return;
        }
        // Only analyze a frame we actually captured this cycle.
        if self.cam_down_since.is_some() {
            self.last_check_at = Some(Instant::now());
            return;
        }
        if let Some((w, h, rgb)) = self.last_rgb.clone() {
            self.run_check(ctx, w, h, &rgb);
        }
    }

    /// A dead camera must never look like a healthy silo: mark it down, stop
    /// trusting the last frame, and keep trying to reopen the stream.
    ///
    /// Health is time-based, not attempt-based, because an RTSP stream is
    /// legitimately frameless for a second or two while it connects.
    fn handle_camera_loss(&mut self) {
        let since_ok = self.last_frame_at.map(|t| t.elapsed());
        let grace = match since_ok {
            Some(d) => d >= CAM_GRACE,
            None => self.started_at.elapsed() >= CAM_GRACE,
        };
        if !grace {
            return;
        }

        if self.cam_down_since.is_none() {
            self.cam_down_since = Some(Instant::now());
            self.last_rgb = None;
            self.last_match = None;
            self.empty_since = None;
            self.empty_streak = 0;
            self.full_since = None;
            self.full_streak = 0;
            // A dead camera must never look like a healthy silo.
            self.empty = false;
            self.stats.empty_state = false;
            self.stats.empty_since_unix = None;
            self.stats.save();
            // Keep announce latch — a dead camera is not "silo filled".
            self.term_line(Self::term_now("camera  no signal — reconnecting"));
            if let Some(cloud) = &self.cloud {
                cloud.push_event("camera_down", None, None);
            }
        }

        let retry_due = self
            .cam_retry_at
            .map(|t| Instant::now() >= t)
            .unwrap_or(true);
        if !retry_due {
            return;
        }
        self.cam_retry_at = Some(Instant::now() + CAM_RETRY_EVERY);
        // Dropping first tears down the old ffmpeg process.
        self.cam = None;
        match camera::Cam::open(&self.cfg.camera) {
            Ok(c) => self.cam = Some(c),
            Err(e) => {
                if !self.warned_cam_open {
                    self.warned_cam_open = true;
                    self.term_line(Self::term_now(&format!("camera  {e}")));
                }
            }
        }
    }

    /// IDE-style output panel pinned to the bottom of the window.
    fn draw_terminal_panel(&mut self, ctx: &egui::Context) {
        let interval = self.cfg.level.check_interval_seconds.max(1.0);
        let next_in = self
            .last_check_at
            .map(|t| (interval - t.elapsed().as_secs_f32()).max(0.0).ceil() as u32)
            .unwrap_or(0);

        let _ = (interval, next_in);

        egui::TopBottomPanel::bottom("terminal")
            .resizable(true)
            .default_height(180.0)
            .min_height(TERM_BAR_H)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(18, 18, 20))
                    .inner_margin(egui::Margin::ZERO),
            )
            .show_separator_line(false)
            .show(ctx, |ui| {
                let full = ui.max_rect();
                ui.painter()
                    .hline(full.x_range(), full.top() + 0.5, Stroke::new(1.0_f32, HAIRLINE));

                // Drag handle — resize / expand / minimize by dragging this edge.
                let grip = egui::Rect::from_min_size(
                    Pos2::new(full.center().x - 18.0, full.top() + 5.0),
                    Vec2::new(36.0, 3.0),
                );
                ui.painter()
                    .rect_filled(grip, CornerRadius::same(2), HAIRLINE);

                ui.add_space(12.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.add_space(2.0);
                        if self.terminal.is_empty() {
                            ui.horizontal(|ui| {
                                ui.add_space(16.0);
                                ui.label(
                                    RichText::new("waiting for first check…")
                                        .size(12.0)
                                        .color(MUTED)
                                        .monospace(),
                                );
                            });
                        }
                        for entry in &self.terminal {
                            let color = if entry.text.contains("alert") {
                                RED
                            } else if entry.text.contains("confirmed")
                                || entry.text.contains("filled")
                            {
                                BLUE
                            } else {
                                TEXT
                            };
                            ui.horizontal(|ui| {
                                ui.add_space(16.0);
                                // Keep text aligned whether or not there is a picture.
                                let slot = Vec2::new(26.0, 15.0);
                                let (r, _) = ui.allocate_exact_size(slot, Sense::hover());
                                if let Some(tex) = entry.thumb.as_ref() {
                                    let s = tex.size_vec2();
                                    let scale =
                                        (slot.x / s.x).min(slot.y / s.y).max(0.01);
                                    let draw = s * scale;
                                    let img = egui::Rect::from_center_size(r.center(), draw);
                                    let uv = egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    );
                                    ui.painter().image(tex.id(), img, uv, Color32::WHITE);
                                }
                                ui.add_space(8.0);
                                ui.label(
                                    RichText::new(&entry.text)
                                        .size(12.0)
                                        .color(color)
                                        .monospace(),
                                );
                            });
                        }
                        ui.add_space(6.0);
                    });
            });
    }

    /// When and how strictly the app compares frames to the empty reference.
    fn config_detection(&mut self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new(
                "Every check compares the marked region against the photos of the empty silo.",
            )
            .size(13.0)
            .color(MUTED),
        );
        ui.add_space(14.0);
        labeled_slider(
            ui,
            "Check every",
            &mut self.cfg.level.check_interval_seconds,
            5.0..=120.0,
            "s",
        );
        labeled_slider(
            ui,
            "State must last",
            &mut self.cfg.level.empty_confirmation_seconds,
            15.0..=120.0,
            "s",
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Applies both ways: empty before alert, and full again before the radio latch clears.",
            )
            .size(11.0)
            .color(MUTED),
        );

        ui.add_space(10.0);
        ui.label(RichText::new("Match threshold").size(12.0).color(MUTED));
        let base_suggested = self
            .reference
            .as_ref()
            .map(|r| r.suggested_threshold())
            .unwrap_or(0.95);
        let effective = self.match_threshold();
        let mut pct = if self.cfg.level.empty_match_threshold > 0.0 {
            self.cfg.level.empty_match_threshold * 100.0
        } else {
            effective * 100.0
        };
        if ui
            .add(egui::Slider::new(&mut pct, 70.0..=99.5).suffix(" %"))
            .changed()
        {
            self.cfg.level.empty_match_threshold = pct / 100.0;
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!(
                "Suggested {:.0}% from empty photos — effective {:.0}% (full samples can raise it).",
                base_suggested * 100.0,
                effective * 100.0
            ))
            .size(11.0)
            .color(MUTED),
        );
        if let Some(warn) = self.reference.as_ref().and_then(|r| r.cohesion_warning()) {
            ui.add_space(4.0);
            ui.label(RichText::new(warn).size(11.0).color(RED));
        }
        if let Some(warn) = self
            .reference
            .as_ref()
            .and_then(|r| vision::full_sample_warning(r, self.base_match_threshold()))
        {
            ui.add_space(4.0);
            ui.label(RichText::new(warn).size(11.0).color(RED));
        }
        if self.cfg.level.empty_match_threshold > 0.0 {
            ui.add_space(4.0);
            if quiet_btn(ui, "Auto").clicked() {
                self.cfg.level.empty_match_threshold = 0.0;
            }
        }

        ui.add_space(14.0);
        ui.label(RichText::new("Reference").size(12.0).color(MUTED));
        ui.add_space(8.0);
        match self.reference.as_ref() {
            Some(r) => {
                row_stat(ui, "Photos", &r.count().to_string());
                row_stat(
                    ui,
                    "Photos agree",
                    &format!("{:.0}%", r.cohesion * 100.0),
                );
                if let Some(warn) = r.cohesion_warning() {
                    ui.add_space(4.0);
                    ui.label(RichText::new(warn).size(11.0).color(RED));
                }
                row_stat(ui, "Built", &fmt_dt(r.built_at_unix));
                row_stat(
                    ui,
                    "Region",
                    if r.roi.is_some() { "Marked" } else { "Whole frame" },
                );
            }
            None => {
                ui.label(
                    RichText::new("No reference yet — add photos on Live.")
                        .size(12.0)
                        .color(RED),
                );
            }
        }
        if self.region_changed() {
            ui.add_space(6.0);
            ui.label(
                RichText::new("Region changed — rebuild the reference to apply it.")
                    .size(12.0)
                    .color(RED),
            );
        }
        ui.add_space(10.0);
        if pill(ui, "Rebuild reference").clicked() {
            self.rebuild_reference();
        }
    }

    /// Ethernet camera over RTSP.
    fn config_camera(&mut self, ui: &mut egui::Ui) {
        self.cfg.camera.source = "rtsp".into();
        ui.label(RichText::new("RTSP URL").size(12.0).color(MUTED));
        ui.add(
            egui::TextEdit::singleline(&mut self.cfg.camera.rtsp_url)
                .desired_width(ui.available_width())
                .hint_text("rtsp://user:pass@192.168.1.50:554/stream1"),
        );

        ui.add_space(10.0);
        ui.label(RichText::new("Resolution").size(12.0).color(MUTED));
        ui.horizontal(|ui| {
            let mut w = self.cfg.camera.width as f32;
            let mut h = self.cfg.camera.height as f32;
            ui.add(egui::DragValue::new(&mut w).range(160.0..=1920.0).speed(16));
            ui.label(RichText::new("×").size(13.0).color(MUTED));
            ui.add(egui::DragValue::new(&mut h).range(120.0..=1080.0).speed(16));
            self.cfg.camera.width = w as u32;
            self.cfg.camera.height = h as u32;
        });

        ui.add_space(10.0);
        let mut fps = self.cfg.camera.fps as f32;
        ui.label(RichText::new("Decode rate").size(12.0).color(MUTED));
        ui.add(egui::Slider::new(&mut fps, 1.0..=15.0).integer().suffix(" fps"));
        self.cfg.camera.fps = fps as u32;
        ui.add_space(4.0);
        ui.label(
            RichText::new("Low is fine — the model only looks each check interval.")
                .size(11.0)
                .color(MUTED),
        );

        ui.add_space(14.0);
        ui.label(RichText::new("Status").size(12.0).color(MUTED));
        ui.add_space(8.0);
        let status = if self.cam.is_none() {
            "Not connected"
        } else if self.cam_down_since.is_some() {
            "No signal — reconnecting"
        } else if self.last_frame_at.is_some() {
            "Streaming"
        } else {
            "Connecting…"
        };
        row_stat(ui, "Camera", status);
        row_stat(ui, "Source", "Ethernet (RTSP)");

        ui.add_space(12.0);
        if pill(ui, "Reconnect camera").clicked() {
            self.reconnect_camera();
        }
    }

    /// SA828 intercom settings matched to the Peltor LiteCom Pro III headset.
    fn config_radio(&mut self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new("Peltor LiteCom Pro III — analog PMR446")
                .size(12.0)
                .color(MUTED),
        );
        ui.add_space(10.0);

        // Sync channel from stored frequency if needed.
        self.cfg.radio.channel = peltor::clamp_channel(self.cfg.radio.channel);
        if self.cfg.radio.channel == 0 {
            self.cfg.radio.channel = peltor::channel_for_freq(&self.cfg.radio.frequency_mhz);
        }

        ui.label(RichText::new("Channel").size(12.0).color(MUTED));
        let mut ch = self.cfg.radio.channel;
        egui::ComboBox::from_id_salt("peltor_channel")
            .width(ui.available_width())
            .selected_text(peltor::channel_label(ch))
            .show_ui(ui, |ui| {
                for &(c, _) in &peltor::ANALOG_CHANNELS {
                    ui.selectable_value(&mut ch, c, peltor::channel_label(c));
                }
            });
        if ch != self.cfg.radio.channel {
            self.cfg.radio.channel = ch;
            self.apply_peltor_channel();
        }
        ui.add_space(4.0);
        row_stat(ui, "SA828 freq", &self.cfg.radio.frequency_mhz);
        ui.add_space(10.0);

        {
            let mut sq = self.cfg.radio.squelch as f32;
            ui.label(RichText::new("Squelch").size(12.0).color(MUTED));
            ui.add(
                egui::Slider::new(&mut sq, 0.0..=8.0)
                    .integer()
                    .suffix("  (0=open hiss, 1=normal)"),
            );
            self.cfg.radio.squelch = sq as u8;
        }
        ui.add_space(10.0);

        self.cfg.radio.ctcss = peltor::clamp_ctcss(self.cfg.radio.ctcss);
        ui.label(RichText::new("CTCSS").size(12.0).color(MUTED));
        egui::ComboBox::from_id_salt("peltor_ctcss")
            .width(ui.available_width())
            .selected_text(peltor::ctcss_label(self.cfg.radio.ctcss))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.cfg.radio.ctcss, 0, "Off");
                for &(idx, _) in &peltor::CTCSS_TONES {
                    let label = peltor::ctcss_label(idx);
                    ui.selectable_value(&mut self.cfg.radio.ctcss, idx, label);
                }
            });
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "Digital DMR channels on the headset are not used — SA828 is analog only.",
            )
            .size(11.0)
            .color(MUTED),
        );

        ui.add_space(10.0);
        ui.label(RichText::new("UART port").size(12.0).color(MUTED));
        ui.add(
            egui::TextEdit::singleline(&mut self.cfg.radio.uart_port)
                .desired_width(ui.available_width())
                .hint_text("/dev/serial0"),
        );
        ui.add_space(10.0);
        {
            let mut ptt = self.cfg.radio.ptt_gpio as f32;
            ui.label(RichText::new("PTT GPIO").size(12.0).color(MUTED));
            ui.add(
                egui::Slider::new(&mut ptt, 0.0..=27.0)
                    .integer()
                    .suffix("  (0=off; LOW=TX, High-Z idle)"),
            );
            self.cfg.radio.ptt_gpio = ptt as u8;
        }
        ui.add_space(10.0);
        ui.label(RichText::new("Audio device").size(12.0).color(MUTED));
        ui.add(
            egui::TextEdit::singleline(&mut self.cfg.radio.audio_device)
                .desired_width(ui.available_width())
                .hint_text("plughw:CARD=Headphones,DEV=0"),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new("Volume is the Pi system volume — the app does not change it.")
                .size(11.0)
                .color(MUTED),
        );
        ui.add_space(10.0);
        if ui
            .checkbox(&mut self.cfg.radio.muted, "Mute radio")
            .changed()
        {
            silo_alert::set_muted(self.cfg.radio.muted);
        }

        ui.add_space(10.0);
        ui.checkbox(
            &mut self.cfg.radio.volume_test_mode,
            "Volume test sounds",
        );
        if self.cfg.radio.volume_test_mode {
            ui.add_space(4.0);
            let clips = silo_alert::volume_test_files();
            if clips.is_empty() {
                ui.label(
                    RichText::new("No files in audio/volume_tests/")
                        .size(11.0)
                        .color(MUTED),
                );
            } else {
                if self.cfg.radio.volume_test_file.is_empty()
                    || !clips.iter().any(|(f, _)| f == &self.cfg.radio.volume_test_file)
                {
                    self.cfg.radio.volume_test_file = clips[0].0.clone();
                }
                let current = self.cfg.radio.volume_test_file.clone();
                let current_label = clips
                    .iter()
                    .find(|(f, _)| f == &current)
                    .map(|(_, l)| l.as_str())
                    .unwrap_or(current.as_str());
                ui.label(RichText::new("Test clip").size(12.0).color(MUTED));
                egui::ComboBox::from_id_salt("volume_test_clip")
                    .width(ui.available_width())
                    .selected_text(current_label)
                    .show_ui(ui, |ui| {
                        for (file, label) in &clips {
                            ui.selectable_value(
                                &mut self.cfg.radio.volume_test_file,
                                file.clone(),
                                label,
                            );
                        }
                    });
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Only used by Test radio — empty alerts still use the normal clip.")
                        .size(11.0)
                        .color(MUTED),
                );
            }
        }

        ui.add_space(14.0);
        row_stat(ui, "Alerts sent", &self.stats.alerts_sent.to_string());
        row_stat(
            ui,
            "Last alert",
            &self
                .stats
                .last_alert_unix
                .map(fmt_dt)
                .unwrap_or_else(|| "Never".into()),
        );

        ui.add_space(12.0);
        if pill(ui, "Program module").clicked() {
            self.program_sender();
        }
        ui.add_space(8.0);
        if pill(ui, "Read module").clicked() {
            self.read_radio();
        }
        ui.add_space(8.0);
        if pill(ui, "Test radio").clicked() {
            self.test_radio();
        }
        ui.add_space(4.0);
        let test_hint = if self.cfg.radio.volume_test_mode {
            "Test radio: PTT LOW → play selected volume-test WAV → High-Z idle."
        } else {
            "Test radio: PTT LOW → play WAV once → High-Z idle."
        };
        ui.label(RichText::new(test_hint).size(11.0).color(MUTED));
    }

    fn apply_peltor_channel(&mut self) {
        let ch = peltor::clamp_channel(self.cfg.radio.channel);
        self.cfg.radio.channel = ch;
        let freq = peltor::sa828_freq_for_channel(ch);
        self.cfg.radio.frequency_mhz = freq.clone();
        self.radio_freq = freq;
    }

    /// Site identity, cloud sync and storage.
    fn config_app(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Site ID").size(12.0).color(MUTED));
        ui.add(
            egui::TextEdit::singleline(&mut self.cfg.supabase.site_id)
                .desired_width(ui.available_width())
                .hint_text("spectr-pi"),
        );
        ui.add_space(12.0);

        ui.checkbox(&mut self.cfg.supabase.enabled, "Send events to Supabase");
        ui.add_space(10.0);
        row_stat(ui, "Cloud", &cloud_status(self.cloud.is_some()));
        row_stat(ui, "Queued", &supabase::pending().to_string());
        if supabase::schema_missing() {
            ui.add_space(8.0);
            ui.label(
                RichText::new("silo_events is missing. Run sql/supabase_silo.sql in the SQL Editor.")
                    .size(12.0)
                    .color(RED),
            );
        } else if let Some(err) = supabase::last_error() {
            ui.add_space(8.0);
            ui.label(RichText::new(err).size(11.0).color(RED));
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new("Events go to Supabase; Stats reads them back. Offline rows stay queued.")
                .size(11.0)
                .color(MUTED),
        );

        ui.add_space(16.0);
        ui.label(RichText::new("Monitoring").size(12.0).color(MUTED));
        ui.add_space(8.0);
        row_stat(ui, "Checks", &self.stats.checks.to_string());
        row_stat(ui, "Empty hits", &self.stats.empty_hits.to_string());
        row_stat(
            ui,
            "Running since",
            &fmt_dt(self.stats.started_unix),
        );
        row_stat(
            ui,
            "State",
            self.plant_state().label(),
        );

        ui.add_space(16.0);
        ui.label(RichText::new("Storage").size(12.0).color(MUTED));
        ui.add_space(8.0);
        row_stat(ui, "Log", &self.log_path.display().to_string());
        row_stat(ui, "Images", &self.samples.len().to_string());
        ui.add_space(10.0);
        let monitoring = if self.monitor { "Pause monitoring" } else { "Resume monitoring" };
        if pill(ui, monitoring).clicked() {
            self.set_paused(self.monitor);
        }
    }

    /// Stats sidebar: the numbers, with the charts in the main area.
    fn stats_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Statistics").size(22.0).color(TEXT).strong());
        ui.add_space(6.0);
        ui.label(
            RichText::new("Everything the monitor has seen since it started.")
                .size(13.0)
                .color(MUTED),
        );

        ui.add_space(16.0);
        ui.label(RichText::new("Right now").size(12.0).color(MUTED));
        ui.add_space(8.0);
        row_stat(ui, "State", self.plant_state().label());
        row_stat(
            ui,
            "Monitoring",
            if !self.armed {
                "Disarmed"
            } else if self.monitor {
                "Running"
            } else {
                "Paused"
            },
        );
        row_stat(
            ui,
            "Camera",
            if self.cam.is_none() {
                "Not connected"
            } else if self.cam_down_since.is_some() {
                "No signal"
            } else {
                "Streaming"
            },
        );
        row_stat(
            ui,
            "Last check",
            &self
                .stats
                .last_check_unix
                .map(fmt_dt)
                .unwrap_or_else(|| "—".into()),
        );

        ui.add_space(16.0);
        ui.label(RichText::new("Totals").size(12.0).color(MUTED));
        ui.add_space(8.0);
        row_stat(ui, "Checks", &self.stats.checks.to_string());
        row_stat(ui, "Empty readings", &self.stats.empty_hits.to_string());
        let rate = if self.stats.checks > 0 {
            format!(
                "{:.0}%",
                self.stats.empty_hits as f32 / self.stats.checks as f32 * 100.0
            )
        } else {
            "—".into()
        };
        row_stat(ui, "Share empty", &rate);
        row_stat(ui, "Alerts sent", &self.stats.alerts_sent.to_string());
        row_stat(
            ui,
            "Last alert",
            &self
                .stats
                .last_alert_unix
                .map(fmt_dt)
                .unwrap_or_else(|| "Never".into()),
        );
        row_stat(ui, "Running since", &fmt_dt(self.stats.started_unix));

        ui.add_space(16.0);
        ui.label(RichText::new("Reference").size(12.0).color(MUTED));
        ui.add_space(8.0);
        row_stat(ui, "Empty photos", &self.stats.labels_empty.to_string());
        row_stat(ui, "Full samples", &self.stats.labels_full.to_string());
        row_stat(
            ui,
            "Photos agree",
            &self
                .reference
                .as_ref()
                .map(|r| format!("{:.0}%", r.cohesion * 100.0))
                .unwrap_or_else(|| "—".into()),
        );
        if let Some(warn) = self.reference.as_ref().and_then(|r| r.cohesion_warning()) {
            ui.add_space(4.0);
            ui.label(RichText::new(warn).size(11.0).color(RED));
        }
        row_stat(
            ui,
            "Built",
            &self
                .stats
                .last_train_unix
                .map(fmt_dt)
                .unwrap_or_else(|| "Never".into()),
        );
        if let Some(risk) = self.false_empty_risk() {
            row_stat(ui, "False-empty risk", &risk);
        }
        if let Some(warn) = self
            .reference
            .as_ref()
            .and_then(|r| vision::full_sample_warning(r, self.base_match_threshold()))
        {
            ui.add_space(4.0);
            ui.label(RichText::new(warn).size(11.0).color(RED));
        }

        ui.add_space(16.0);
        ui.label(RichText::new("Cloud").size(12.0).color(MUTED));
        ui.add_space(8.0);
        row_stat(ui, "Supabase", &cloud_status(self.cloud.is_some()));
        row_stat(ui, "Queued", &supabase::pending().to_string());
        row_stat(
            ui,
            "Site",
            if self.cfg.supabase.site_id.is_empty() {
                "spectr-pi"
            } else {
                self.cfg.supabase.site_id.as_str()
            },
        );
        ui.add_space(10.0);
    }

    /// Pull alert history from Supabase (throttled). Local outbox still covers offline.
    fn refresh_from_cloud(&mut self) {
        let Some(cloud) = &self.cloud else {
            return;
        };
        if self
            .last_cloud_pull
            .map(|t| t.elapsed() < Duration::from_secs(15))
            .unwrap_or(false)
        {
            return;
        }
        match cloud.fetch_empty_alerts(500) {
            Ok(remote) => {
                self.empty_alerts = merge_alert_ts(remote, self.empty_alerts.clone());
                self.last_cloud_pull = Some(Instant::now());
            }
            Err(e) => eprintln!("supabase: pull {e}"),
        }
    }

    /// Stats main area: alert history plus the most recent alerts.
    fn stats_main(&mut self, ui: &mut egui::Ui) {
        self.refresh_from_cloud();
        let days = eventlog::bucket_counts(&self.empty_alerts, 14, 86400);
        let hours = eventlog::bucket_counts(&self.empty_alerts, 24, 3600);

        egui::Frame::new()
            .fill(VIDEO_BG)
            .corner_radius(16)
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("Empty alerts — last 14 days")
                                .size(13.0)
                                .color(TEXT),
                        );
                        ui.add_space(8.0);
                        line_chart(ui, &days, 86400);

                        ui.add_space(24.0);
                        ui.label(
                            RichText::new("Empty alerts — last 24 hours")
                                .size(13.0)
                                .color(TEXT),
                        );
                        ui.add_space(8.0);
                        line_chart(ui, &hours, 3600);

                        ui.add_space(24.0);
                        ui.label(RichText::new("Recent alerts").size(13.0).color(TEXT));
                        ui.add_space(8.0);
                        if self.empty_alerts.is_empty() {
                            ui.label(
                                RichText::new("No alerts recorded yet.")
                                    .size(12.0)
                                    .color(MUTED),
                            );
                        }
                        for ts in self.empty_alerts.iter().rev().take(12) {
                            ui.label(
                                RichText::new(fmt_dt(*ts))
                                    .size(12.0)
                                    .color(MUTED)
                                    .monospace(),
                            );
                        }
                        ui.add_space(8.0);
                    });
            });
    }

    /// Tear down the current source and open it again from saved settings.
    fn reconnect_camera(&mut self) {
        self.cam = None;
        self.last_frame_at = None;
        self.cam_down_since = None;
        self.cam_retry_at = None;
        self.warned_cam_open = false;
        self.started_at = Instant::now();
        match camera::Cam::open(&self.cfg.camera) {
            Ok(c) => {
                self.cam = Some(c);
                self.note = "Camera reconnecting".into();
                self.term_line(Self::term_now("camera  reconnect requested"));
            }
            Err(e) => {
                self.note = e.clone();
                self.term_line(Self::term_now(&format!("camera  {e}")));
            }
        }
    }

    fn term_line(&mut self, line: String) {
        self.term_entry(line, None);
    }

    fn term_entry(&mut self, text: String, thumb: Option<egui::TextureHandle>) {
        append_log(&self.log_path, &text);
        self.terminal.push_back(TermEntry { text, thumb });
        while self.terminal.len() > TERM_MAX {
            self.terminal.pop_front();
        }
    }

    fn term_now(prefix: &str) -> String {
        let now = Local::now();
        format!("{}  {prefix}", now.format("%H:%M:%S"))
    }

    /// One Live cycle: model decides empty/full from training only (no human confirm).
    fn run_check(&mut self, ctx: &egui::Context, w: u32, h: u32, rgb: &[u8]) {
        self.last_check_at = Some(Instant::now());

        let Some(reference) = self.reference.as_ref() else {
            self.last_match = None;
            self.empty = false;
            self.empty_since = None;
            self.full_since = None;
            self.full_streak = 0;
            // Say it once, not every cycle.
            if !self.warned_no_reference {
                self.warned_no_reference = true;
                self.term_line(Self::term_now("no reference yet — add photos of the empty silo"));
            }
            return;
        };
        self.warned_no_reference = false;

        // Compare through the same region the reference was built with,
        // weighing the plain picture and its black-and-white twin equally.
        let roi = reference.roi;
        let feat = vision::features_in_roi(w, h, rgb, roi);
        let feat_bw = vision::bw_features(w, h, rgb, roi, Some(reference.bw_threshold));
        let score = reference.similarity_pair(&feat, &feat_bw);
        self.last_match = Some(score);

        let threshold = self.match_threshold();
        // Enter empty at `threshold`; leave only clearly below it (hysteresis)
        // so flicker on the line cannot clear the radio latch in one frame.
        const CLEAR_HYSTERESIS: f32 = 0.03;
        let looks_empty = score >= threshold;
        let looks_full = score < (threshold - CLEAR_HYSTERESIS).max(0.0);

        let unix = eventlog::now_unix();
        let interval = self.cfg.level.check_interval_seconds.max(1.0);
        let need = self.cfg.level.empty_confirmation_seconds.max(15.0) as u64;
        let need_streak = ((need as f32 / interval).ceil() as u32).max(2);
        let pct = (score * 100.0).round();
        let ts = Local::now().format("%H:%M:%S");

        if looks_empty {
            self.empty_streak = self.empty_streak.saturating_add(1);
            if self.empty_since.is_none() {
                self.empty_since = Some(unix);
            }
            self.full_streak = 0;
            self.full_since = None;
        } else {
            self.empty_streak = 0;
            self.empty_since = None;
            if looks_full && self.empty {
                self.full_streak = self.full_streak.saturating_add(1);
                if self.full_since.is_none() {
                    self.full_since = Some(unix);
                }
            } else {
                self.full_streak = 0;
                self.full_since = None;
            }
        }

        let long_enough = self
            .empty_since
            .map(|t| unix.saturating_sub(t) >= need)
            .unwrap_or(false);
        let confirmed = looks_empty && long_enough && self.empty_streak >= need_streak;

        let filled_long_enough = self
            .full_since
            .map(|t| unix.saturating_sub(t) >= need)
            .unwrap_or(false);
        let filled_confirmed =
            looks_full && filled_long_enough && self.full_streak >= need_streak;

        let just_confirmed = self.armed && confirmed && !self.empty;
        let just_filled = self.armed && filled_confirmed && self.empty;

        let mut alerted = false;
        if just_confirmed {
            self.empty = true;
            self.events.empty_alert(unix);
            self.empty_alerts.push(unix);
            self.events.state(unix, "level_state", "OK", "EMPTY", "");
            if let Some(cloud) = &self.cloud {
                cloud.push_empty_alert(unix);
            }
            if let Ok(path) =
                vision::save_alert_evidence(w, h, rgb, roi, unix, score, threshold, self.empty_streak)
            {
                self.evidence_path = Some(path);
                self.evidence_tex = None;
            }
            // TX only on this edge — never on every check while still empty.
            if let Some(alert) = &mut self.alert {
                if alert.on_empty_confirmed() {
                    alerted = true;
                    self.stats.alerts_sent += 1;
                    self.stats.last_alert_unix = alert.last_alert_unix();
                    self.stats.save();
                }
            }
        } else if just_filled {
            self.empty = false;
            self.full_streak = 0;
            self.full_since = None;
            self.events.state(unix, "level_state", "EMPTY", "OK", "");
            if let Some(alert) = &mut self.alert {
                alert.on_filled();
            }
        }

        let kind = if looks_empty { "empty" } else { "full" };
        // A small picture of exactly what was compared, shown in the log.
        let shot = {
            let (tw, th, pixels) = vision::thumb_from_rgb(w, h, rgb, roi, 72);
            let img = egui::ColorImage::from_rgb([tw as usize, th as usize], &pixels);
            Some(ctx.load_texture(format!("shot_{unix}"), img, Default::default()))
        };
        let cooldown = looks_empty && self.empty && !alerted;
        let conf = self
            .confidence()
            .map(|c| format!("  conf {:.0}%", c * 100.0))
            .unwrap_or_default();
        self.term_entry(
            format!(
                "{ts}  check  {kind}  match {pct:.0}%{conf}{}",
                if cooldown { "  (already alerted)" } else { "" }
            ),
            shot,
        );
        if just_confirmed {
            self.term_line(format!("{ts}  empty confirmed"));
        } else if just_filled {
            self.term_line(format!("{ts}  silo filled — monitoring"));
        }
        if alerted {
            self.term_line(format!("{ts}  alert  silo is empty"));
        }

        self.stats.checks = self.stats.checks.saturating_add(1);
        if looks_empty {
            self.stats.empty_hits = self.stats.empty_hits.saturating_add(1);
        }
        self.stats.last_check_unix = Some(unix);
        self.stats.last_check_empty = Some(looks_empty);
        self.stats.empty_state = self.empty;
        self.stats.empty_since_unix = self.empty_since;
        if just_confirmed || just_filled {
            self.stats.save();
        }
        if self.stats.checks % 4 == 0 || just_confirmed || just_filled {
            self.stats.save();
        }
    }

    /// Live: the camera, and beside it what the comparison actually works on.
    fn draw_live_view(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_size();
        let gap = 10.0;
        // Both feeds get the same box so they read as a matched pair.
        let split = self.show_model_view && full.x > 720.0;
        let pane = if split {
            Vec2::new((full.x - gap) * 0.5, full.y)
        } else {
            Vec2::new(full.x, full.y)
        };

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;

            // —— Camera ——
            let (rect, resp) = ui.allocate_exact_size(pane, Sense::click_and_drag());
            ui.painter()
                .rect_filled(rect, CornerRadius::same(16), VIDEO_BG);
            ui.painter().text(
                Pos2::new(rect.left() + 12.0, rect.top() + 12.0),
                egui::Align2::LEFT_TOP,
                "CAMERA",
                egui::FontId::proportional(10.0),
                MUTED,
            );
            // Controls sit on the feed itself, bottom-right.
            let controls = self.live_controls(ui, rect);

            if let Some(tex) = self.live_tex.as_ref() {
                let body = egui::Rect::from_min_max(
                    Pos2::new(rect.left() + 10.0, rect.top() + 30.0),
                    Pos2::new(rect.right() - 10.0, rect.bottom() - 10.0),
                );
                let size = tex.size_vec2();
                let scale = (body.width() / size.x).min(body.height() / size.y);
                let draw = size * scale;
                let img = egui::Rect::from_center_size(body.center(), draw);
                let uv =
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                ui.painter().image(tex.id(), img, uv, Color32::WHITE);
                // A press on a control must not start drawing a box.
                if !controls {
                    self.handle_region_drag(ui, &resp, img);
                }
                self.draw_region_overlay(ui, img);
            }

            if !split {
                return;
            }

            // —— What the comparison sees ——
            pane_with_caption(
                ui,
                pane,
                "BLACK & WHITE",
                self.model_tex.as_ref(),
                "mark a region",
            );
        });
    }

    /// Mark / Clear / B&W laid over the bottom-right of the feed.
    /// Returns true while the pointer is on one of them, so the region drag
    /// can stand down.
    fn live_controls(&mut self, ui: &mut egui::Ui, feed: egui::Rect) -> bool {
        let h = 26.0;
        let pad = 12.0;
        let gap = 6.0;

        let mut labels: Vec<(&str, bool)> = Vec::new();
        if self.marking_region {
            labels.push(("Cancel", true));
        } else {
            labels.push(("Mark", false));
        }
        if self.region.is_some() && !self.marking_region {
            labels.push(("Clear", false));
        }
        labels.push(("B&W", self.show_model_view));

        // Lay out right to left so the row hugs the corner.
        let mut x = feed.right() - pad;
        let y = feed.bottom() - pad - h;
        let mut hits: Vec<(egui::Rect, &str)> = Vec::new();
        for (text, on) in labels.iter().rev() {
            let w = (text.len() as f32 * 7.0 + 22.0).max(46.0);
            let r = egui::Rect::from_min_size(Pos2::new(x - w, y), Vec2::new(w, h));
            x = r.left() - gap;

            let id = ui.id().with(("live_ctl", *text));
            let resp = ui.interact(r, id, Sense::click());
            let bg = if *on {
                BLUE
            } else if resp.hovered() {
                Color32::from_rgb(70, 70, 74)
            } else {
                Color32::from_rgba_unmultiplied(28, 28, 30, 220)
            };
            ui.painter().rect_filled(r, CornerRadius::same(7), bg);
            ui.painter().text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                *text,
                egui::FontId::proportional(12.0),
                if *on { Color32::WHITE } else { TEXT },
            );
            if resp.clicked() {
                hits.push((r, text));
            }
        }

        for (_, text) in hits {
            match text {
                // Marking always starts from a clean frame, so pressing Mark
                // again simply replaces whatever box was there.
                "Mark" => {
                    if self.region.is_some() {
                        self.clear_region();
                    }
                    self.marking_region = true;
                }
                "Cancel" => {
                    self.marking_region = false;
                    self.box_drag_start = None;
                    self.box_drag_now = None;
                }
                "B&W" => self.show_model_view = !self.show_model_view,
                "Clear" => self.clear_region(),
                _ => {}
            }
        }

        if self.marking_region {
            ui.painter().text(
                Pos2::new(feed.left() + 12.0, feed.bottom() - pad - h * 0.5),
                egui::Align2::LEFT_CENTER,
                "Drag a box on the view",
                egui::FontId::proportional(12.0),
                BLUE,
            );
        }

        let row = egui::Rect::from_min_max(
            Pos2::new(x, y),
            Pos2::new(feed.right() - pad, y + h),
        );
        ui.ctx()
            .pointer_latest_pos()
            .map(|p| row.contains(p))
            .unwrap_or(false)
    }

    /// Keep the two diagnostic panes in step with the live frame.
    fn sync_model_view(&mut self, ctx: &egui::Context) {
        if !self.show_model_view || self.page != Page::Live {
            return;
        }
        let Some((w, h, rgb)) = self.last_rgb.as_ref() else {
            return;
        };
        let roi = self.reference.as_ref().and_then(|r| r.roi).or(self.region);
        let threshold = self.reference.as_ref().map(|r| r.bw_threshold);
        let (mw, mh, pixels) = vision::bw_from_rgb(*w, *h, rgb, roi, threshold);
        let img = egui::ColorImage::from_rgb([mw as usize, mh as usize], &pixels);
        match &mut self.model_tex {
            Some(t) => t.set(img, egui::TextureOptions::NEAREST),
            None => {
                self.model_tex =
                    Some(ctx.load_texture("model_view", img, egui::TextureOptions::NEAREST))
            }
        }

    }

    /// How decisively the last frame landed on one side of the threshold.
    ///
    /// Measured against how much the reference photos naturally differ from
    /// each other: 0% sits right on the line, 100% is far clear of it.
    fn confidence(&self) -> Option<f32> {
        let score = self.last_match?;
        let reference = self.reference.as_ref()?;
        let spread = (1.0 - reference.cohesion).max(0.01);
        Some(((score - self.match_threshold()).abs() / spread).clamp(0.0, 1.0))
    }

    /// Where the line sits: operator setting or auto suggestion, then raised
    /// if any full sample would sit too close to that line.
    fn match_threshold(&self) -> f32 {
        let base = self.base_match_threshold();
        match self.reference.as_ref() {
            Some(r) => vision::enforce_full_sample_floor(base, r),
            None => base,
        }
    }

    fn base_match_threshold(&self) -> f32 {
        let set = self.cfg.level.empty_match_threshold;
        if set > 0.0 {
            return set;
        }
        self.reference
            .as_ref()
            .map(|r| r.suggested_threshold())
            .unwrap_or(0.95)
    }

    /// Store the current frame as a photo of the empty silo.
    fn add_reference_photo(&mut self) {
        let Some((w, h, rgb)) = self.last_rgb.clone() else {
            self.note = "No frame yet".into();
            return;
        };
        match vision::save_reference_photo(w, h, &rgb) {
            Ok(_) => {
                self.dataset_dirty = true;
                self.rebuild_reference();
            }
            Err(e) => self.note = format!("Capture failed: {e}"),
        }
    }

    /// Recompute the reference from every photo on disk.
    fn rebuild_reference(&mut self) {
        match vision::build_reference() {
            Ok(r) => {
                let n = r.count();
                self.stats.labels_empty = n as u64;
                self.stats.labels_full = vision::list_full_samples().len() as u64;
                self.stats.last_train_unix = Some(r.built_at_unix);
                self.stats.last_train_accuracy = Some(r.cohesion);
                self.stats.save();
                let warn = r.cohesion_warning();
                self.reference = Some(r);
                self.note = match warn {
                    Some(w) => format!("Reference updated — {n} photos. {w}"),
                    None => format!("Reference updated — {n} photos"),
                };
                self.term_line(Self::term_now(&format!("reference updated ({n} photos)")));
            }
            Err(e) => self.note = e,
        }
    }

    fn test_radio(&mut self) {
        let wav = if self.cfg.radio.volume_test_mode {
            match silo_alert::volume_test_path(&self.cfg.radio.volume_test_file) {
                Some(p) => Some(p),
                None => {
                    self.note = if self.cfg.radio.volume_test_file.is_empty() {
                        "Pick a volume-test clip first".into()
                    } else {
                        format!(
                            "Missing volume-test file: {}",
                            self.cfg.radio.volume_test_file
                        )
                    };
                    return;
                }
            }
        } else {
            None
        };
        let result = match self.alert.as_mut() {
            Some(a) => a.test_transmit(wav.clone()),
            None => match &wav {
                Some(p) => silo_alert::play_voice_file(p),
                None => silo_alert::play_voice(),
            },
        };
        self.note = match result {
            Ok(()) => "Transmitting…".into(),
            Err(e) => e,
        };
    }

    fn program_sender(&mut self) {
        self.apply_peltor_channel();
        match sa828::program(
            &self.cfg.radio.frequency_mhz,
            self.cfg.radio.squelch,
            self.cfg.radio.ctcss,
            &self.cfg.radio.uart_port,
        ) {
            Ok(msg) => {
                let _ = self.cfg.save();
                self.note = msg;
            }
            Err(e) => self.note = e,
        }
    }

    fn read_radio(&mut self) {
        self.note = match sa828::read(&self.cfg.radio.uart_port) {
            Ok(msg) => msg,
            Err(e) => e,
        };
    }

    fn save_config(&mut self) {
        self.apply_peltor_channel();
        self.cfg.radio.ctcss = peltor::clamp_ctcss(self.cfg.radio.ctcss);
        silo_alert::set_ptt_pin(self.cfg.radio.ptt_gpio);
        silo_alert::set_audio_device(&self.cfg.radio.audio_device);
        silo_alert::set_muted(self.cfg.radio.muted);
        match self.cfg.save() {
            Ok(()) => self.note = "Saved".into(),
            Err(e) => self.note = e,
        }
    }

    fn add_full_photo(&mut self) {
        let Some((w, h, rgb)) = self.last_rgb.clone() else {
            self.note = "No frame yet".into();
            return;
        };
        match vision::save_full_photo(w, h, &rgb) {
            Ok(_) => {
                self.stats.labels_full = vision::list_full_samples().len() as u64;
                self.stats.save();
                self.note = "Full sample saved".into();
                self.term_line(Self::term_now("full sample saved"));
            }
            Err(e) => self.note = format!("Capture failed: {e}"),
        }
    }

    fn set_armed(&mut self, armed: bool) {
        self.armed = armed;
        self.stats.armed = armed;
        if armed {
            self.stats.wizard_done = true;
            self.stats.monitor_paused = false;
            self.monitor = true;
            self.term_line(Self::term_now("monitoring armed"));
            if let Some(cloud) = &self.cloud {
                cloud.push_event("armed", Some(self.empty), None);
            }
        } else {
            self.term_line(Self::term_now("monitoring disarmed"));
            // Do not clear announce latch — re-arm while still empty must not re-TX.
            if let Some(cloud) = &self.cloud {
                cloud.push_event("disarmed", Some(self.empty), None);
            }
        }
        self.stats.save();
    }

    fn set_paused(&mut self, paused: bool) {
        self.monitor = !paused;
        self.stats.monitor_paused = paused;
        self.stats.save();
        if paused {
            self.term_line(Self::term_now("monitoring paused"));
        } else {
            self.term_line(Self::term_now("monitoring resumed"));
        }
    }

    fn can_arm(&self) -> bool {
        self.region.is_some() && self.reference.as_ref().map(|r| r.count() >= 1).unwrap_or(false)
    }

    fn wizard_step(&self) -> Option<&'static str> {
        if self.armed || self.stats.wizard_done {
            return None;
        }
        if self.region.is_none() {
            return Some("1  Mark the region");
        }
        if self.samples.len() < 3 {
            return Some("2  Take 3 empty photos");
        }
        if vision::list_full_samples().is_empty() {
            return Some("3  Optional: take a full sample");
        }
        Some("4  Arm when the dry-run looks right")
    }

    fn dry_run_line(&self) -> Option<String> {
        let score = self.last_match?;
        let th = self.match_threshold();
        if score >= th {
            Some(format!("would alert  ({:.0}% ≥ {:.0}%)", score * 100.0, th * 100.0))
        } else {
            Some(format!("would not  ({:.0}% < {:.0}%)", score * 100.0, th * 100.0))
        }
    }

    fn false_empty_risk(&self) -> Option<String> {
        let r = self.reference.as_ref()?;
        let risk = vision::false_empty_risk(r, self.match_threshold())?;
        Some(if risk < 0.34 {
            "Low".into()
        } else if risk < 0.67 {
            "Medium".into()
        } else {
            "High".into()
        })
    }

    fn ensure_evidence(&mut self, ctx: &egui::Context) {
        if self.evidence_tex.is_some() {
            return;
        }
        let Some(path) = self.evidence_path.as_ref() else {
            return;
        };
        if let Ok((w, h, rgba)) = vision::load_thumb_rgba(path, 240) {
            let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
            self.evidence_tex = Some(ctx.load_texture("evidence", img, Default::default()));
        }
    }

    fn plant_state(&self) -> PlantState {
        let cam_dead = self.cam_down_since.is_some()
            || (self.last_frame_at.is_none() && self.started_at.elapsed() >= CAM_GRACE);
        if cam_dead {
            return PlantState::CameraFault;
        }
        if self.radio_fault && !self.cfg.radio.muted {
            return PlantState::RadioFault;
        }
        if !self.armed {
            return PlantState::Disarmed;
        }
        if !self.monitor {
            return PlantState::Paused;
        }
        if self.empty {
            PlantState::Empty
        } else {
            PlantState::Ok
        }
    }

    fn tick_heartbeat(&mut self) {
        let due = self
            .last_heartbeat
            .map(|t| t.elapsed() >= Duration::from_secs(300))
            .unwrap_or(true);
        if !due {
            return;
        }
        self.last_heartbeat = Some(Instant::now());
        if let Some(cloud) = &self.cloud {
            cloud.push_heartbeat(&self.stats);
        }
    }

    fn take_radio_status(&mut self) {
        let Some(msg) = silo_alert::take_status() else {
            return;
        };
        let ok = silo_alert::status_ok(&msg);
        self.radio_fault = !ok;
        self.stats.last_radio_ok = ok;
        self.stats.last_radio_unix = Some(eventlog::now_unix());
        self.stats.last_radio_err = if ok { None } else { Some(msg.clone()) };
        self.stats.save();
        self.term_line(Self::term_now(&format!("radio  {msg}")));
        self.note = msg.clone();
        if let Some(cloud) = &self.cloud {
            if ok {
                cloud.push_event("radio_tx", Some(self.empty), None);
            } else {
                cloud.push_event("radio_fail", Some(self.empty), None);
            }
        }
    }

    fn refresh_samples(&mut self) {
        let keep_name = self
            .sample_idx
            .and_then(|i| self.samples.get(i).map(|s| s.file_name.clone()));
        self.samples = vision::list_references();
        self.thumb_tex
            .retain(|k, _| self.samples.iter().any(|s| s.path.to_string_lossy() == k.as_str()));
        self.sample_idx = keep_name.and_then(|name| {
            self.samples.iter().position(|s| s.file_name == name)
        });
        self.dataset_dirty = false;
        if let Some(i) = self.sample_idx {
            self.load_sample_preview(i);
        } else {
            self.sample_tex = None;
            self.sample_view = None;
        }
        self.stats.labels_empty = self.samples.len() as u64;
        self.stats.labels_full = vision::list_full_samples().len() as u64;
    }

    fn load_sample_preview(&mut self, idx: usize) {
        if idx >= self.samples.len() {
            return;
        }
        self.sample_idx = Some(idx);
        let meta = self.samples[idx].clone();
        match vision::analyze_sample(&meta.path, self.reference.as_ref()) {
            Ok(view) => {
                self.sample_view = Some(view);
                self.note.clear();
            }
            Err(e) => {
                self.sample_view = None;
                self.note = friendly_io_note(&e);
            }
        }
    }

    fn sync_sample_tex(&mut self, ctx: &egui::Context) {
        let Some(view) = self.sample_view.as_ref() else {
            return;
        };
        let img = egui::ColorImage::from_rgb(
            [view.width as usize, view.height as usize],
            &view.rgb,
        );
        match &mut self.sample_tex {
            Some(t) => t.set(img, Default::default()),
            None => self.sample_tex = Some(ctx.load_texture("sample", img, Default::default())),
        }
    }

    /// Thumbnails for both pictures of a reference: the plain one and its
    /// black-and-white twin.
    fn ensure_thumb(&mut self, ctx: &egui::Context, meta: &SampleMeta) {
        let pairs = [
            (meta.path.clone(), meta.path.to_string_lossy().to_string()),
            (
                vision::bw_path_for(&meta.file_name),
                format!("bw:{}", meta.file_name),
            ),
        ];
        for (path, key) in pairs {
            if self.thumb_tex.contains_key(&key) {
                continue;
            }
            if let Ok((w, h, rgba)) = vision::load_thumb_rgba(&path, 160) {
                let img =
                    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                let tex = ctx.load_texture(key.clone(), img, Default::default());
                self.thumb_tex.insert(key, tex);
            }
        }
    }

    fn delete_selected(&mut self) {
        let Some(idx) = self.sample_idx.filter(|i| *i < self.samples.len()) else {
            self.note = "Nothing selected".into();
            return;
        };
        let idxs: Vec<usize> = (0..self.samples.len()).collect();
        let pos = idxs.iter().position(|&x| x == idx);
        let meta = self.samples[idx].clone();
        let key = meta.path.to_string_lossy().to_string();
        match vision::delete_sample(&meta.path) {
            Ok(()) => {
                self.thumb_tex.remove(&key);
                self.sample_view = None;
                self.sample_tex = None;
                self.sample_idx = None;
                self.refresh_samples();
                let idxs: Vec<usize> = (0..self.samples.len()).collect();
                if let Some(p) = pos {
                    if !idxs.is_empty() {
                        let ni = p.min(idxs.len() - 1);
                        self.load_sample_preview(idxs[ni]);
                    }
                }
                self.note = match meta.label_empty {
                    Some(true) => "Deleted empty frame".into(),
                    Some(false) => "Deleted full frame".into(),
                    None => "Deleted capture".into(),
                };
            }
            Err(e) => self.note = friendly_io_note(&e),
        }
    }

    /// Library: every image at once in a scrolling grid, so a long capture
    /// session is actually browsable.
    fn draw_library_grid(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let idxs: Vec<usize> = (0..self.samples.len()).collect();
        for &i in &idxs {
            let meta = self.samples[i].clone();
            self.ensure_thumb(ctx, &meta);
        }

        let avail = ui.available_size();
        let mut clicked: Option<usize> = None;
        let total = idxs.len();

        egui::Frame::new()
            .fill(VIDEO_BG)
            .corner_radius(16)
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.set_min_size(avail);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{total} images"))
                            .size(12.0)
                            .color(MUTED),
                    );
                });
                ui.add_space(10.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if idxs.is_empty() {
                            ui.add_space(avail.y * 0.25);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new("Nothing here").size(16.0).color(MUTED),
                                );
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(
                                        "Capture frames on Live, or change the filter.",
                                    )
                                    .size(13.0)
                                    .color(MUTED),
                                );
                            });
                            return;
                        }

                        // Each cell holds both pictures of one reference:
                        // the camera view on top, black and white below.
                        let cell_w = 150.0;
                        let cell_h = 200.0;
                        let gap = 10.0;
                        let width = ui.available_width().max(cell_w);
                        let cols = (((width + gap) / (cell_w + gap)).floor() as usize).max(1);

                        for row in idxs.chunks(cols) {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = gap;
                                for &i in row {
                                    let meta = &self.samples[i];
                                    let key = meta.path.to_string_lossy().to_string();
                                    let bw_key = format!("bw:{}", meta.file_name);
                                    let selected = self.sample_idx == Some(i);
                                    let (rect, resp) = ui.allocate_exact_size(
                                        Vec2::new(cell_w, cell_h),
                                        Sense::click(),
                                    );
                                    ui.painter().rect_filled(
                                        rect,
                                        CornerRadius::same(10),
                                        Color32::from_rgb(18, 18, 20),
                                    );

                                    let half = (cell_h - 34.0) * 0.5;
                                    let uv = egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    );
                                    for (n, k) in [&key, &bw_key].into_iter().enumerate() {
                                        let Some(tex) = self.thumb_tex.get(k) else {
                                            continue;
                                        };
                                        let slot = egui::Rect::from_min_size(
                                            Pos2::new(
                                                rect.left() + 6.0,
                                                rect.top() + 6.0 + n as f32 * (half + 4.0),
                                            ),
                                            Vec2::new(cell_w - 12.0, half),
                                        );
                                        let size = tex.size_vec2();
                                        let scale = (slot.width() / size.x)
                                            .min(slot.height() / size.y)
                                            .max(0.01);
                                        let draw = size * scale;
                                        let img_rect =
                                            egui::Rect::from_center_size(slot.center(), draw);
                                        ui.painter()
                                            .image(tex.id(), img_rect, uv, Color32::WHITE);
                                    }

                                    ui.painter().text(
                                        Pos2::new(rect.left() + 10.0, rect.bottom() - 12.0),
                                        egui::Align2::LEFT_CENTER,
                                        fmt_ms(meta.captured_ms),
                                        egui::FontId::proportional(11.0),
                                        MUTED,
                                    );
                                    if selected {
                                        ui.painter().rect_stroke(
                                            rect,
                                            CornerRadius::same(10),
                                            Stroke::new(2.0_f32, BLUE),
                                            egui::StrokeKind::Inside,
                                        );
                                    } else if resp.hovered() {
                                        ui.painter().rect_stroke(
                                            rect,
                                            CornerRadius::same(10),
                                            Stroke::new(1.0_f32, HAIRLINE),
                                            egui::StrokeKind::Inside,
                                        );
                                    }
                                    if resp.clicked() {
                                        clicked = Some(i);
                                    }
                                }
                            });
                            ui.add_space(gap);
                        }
                    });
            });

        if let Some(i) = clicked {
            self.load_sample_preview(i);
        }
    }

    /// Drag on the live view to mark the region the app watches. Stored as
    /// fractions of the frame, so it survives a resolution change.
    fn handle_region_drag(&mut self, ui: &egui::Ui, resp: &egui::Response, img: egui::Rect) {
        if !self.marking_region {
            return;
        }
        let to_norm = |p: Pos2| {
            (
                ((p.x - img.left()) / img.width().max(1.0)).clamp(0.0, 1.0),
                ((p.y - img.top()) / img.height().max(1.0)).clamp(0.0, 1.0),
            )
        };

        if resp.drag_started() {
            if let Some(p) = resp.interact_pointer_pos() {
                if img.contains(p) {
                    self.box_drag_start = Some(to_norm(p));
                }
            }
        }
        if resp.dragged() {
            if let Some(p) = resp.interact_pointer_pos() {
                self.box_drag_now = Some(to_norm(p));
            }
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
        if resp.drag_stopped() {
            if let (Some((ax, ay)), Some((bx, by))) = (self.box_drag_start, self.box_drag_now) {
                match BoxNorm::from_corners(ax, ay, bx, by) {
                    Some(b) => {
                        self.region = Some(b);
                        vision::save_region(self.region);
                        self.marking_region = false;
                        self.term_line(Self::term_now("region marked"));
                        if !self.samples.is_empty() {
                            self.rebuild_reference();
                        } else {
                            self.note = "Region marked".into();
                        }
                    }
                    None => self.note = "Region too small".into(),
                }
            }
            self.box_drag_start = None;
            self.box_drag_now = None;
        }
    }

    /// Paint the watched region and the rubber band while dragging.
    fn draw_region_overlay(&self, ui: &egui::Ui, img: egui::Rect) {
        let px = |b: BoxNorm| {
            egui::Rect::from_min_max(
                Pos2::new(
                    img.left() + b.x1 * img.width(),
                    img.top() + b.y1 * img.height(),
                ),
                Pos2::new(
                    img.left() + b.x2 * img.width(),
                    img.top() + b.y2 * img.height(),
                ),
            )
        };

        if let Some(b) = self.region {
            ui.painter().rect_stroke(
                px(b),
                CornerRadius::same(4),
                Stroke::new(2.0_f32, BLUE),
                egui::StrokeKind::Inside,
            );
        }

        if let (Some((ax, ay)), Some((bx, by))) = (self.box_drag_start, self.box_drag_now) {
            let r = egui::Rect::from_two_pos(
                Pos2::new(img.left() + ax * img.width(), img.top() + ay * img.height()),
                Pos2::new(img.left() + bx * img.width(), img.top() + by * img.height()),
            );
            ui.painter().rect_stroke(
                r,
                CornerRadius::same(4),
                Stroke::new(2.0_f32, Color32::WHITE),
                egui::StrokeKind::Inside,
            );
        }
    }

    fn clear_region(&mut self) {
        self.region = None;
        vision::save_region(None);
        if !self.samples.is_empty() {
            self.rebuild_reference();
        } else {
            self.note = "Region cleared".into();
        }
    }

    /// True when the marked region no longer matches the reference's.
    fn region_changed(&self) -> bool {
        match self.reference.as_ref() {
            Some(r) => self.region != r.roi,
            None => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlantState {
    CameraFault,
    RadioFault,
    Disarmed,
    Paused,
    Empty,
    Ok,
}

impl PlantState {
    fn label(self) -> &'static str {
        match self {
            PlantState::CameraFault => "CAMERA FAULT",
            PlantState::RadioFault => "RADIO FAULT",
            PlantState::Disarmed => "DISARMED",
            PlantState::Paused => "PAUSED",
            PlantState::Empty => "EMPTY",
            PlantState::Ok => "OK",
        }
    }

    fn color(self) -> Color32 {
        match self {
            PlantState::CameraFault | PlantState::RadioFault | PlantState::Empty => RED,
            PlantState::Disarmed | PlantState::Paused => MUTED,
            PlantState::Ok => TEXT,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        style(ctx);
        self.ensure_logo(ctx);
        self.take_radio_status();
        if self.dataset_dirty {
            self.refresh_samples();
        }
        self.pull_frame(ctx);
        self.sync_model_view(ctx);
        self.tick_heartbeat();
        if self.page == Page::Model && self.sample_idx.is_some() {
            self.sync_sample_tex(ctx);
        }
        ctx.request_repaint_after(Duration::from_millis(100));

        let has_reference = self.reference.is_some();

        egui::TopBottomPanel::top("navbar")
            .exact_height(NAV_H)
            .frame(
                egui::Frame::new()
                    .fill(CARD)
                    .inner_margin(egui::Margin::ZERO),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                ui.horizontal(|ui| {
                    // Logo cell — larger square segment, mark painted dead-center
                    let cell = Vec2::new(LOGO_CELL, NAV_H);
                    let (logo_rect, _) = ui.allocate_exact_size(cell, Sense::hover());
                    if let Some(logo) = &self.logo_tex {
                        let mark = Vec2::splat(LOGO_MARK);
                        let img_rect = egui::Rect::from_center_size(logo_rect.center(), mark);
                        let uv = egui::Rect::from_min_max(
                            egui::pos2(0.0, 0.0),
                            egui::pos2(1.0, 1.0),
                        );
                        ui.painter().image(logo.id(), img_rect, uv, Color32::WHITE);
                    }
                    nav_bar(ui, &mut self.page);
                });
                // Bottom hairline across the bar
                let r = ui.max_rect();
                ui.painter()
                    .hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0_f32, HAIRLINE));
            });

        egui::SidePanel::right("side")
            .exact_width(356.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(CARD)
                    // Less padding on the right: the scrollbar lives there.
                    .inner_margin(egui::Margin {
                        left: 22,
                        right: 10,
                        top: 22,
                        bottom: 22,
                    }),
            )
            .show(ctx, |ui| {
                match self.page {
                    Page::Live => {
                        let interval = self.cfg.level.check_interval_seconds.max(1.0);
                        let next_in = self
                            .last_check_at
                            .map(|t| (interval - t.elapsed().as_secs_f32()).max(0.0).ceil() as u32)
                            .unwrap_or(0);
                        let state = self.plant_state();

                        ui.label(RichText::new("Live").size(22.0).color(TEXT).strong());
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(state.label())
                                .size(28.0)
                                .color(state.color())
                                .strong(),
                        );
                        if self.cfg.radio.muted {
                            ui.add_space(4.0);
                            ui.label(RichText::new("MUTE").size(13.0).color(RED).strong());
                        }
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(
                                self.last_match
                                    .map(|m| format!("{:.0}% match", m * 100.0))
                                    .unwrap_or_else(|| "—".into()),
                            )
                            .size(14.0)
                            .color(MUTED),
                        );

                        if let Some(step) = self.wizard_step() {
                            ui.add_space(10.0);
                            ui.label(RichText::new(step).size(13.0).color(BLUE));
                            if let Some(dry) = self.dry_run_line() {
                                ui.add_space(4.0);
                                ui.label(RichText::new(dry).size(12.0).color(MUTED));
                            }
                        }

                        if has_reference {
                            ui.add_space(12.0);
                            match self.confidence() {
                                Some(c) => {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new("Confidence")
                                                .size(12.0)
                                                .color(MUTED),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                ui.label(
                                                    RichText::new(format!("{:.0}%", c * 100.0))
                                                        .size(12.0)
                                                        .color(TEXT),
                                                );
                                            },
                                        );
                                    });
                                    ui.add_space(4.0);
                                    meter(ui, c, if c < 0.35 { RED } else { BLUE });
                                }
                                None => {
                                    ui.label(
                                        RichText::new("Confidence  —")
                                            .size(12.0)
                                            .color(MUTED),
                                    );
                                }
                            }

                            ui.add_space(10.0);
                            row_stat(
                                ui,
                                "Alerts at",
                                &format!("{:.0}% match", self.match_threshold() * 100.0),
                            );
                            row_stat(
                                ui,
                                "Next check",
                                &format!("{next_in}s  ·  every {interval:.0}s"),
                            );
                        }

                        ui.add_space(12.0);
                        row_stat(
                            ui,
                            "Last TX",
                            &self
                                .stats
                                .last_radio_unix
                                .map(fmt_dt)
                                .unwrap_or_else(|| "Never".into()),
                        );
                        row_stat(
                            ui,
                            "Radio",
                            if self.radio_fault {
                                "Fault"
                            } else if self.cfg.radio.muted {
                                "Muted"
                            } else if self
                                .stats
                                .last_radio_ok
                            {
                                "OK"
                            } else {
                                "—"
                            },
                        );
                        if self.empty && self.armed {
                            row_stat(ui, "Alert", "Sent for this empty");
                        }

                        self.ensure_evidence(ui.ctx());
                        if let Some(tex) = self.evidence_tex.as_ref() {
                            ui.add_space(8.0);
                            ui.label(RichText::new("Last alert").size(12.0).color(MUTED));
                            ui.add_space(4.0);
                            let size = tex.size_vec2();
                            let w = ui.available_width().min(200.0);
                            let scale = w / size.x.max(1.0);
                            ui.image((tex.id(), size * scale));
                        }

                        ui.add_space(14.0);
                        if pill_accent(ui, "Take reference").clicked() {
                            self.add_reference_photo();
                        }
                        if !self.empty {
                            ui.add_space(8.0);
                            if pill(ui, "Take full sample").clicked() {
                                self.add_full_photo();
                            }
                        }

                        ui.add_space(12.0);
                        note_line(ui, &self.note);
                        ui.add_space(8.0);
                        let pause = if self.monitor { "Pause" } else { "Resume" };
                        if pill(ui, pause).clicked() {
                            self.set_paused(self.monitor);
                        }
                        ui.add_space(8.0);
                        if !self.armed {
                            let can_arm = self.can_arm();
                            if pill_accent(ui, "Arm").clicked() {
                                if can_arm {
                                    self.set_armed(true);
                                } else {
                                    self.note = "Mark a region and take empty photos first".into();
                                }
                            }
                        } else if pill(ui, "Disarm").clicked() {
                            self.set_armed(false);
                        }
                    }
                    Page::Model => {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new("Model").size(22.0).color(TEXT).strong(),
                                );
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(
                                        "The photos of the empty silo that every frame is compared against.",
                                    )
                                    .size(13.0)
                                    .color(MUTED),
                                );

                                ui.add_space(14.0);
                                row_stat(ui, "Photos", &self.samples.len().to_string());
                                match self.reference.as_ref() {
                                    Some(r) => {
                                        row_stat(
                                            ui,
                                            "Photos agree",
                                            &format!("{:.0}%", r.cohesion * 100.0),
                                        );
                                        if let Some(warn) = r.cohesion_warning() {
                                            ui.add_space(4.0);
                                            ui.label(RichText::new(warn).size(11.0).color(RED));
                                        }
                                        row_stat(ui, "Built", &fmt_dt(r.built_at_unix));
                                    }
                                    None => {
                                        row_stat(ui, "Reference", "Not built");
                                    }
                                }
                                row_stat(
                                    ui,
                                    "Region",
                                    if self.region.is_some() { "Marked" } else { "Whole frame" },
                                );
                                row_stat(
                                    ui,
                                    "Alerts at",
                                    &format!("{:.0}% match", self.match_threshold() * 100.0),
                                );
                                row_stat(
                                    ui,
                                    "Full samples",
                                    &vision::list_full_samples().len().to_string(),
                                );
                                if let Some(risk) = self.false_empty_risk() {
                                    row_stat(ui, "False-empty risk", &risk);
                                }
                                if let Some(warn) = self
                                    .reference
                                    .as_ref()
                                    .and_then(|r| {
                                        vision::full_sample_warning(r, self.base_match_threshold())
                                    })
                                {
                                    ui.add_space(4.0);
                                    ui.label(RichText::new(warn).size(11.0).color(RED));
                                }

                                if self.region_changed() {
                                    ui.add_space(8.0);
                                    ui.label(
                                        RichText::new(
                                            "Region changed — rebuild to apply it.",
                                        )
                                        .size(12.0)
                                        .color(RED),
                                    );
                                }

                                ui.add_space(12.0);
                                if pill_accent(ui, "Rebuild reference").clicked() {
                                    self.rebuild_reference();
                                }

                                if let (Some(i), Some(view)) =
                                    (self.sample_idx, self.sample_view.as_ref())
                                {
                                    if let Some(meta) = self.samples.get(i) {
                                        ui.add_space(16.0);
                                        ui.label(
                                            RichText::new("Selected photo")
                                                .size(12.0)
                                                .color(MUTED),
                                        );
                                        ui.add_space(8.0);
                                        row_stat(ui, "Taken", &fmt_ms(meta.captured_ms));
                                        row_stat(
                                            ui,
                                            "Size",
                                            &format!("{}×{}", view.width, view.height),
                                        );
                                        row_stat(ui, "Disk", &format_bytes(meta.bytes));
                                        if let Some(m) = view.match_score {
                                            row_stat(
                                                ui,
                                                "Match",
                                                &format!("{:.0}%", m * 100.0),
                                            );
                                        }
                                        ui.add_space(10.0);
                                        if quiet_btn(ui, "Delete").clicked() {
                                            self.delete_selected();
                                            self.rebuild_reference();
                                        }
                                    }
                                }

                                ui.add_space(12.0);
                                note_line(ui, &self.note);
                                ui.add_space(8.0);
                            });
                    }
                    Page::Stats => {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| self.stats_sidebar(ui));
                    }
                    Page::Config => {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new("Configure").size(22.0).color(TEXT).strong(),
                                );
                                ui.add_space(10.0);
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    for (t, label) in [
                                        (ConfigTab::Detection, "Detection"),
                                        (ConfigTab::Camera, "Camera"),
                                        (ConfigTab::Radio, "Radio"),
                                        (ConfigTab::App, "App"),
                                    ] {
                                        if filter_chip(ui, label, self.config_tab == t).clicked() {
                                            self.config_tab = t;
                                        }
                                    }
                                });
                                ui.add_space(16.0);

                                match self.config_tab {
                                    ConfigTab::Detection => self.config_detection(ui),
                                    ConfigTab::Camera => self.config_camera(ui),
                                    ConfigTab::Radio => self.config_radio(ui),
                                    ConfigTab::App => self.config_app(ui),
                                }

                                ui.add_space(18.0);
                                if pill_accent(ui, "Save").clicked() {
                                    self.save_config();
                                }
                                ui.add_space(10.0);
                                note_line(ui, &self.note);
                                ui.add_space(8.0);
                            });
                    }
                }
            });

        self.draw_terminal_panel(ctx);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                match self.page {
                    Page::Stats => self.stats_main(ui),
                    Page::Model => self.draw_library_grid(ui, ctx),
                    _ => self.draw_live_view(ui),
                }
            });
    }
}

/// One diagnostic pane: a label strip over a square image.
fn pane_with_caption(
    ui: &mut egui::Ui,
    size: Vec2,
    caption: &str,
    tex: Option<&egui::TextureHandle>,
    empty_hint: &str,
) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(12), VIDEO_BG);
    ui.painter().text(
        Pos2::new(rect.left() + 12.0, rect.top() + 12.0),
        egui::Align2::LEFT_TOP,
        caption,
        egui::FontId::proportional(10.0),
        MUTED,
    );

    let body = egui::Rect::from_min_max(
        Pos2::new(rect.left() + 10.0, rect.top() + 30.0),
        Pos2::new(rect.right() - 10.0, rect.bottom() - 10.0),
    );
    match tex {
        Some(tex) => {
            let s = tex.size_vec2();
            let scale = (body.width() / s.x).min(body.height() / s.y);
            let draw = s * scale;
            let img = egui::Rect::from_center_size(body.center(), draw);
            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            ui.painter().image(tex.id(), img, uv, Color32::WHITE);
        }
        None => {
            ui.painter().text(
                body.center(),
                egui::Align2::CENTER_CENTER,
                empty_hint,
                egui::FontId::proportional(11.0),
                MUTED,
            );
        }
    }
}

fn style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.dark_mode = true;
    v.panel_fill = CARD;
    v.window_fill = BG;
    v.override_text_color = Some(TEXT);
    v.widgets.inactive.weak_bg_fill = BTN;
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(58, 58, 60);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(72, 72, 74);
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.hovered.bg_stroke = Stroke::NONE;
    v.widgets.active.bg_stroke = Stroke::NONE;
    v.selection.bg_fill = Color32::from_rgb(10, 132, 255);
    v.extreme_bg_color = Color32::from_rgb(44, 44, 46);
    ctx.set_visuals(v);

    // A floating scrollbar draws on top of the sidebar text; give it its own
    // column instead.
    ctx.style_mut(|s| {
        s.spacing.scroll.floating = false;
        s.spacing.scroll.bar_width = 6.0;
        s.spacing.scroll.bar_inner_margin = 6.0;
        s.spacing.scroll.bar_outer_margin = 0.0;
    });
}

fn nav_bar(ui: &mut egui::Ui, page: &mut Page) {
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    let items = [
        (Page::Live, "Live"),
        (Page::Model, "Model"),
        (Page::Stats, "Stats"),
        (Page::Config, "Config"),
    ];
    let n = items.len();
    for (i, (p, label)) in items.into_iter().enumerate() {
        let on = *page == p;
        let color = if on { TEXT } else { MUTED };
        let galley = ui.fonts(|f| {
            f.layout_no_wrap(
                label.to_string(),
                egui::FontId::proportional(15.0),
                color,
            )
        });
        let pad_x = 22.0;
        let size = Vec2::new(galley.size().x + pad_x * 2.0, NAV_H);
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        // Segment borders — left edge always; right edge on last item
        ui.painter().vline(
            rect.left(),
            rect.y_range(),
            Stroke::new(1.0_f32, HAIRLINE),
        );
        if i + 1 == n {
            ui.painter().vline(
                rect.right(),
                rect.y_range(),
                Stroke::new(1.0_f32, HAIRLINE),
            );
        }
        if resp.hovered() && !on {
            ui.painter()
                .rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(255, 255, 255, 8));
        }
        ui.painter().galley(
            egui::pos2(
                rect.center().x - galley.size().x * 0.5,
                rect.center().y - galley.size().y * 0.5,
            ),
            galley,
            color,
        );
        if resp.clicked() {
            *page = p;
        }
    }
}

fn pill(ui: &mut egui::Ui, text: &str) -> egui::Response {
    centered_btn(ui, text, 14.0, TEXT, BTN, 20, Vec2::new(ui.available_width(), 40.0))
}

fn pill_accent(ui: &mut egui::Ui, text: &str) -> egui::Response {
    centered_btn(
        ui,
        text,
        14.0,
        Color32::WHITE,
        BLUE,
        20,
        Vec2::new(ui.available_width(), 40.0),
    )
}

fn centered_btn(
    ui: &mut egui::Ui,
    text: &str,
    size: f32,
    color: Color32,
    fill: Color32,
    radius: u8,
    min: Vec2,
) -> egui::Response {
    ui.allocate_ui_with_layout(
        min,
        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
        |ui| {
            ui.add(
                egui::Button::new(RichText::new(text).size(size).color(color))
                    .fill(fill)
                    .corner_radius(radius)
                    .stroke(Stroke::NONE),
            )
        },
    )
    .inner
}

fn cloud_status(enabled: bool) -> String {
    if !enabled {
        return "Off".into();
    }
    if supabase::schema_missing() {
        return "Table missing".into();
    }
    if let Some(err) = supabase::last_error() {
        return err;
    }
    if supabase::pending() > 0 {
        return "Syncing".into();
    }
    "OK".into()
}

fn merge_alert_ts(remote: Vec<u64>, local: Vec<u64>) -> Vec<u64> {
    let mut out = remote;
    out.extend(local);
    out.sort_unstable();
    out.dedup();
    out
}

fn row_stat(ui: &mut egui::Ui, k: &str, v: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(k).size(13.0).color(MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(v).size(13.0).color(TEXT));
        });
    });
    ui.add_space(6.0);
}

fn note_line(ui: &mut egui::Ui, note: &str) {
    if !note.is_empty() {
        ui.label(RichText::new(note).size(12.0).color(MUTED));
    }
}

/// Mirror terminal lines to disk, with a date stamp and a size cap so the
/// file cannot fill a Pi running for months.
fn append_log(path: &std::path::Path, line: &str) {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > LOG_MAX_BYTES {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{} {line}", Local::now().format("%Y-%m-%d"));
    }
}

/// Hide raw filesystem paths / timestamp filenames from status text.
fn friendly_io_note(err: &str) -> String {
    let lower = err.to_ascii_lowercase();
    if lower.contains("no such file") || lower.contains("not found") {
        return "Image missing".into();
    }
    if lower.contains("permission") {
        return "Permission denied".into();
    }
    // Strip absolute/relative paths and *.jpg names from the message.
    let mut out = err.to_string();
    for part in err.split(|c: char| c.is_whitespace() || c == ':' || c == '"' || c == '\'') {
        if part.contains('/') || part.ends_with(".jpg") || part.ends_with(".jpeg") {
            out = out.replace(part, "image");
        }
    }
    let cleaned = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() || cleaned == "image" || cleaned == "image:" {
        "Could not open image".into()
    } else {
        cleaned
    }
}

fn labeled_slider(ui: &mut egui::Ui, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>, unit: &str) {
    ui.label(RichText::new(label).size(12.0).color(MUTED));
    ui.add(egui::Slider::new(value, range).suffix(format!(" {unit}")));
    ui.add_space(6.0);
}




/// A thin horizontal bar, 0..1.
fn meter(ui: &mut egui::Ui, value: f32, color: Color32) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 6.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(3), Color32::from_rgb(44, 44, 46));
    let filled = egui::Rect::from_min_size(
        rect.min,
        Vec2::new(rect.width() * value.clamp(0.0, 1.0), rect.height()),
    );
    ui.painter().rect_filled(filled, CornerRadius::same(3), color);
}

fn quiet_btn(ui: &mut egui::Ui, text: &str) -> egui::Response {
    centered_btn(ui, text, 13.0, TEXT, BTN, 8, Vec2::new(64.0, 30.0))
}

fn filter_chip(ui: &mut egui::Ui, text: &str, on: bool) -> egui::Response {
    let w = (text.len() as f32 * 7.5 + 20.0).clamp(56.0, 100.0);
    centered_btn(
        ui,
        text,
        12.0,
        if on { Color32::WHITE } else { MUTED },
        if on { BLUE } else { BTN },
        8,
        Vec2::new(w, 28.0),
    )
}

fn format_bytes(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f32 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.0} KB", n as f32 / 1024.0)
    } else {
        format!("{n} B")
    }
}

fn fmt_ms(ms: Option<u64>) -> String {
    let Some(ms) = ms else {
        return "—".into();
    };
    fmt_dt(ms / 1000)
}

fn fmt_dt(unix: u64) -> String {
    match Local.timestamp_opt(unix as i64, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%d %b  %H:%M").to_string(),
        _ => unix.to_string(),
    }
}

fn line_chart(ui: &mut egui::Ui, values: &[u32], bucket_secs: u64) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 148.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let chart = egui::Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.max.y - 18.0));
    if values.is_empty() {
        return;
    }
    let max = values.iter().copied().max().unwrap_or(0).max(1) as f32;
    let n = values.len();
    let inner = chart.shrink(8.0);
    let pts: Vec<Pos2> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = if n == 1 {
                inner.center().x
            } else {
                inner.left() + i as f32 * inner.width() / (n as f32 - 1.0)
            };
            let y = inner.bottom() - (*v as f32 / max) * inner.height();
            Pos2::new(x, y)
        })
        .collect();
    let mut fill = pts.clone();
    if let (Some(first), Some(last)) = (fill.first().copied(), fill.last().copied()) {
        fill.push(Pos2::new(last.x, inner.bottom()));
        fill.push(Pos2::new(first.x, inner.bottom()));
        painter.add(egui::Shape::convex_polygon(fill, FILL, Stroke::NONE));
    }
    for pair in pts.windows(2) {
        painter.line_segment([pair[0], pair[1]], Stroke::new(2.2_f32, BLUE));
    }
    for p in &pts {
        painter.circle_filled(*p, 3.0, BLUE);
        painter.circle_filled(*p, 1.5, CARD);
    }
    let now = eventlog::now_unix();
    let step = bucket_secs.max(1);
    let end = (now / step) * step;
    for i in 0..n {
        let t = end.saturating_sub((n as u64 - 1 - i as u64) * step);
        let letter = match Local.timestamp_opt(t as i64, 0) {
            chrono::LocalResult::Single(dt) => {
                if bucket_secs >= 86400 {
                    dt.format("%a").to_string()
                } else {
                    dt.format("%H").to_string()
                }
            }
            _ => String::new(),
        };
        let letter = if bucket_secs >= 86400 {
            letter.chars().next().unwrap_or(' ').to_string()
        } else {
            letter
        };
        let x = if n == 1 {
            inner.center().x
        } else {
            inner.left() + i as f32 * inner.width() / (n as f32 - 1.0)
        };
        painter.text(
            Pos2::new(x, rect.bottom() - 2.0),
            egui::Align2::CENTER_BOTTOM,
            letter,
            egui::FontId::proportional(11.0),
            MUTED,
        );
    }
}

fn main() -> eframe::Result {
    load_dotenv();
    if std::env::var_os("WINIT_UNIX_BACKEND").is_none() {
        unsafe { std::env::set_var("WINIT_UNIX_BACKEND", "x11") };
    }

    // Unkey PTT before anything else — power-up must not leave the radio keyed.
    {
        let cfg = Config::load();
        silo_alert::set_ptt_pin(cfg.radio.ptt_gpio);
    }

    // Headless check of the exact path the Test radio button uses.
    if std::env::args().any(|a| a == "--test-radio") {
        match silo_alert::play_voice() {
            Ok(()) => {
                println!("Played silo is empty");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("Test radio failed: {e}");
                std::process::exit(1);
            }
        }
    }

    let cam = match camera::Cam::open(&Config::load().camera) {
        Ok(c) => Some(c),
        Err(e) => {
            // Keep running: the supervisor retries, and Train still works.
            eprintln!("camera open failed (will keep retrying): {e}");
            None
        }
    };
    eprintln!("opening Spectr Vision");

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 720.0])
            .with_title("Spectr Vision"),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        "Spectr Vision",
        opts,
        Box::new(move |_cc| Ok(Box::new(App::new(cam)))),
    )
}

/// Load KEY=VALUE lines from .env next to the binary / cwd. Does not override existing env.
fn load_dotenv() {
    let candidates = [
        std::path::PathBuf::from(".env"),
        std::path::PathBuf::from("/home/spectr/silo-alert/.env"),
    ];
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let k = k.trim();
            let v = v.trim().trim_matches('"');
            if std::env::var_os(k).is_none() {
                unsafe { std::env::set_var(k, v) };
            }
        }
        eprintln!("env: {}", path.display());
        break;
    }
}
