mod camera;
mod config;
mod eventlog;
mod host;
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
use host::{HostMonitor, HostSnapshot};
use silo_alert::SiloAlert;
use stats::Stats;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use supabase::Supabase;
use vision::{BoxNorm, Reference, SampleAnalysis, SampleMeta};

const BG: Color32 = Color32::from_rgb(0, 0, 0);
const CARD: Color32 = Color32::from_rgb(28, 28, 30);
const TEXT: Color32 = Color32::from_rgb(245, 245, 247);
const MUTED: Color32 = Color32::from_rgb(142, 142, 147);
const BLUE: Color32 = Color32::from_rgb(10, 132, 255);
const GREEN: Color32 = Color32::from_rgb(48, 209, 88);
const RED: Color32 = Color32::from_rgb(255, 69, 58);
/// Fixed width for Config text fields and dropdowns (not full page).
const CONFIG_FIELD_W: f32 = 280.0;
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

/// One terminal line. Pictures stay in Supabase, not in this buffer.
struct TermEntry {
    text: String,
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
    sample_bw_tex: Option<egui::TextureHandle>,
    sample_view: Option<SampleAnalysis>,
    dataset_dirty: bool,
    thumb_tex: HashMap<String, egui::TextureHandle>,
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
    empty_streak: u32,
    radio_fault: bool,
    last_heartbeat: Option<Instant>,
    last_log_purge: Option<Instant>,
    /// Last `vcgencmd get_throttled` value. 0 means 5 V has held since boot.
    last_throttled: Option<u32>,
    last_power_daily: Option<Instant>,
    host: HostMonitor,
    /// Last sparse "silo is full" cloud row.
    last_full_note: Option<Instant>,
    evidence_tex: Option<egui::TextureHandle>,
    evidence_path: Option<std::path::PathBuf>,
    /// Continuous-learning label on the last alert: `Some(true)` Empty, `Some(false)` Full.
    evidence_feedback: Option<bool>,
}

impl App {
    fn new(cam: Option<camera::Cam>) -> Self {
        vision::ensure_dirs();
        let cfg = Config::load();
        let mut stats = Stats::load();
        let cloud = Supabase::from_cfg(&cfg.supabase);
        let mut reference = sync_reference(cloud.as_ref(), Reference::load());
        stats.labels_empty = vision::list_empty_references().len() as u64;
        stats.labels_full = vision::list_references()
            .iter()
            .filter(|s| s.label_empty == Some(false))
            .count() as u64;
        // Older reference.json files omit Full negatives — rebuild once so
        // continuous-learning Full photos start rejecting false empties.
        if let Some(r) = &reference {
            if stats.labels_full > 0 && r.negatives.is_empty() {
                if let Ok(built) = vision::build_reference() {
                    if let Some(c) = &cloud {
                        if let Ok(bytes) = serde_json::to_vec(&built) {
                            c.upload_bytes(&c.reference_path(), bytes, "application/json");
                        }
                    }
                    reference = Some(built);
                }
            }
        }
        if reference.is_none() {
            stats.last_train_unix = None;
            stats.last_train_accuracy = None;
        }
        silo_alert::set_ptt_pin(cfg.radio.ptt_gpio);
        silo_alert::set_spken_pin(cfg.radio.spken_gpio);
        silo_alert::set_channel_pins(
            cfg.radio.ch_bit1_gpio,
            cfg.radio.ch_bit2_gpio,
            cfg.radio.ch_bit4_gpio,
            cfg.radio.ch_bit8_gpio,
        );
        let _ = silo_alert::select_channel(cfg.radio.channel);
        silo_alert::set_audio_device(&cfg.radio.audio_device);
        silo_alert::set_muted(cfg.radio.muted);
        let mut alert = SiloAlert::new();
        // Already-empty after a reboot must not look like a fresh empty.
        alert.restore(stats.empty_state, stats.last_alert_unix);
        let log_path = cfg.resolve_db_path().with_extension("jsonl");
        let text_log = cfg.resolve_db_path().with_extension("log");
        let mut empty_alerts = EventLog::load_empty_alerts(&log_path);
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
            sb.backfill_empty_image_paths();
        } else {
            eprintln!("supabase: disabled (set supabase.enabled + key, or SUPABASE_KEY)");
        }
        let resume_empty = stats.empty_state;
        let resume_empty_since = stats.empty_since_unix;
        // Old files had Arm + Pause. One flag now: running only if it was armed and not paused.
        let monitor = stats.armed && !stats.monitor_paused;
        stats.armed = monitor;
        stats.monitor_paused = !monitor;
        let radio_fault = !stats.last_radio_ok && stats.last_radio_err.is_some();
        if let Some(ref sb) = cloud {
            sb.push_event("app_start", Some(resume_empty), None);
            sb.sync_all_reference_images();
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
        let host = HostMonitor::new();
        let last_throttled = host.snapshot().and_then(|s| s.throttled);
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
            sample_bw_tex: None,
            sample_view: None,
            dataset_dirty: false,
            thumb_tex: HashMap::new(),
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
            empty_streak: 0,
            radio_fault,
            last_heartbeat: None,
            last_log_purge: None,
            last_throttled,
            last_power_daily: None,
            host,
            last_full_note: None,
            evidence_tex: None,
            evidence_path: vision::latest_alert_evidence(),
            evidence_feedback: vision::latest_alert_feedback(),
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

    /// Camera, check, radio and cloud. No window required.
    fn tick_plant(&mut self) {
        pet_watchdog();
        self.take_radio_status();
        self.pull_frame(None);
        self.tick_heartbeat();
        self.maybe_daily_power();
        self.tick_host();
    }
    /// Grab a frame and run the scheduled check. The picture is optional;
    /// headless mode passes no context and still compares and alerts.
    fn pull_frame(&mut self, ctx: Option<&egui::Context>) {
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

                if let Some(ctx) = ctx {
                    let img = egui::ColorImage::from_rgb([w as usize, h as usize], &rgb);
                    match &mut self.live_tex {
                        Some(t) => t.set(img, Default::default()),
                        None => {
                            self.live_tex =
                                Some(ctx.load_texture("live", img, Default::default()))
                        }
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
            self.run_check(w, h, &rgb);
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
                scroll_y()
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
        labeled_slider(
            ui,
            "Check every",
            "How often a live frame is compared to the empty-silo reference photos.",
            &mut self.cfg.level.check_interval_seconds,
            5.0..=120.0,
            "s",
        );
        labeled_slider(
            ui,
            "State must last",
            "Empty (before alert) and full again (before radio latch clears) must hold this long.",
            &mut self.cfg.level.empty_confirmation_seconds,
            15.0..=120.0,
            "s",
        );

        ui.add_space(4.0);
        field_label(
            ui,
            "Match threshold",
            "How closely the frame must match empty photos. Auto uses cohesion from your reference set.",
        );
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
            .allocate_ui_with_layout(
                Vec2::new(CONFIG_FIELD_W, ui.spacing().interact_size.y),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| ui.add(egui::Slider::new(&mut pct, 70.0..=99.5).suffix(" %")),
            )
            .inner
            .changed()
        {
            self.cfg.level.empty_match_threshold = pct / 100.0;
        }
        row_stat(
            ui,
            "Effective",
            &format!(
                "{:.0}% (suggest {:.0}%)",
                effective * 100.0,
                base_suggested * 100.0
            ),
        );
        if let Some(warn) = self.reference.as_ref().and_then(|r| r.cohesion_warning()) {
            ui.label(RichText::new(warn).size(11.0).color(RED));
            ui.add_space(4.0);
        }
        if self.cfg.level.empty_match_threshold > 0.0 {
            if quiet_btn(ui, "Auto").clicked() {
                self.cfg.level.empty_match_threshold = 0.0;
            }
        }

        ui.add_space(10.0);
        field_label(
            ui,
            "Reference",
            "Built from empty-silo photos on Home. Rebuild after changing the watched region.",
        );
        ui.add_space(6.0);
        match self.reference.as_ref() {
            Some(r) => {
                row_stat(ui, "Photos", &r.count().to_string());
                row_stat(
                    ui,
                    "Photos agree",
                    &format!("{:.0}%", r.cohesion * 100.0),
                );
                if let Some(warn) = r.cohesion_warning() {
                    ui.label(RichText::new(warn).size(11.0).color(RED));
                    ui.add_space(4.0);
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
                    RichText::new("No reference yet — add photos on Home.")
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
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if config_action_btn(ui, "Rebuild reference", false).clicked() {
                self.rebuild_reference();
            }
            if config_action_btn(ui, "Sync photos to cloud", false)
                .on_hover_text("Upload every Empty / Not empty photo + metadata to Supabase.")
                .clicked()
            {
                if let Some(cloud) = &self.cloud {
                    cloud.sync_all_reference_images();
                    self.note = "Syncing reference photos to Supabase…".into();
                } else {
                    self.note = "Supabase is not configured".into();
                }
            }
        });

        ui.add_space(16.0);
        field_label(
            ui,
            "Reference photos",
            "Home Empty/Full feedback on the last alert lands here. Click a photo to enlarge it. Only Empty photos build the match set; Full photos reject false empties.",
        );
        ui.add_space(8.0);
        let ctx = ui.ctx().clone();
        self.draw_library_grid(ui, &ctx);
        self.draw_selected_reference(ui);
    }

    /// Large colour + B&W preview for the photo selected in Config → Detection.
    fn draw_selected_reference(&mut self, ui: &mut egui::Ui) {
        let Some(i) = self.sample_idx.filter(|i| *i < self.samples.len()) else {
            return;
        };
        if self.sample_view.is_none() {
            return;
        }
        let meta = self.samples[i].clone();
        let n = self.samples.len();
        let num = i + 1;
        let taken = fmt_ms(meta.captured_ms);
        let is_empty = meta.label_empty != Some(false);
        let kind = if is_empty { "Empty" } else { "Not empty" };
        let (match_txt, size_txt) = {
            let view = self.sample_view.as_ref().unwrap();
            (
                view.match_score
                    .map(|m| format!("{:.0}%", m * 100.0))
                    .unwrap_or_else(|| "—".into()),
                format!("{}×{}", view.width, view.height),
            )
        };
        let empty_n = self
            .samples
            .iter()
            .filter(|s| s.label_empty != Some(false))
            .count();
        let full_n = n.saturating_sub(empty_n);

        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("Selected  ·  #{num} of {n}"))
                    .size(14.0)
                    .color(TEXT)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if quiet_btn(ui, "Close").clicked() {
                    self.sample_idx = None;
                    self.sample_view = None;
                    self.sample_tex = None;
                    self.sample_bw_tex = None;
                }
            });
        });
        ui.add_space(6.0);
        row_stat(ui, "Label", kind);
        row_stat(ui, "Taken", &taken);
        row_stat(ui, "Size", &size_txt);
        row_stat(ui, "Disk", &format_bytes(meta.bytes));
        row_stat(ui, "Match to empty set", &match_txt);
        row_stat(ui, "Library", &format!("{empty_n} empty  ·  {full_n} not empty"));

        ui.add_space(8.0);
        field_label(
            ui,
            "Silo state",
            "Empty = used for detection. Not empty = kept for reference only.",
        );
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let empty_btn = egui::Button::new(
                RichText::new("Empty")
                    .size(14.0)
                    .color(if is_empty { TEXT } else { MUTED }),
            )
            .min_size(Vec2::new(120.0, 36.0))
            .fill(if is_empty { GREEN } else { BTN });
            let full_btn = egui::Button::new(
                RichText::new("Not empty")
                    .size(14.0)
                    .color(if !is_empty { TEXT } else { MUTED }),
            )
            .min_size(Vec2::new(120.0, 36.0))
            .fill(if !is_empty { RED } else { BTN });
            if ui.add(empty_btn).clicked() && !is_empty {
                self.set_selected_empty(true);
            }
            if ui.add(full_btn).clicked() && is_empty {
                self.set_selected_empty(false);
            }
        });

        ui.add_space(10.0);
        let avail_w = ui.available_width().max(200.0);
        let pane_w = ((avail_w - 12.0) * 0.5).clamp(160.0, 520.0);
        let pane_h = (pane_w * 0.75).clamp(140.0, 400.0);
        let colour = self.sample_tex.clone();
        let bw = self.sample_bw_tex.clone();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            draw_preview_pane(ui, colour.as_ref(), "Colour", pane_w, pane_h);
            draw_preview_pane(ui, bw.as_ref(), "B&W filter", pane_w, pane_h);
        });

        ui.add_space(10.0);
        if quiet_btn(ui, "Delete photo").clicked() {
            self.delete_selected();
            self.rebuild_reference();
        }
    }

    /// Ethernet camera / Hikvision NVR over RTSP.
    fn config_camera(&mut self, ui: &mut egui::Ui) {
        self.cfg.camera.source = "rtsp".into();

        field_label(
            ui,
            "NVR host",
            "LAN IP or hostname of the Hikvision NVR (not each camera).",
        );
        config_text_field(ui, "nvr_host", &mut self.cfg.camera.host, "192.168.1.64", false);

        ui.add_space(8.0);
        field_label(ui, "RTSP port", "Usually 554. Confirm in the NVR network settings.");
        {
            let buf_id = egui::Id::new("nvr_rtsp_port_buf");
            let mut port = ui.data_mut(|d| {
                d.get_temp_mut_or_insert_with(buf_id, || self.cfg.camera.rtsp_port.to_string())
                    .clone()
            });
            let resp = config_text_field(ui, "nvr_rtsp_port", &mut port, "554", false);
            if resp.changed() {
                ui.data_mut(|d| d.insert_temp(buf_id, port.clone()));
                if let Ok(p) = port.trim().parse::<u16>() {
                    if p > 0 {
                        self.cfg.camera.rtsp_port = p;
                    }
                }
            } else if !resp.has_focus() {
                let synced = self.cfg.camera.rtsp_port.to_string();
                ui.data_mut(|d| d.insert_temp(buf_id, synced));
            }
        }

        ui.add_space(8.0);
        field_label(ui, "Username", "Prefer a view-only NVR user (not admin).");
        config_text_field(ui, "nvr_user", &mut self.cfg.camera.username, "viewer", false);

        ui.add_space(8.0);
        field_label(ui, "Password", "Stored only in local config on this Pi — not committed to git.");
        config_text_field(ui, "nvr_pass", &mut self.cfg.camera.password, "••••••••", true);

        ui.add_space(10.0);
        field_label(
            ui,
            "Camera feed",
            "Which NVR camera to watch (1–16). Your plant has live feeds on 1–14. Save reconnects to the chosen feed.",
        );
        {
            let mut ch = self.cfg.camera.channel.clamp(1, 16);
            let label = format!(
                "Camera {ch}  ·  stream {}",
                self.cfg.camera.hikvision_channel_id()
            );
            egui::ComboBox::from_id_salt("nvr_camera_feed")
                .width(CONFIG_FIELD_W)
                .selected_text(label)
                .show_ui(ui, |ui| {
                    for c in 1u8..=16 {
                        let stream_id = {
                            let suffix = if self.cfg.camera.stream.eq_ignore_ascii_case("main") {
                                1
                            } else {
                                2
                            };
                            c as u32 * 100 + suffix
                        };
                        ui.selectable_value(
                            &mut ch,
                            c,
                            format!("Camera {c}  (Channels/{stream_id})"),
                        );
                    }
                });
            self.cfg.camera.channel = ch;
        }
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
            let current = self.cfg.camera.channel.clamp(1, 16);
            for c in 1u8..=16 {
                let selected = c == current;
                let btn = egui::Button::new(
                    RichText::new(format!("{c}"))
                        .size(14.0)
                        .color(TEXT),
                )
                .min_size(Vec2::new(36.0, 32.0))
                .fill(if selected { BLUE } else { BTN })
                .stroke(egui::Stroke::new(
                    1.0_f32,
                    if selected { BLUE } else { HAIRLINE },
                ));
                if ui.add(btn).on_hover_text(format!("Camera {c}")).clicked() {
                    self.cfg.camera.channel = c;
                }
            }
        });
        ui.add_space(4.0);
        row_stat(
            ui,
            "Selected",
            &format!(
                "Camera {} → Channels/{}",
                self.cfg.camera.channel.clamp(1, 16),
                self.cfg.camera.hikvision_channel_id()
            ),
        );

        ui.add_space(8.0);
        field_label(
            ui,
            "Stream",
            "Main (…01) is HD from the NVR. Sub (…02) is low-res if the Pi needs less load.",
        );
        ui.horizontal(|ui| {
            let is_sub = !self.cfg.camera.stream.eq_ignore_ascii_case("main");
            if ui.selectable_label(is_sub, "Sub").clicked() {
                self.cfg.camera.stream = "sub".into();
            }
            if ui.selectable_label(!is_sub, "Main").clicked() {
                self.cfg.camera.stream = "main".into();
            }
        });

        ui.add_space(10.0);
        field_label(
            ui,
            "Resolved URL",
            "URL ffmpeg will open. Password shown as ***.",
        );
        ui.label(
            RichText::new(self.cfg.camera.preview_rtsp_url())
                .size(12.0)
                .color(TEXT)
                .monospace(),
        );

        ui.add_space(10.0);
        ui.checkbox(&mut self.cfg.camera.use_custom_url, "Use custom RTSP URL")
            .on_hover_text("Override Hikvision URL builder with a full rtsp:// address.");
        if self.cfg.camera.use_custom_url {
            ui.add_space(4.0);
            config_text_field(
                ui,
                "nvr_custom_url",
                &mut self.cfg.camera.rtsp_url,
                "rtsp://user:pass@host:554/Streaming/Channels/102",
                false,
            );
        }

        ui.add_space(10.0);
        field_label(
            ui,
            "Resolution",
            "Decode size after ffmpeg scale. 1280×720 is a good HD default; main stream source is up to 2560×1440.",
        );
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
        field_label(
            ui,
            "Decode rate",
            "Live-view frames per second from ffmpeg. NVR source is ~40 fps; 10 is smooth on the Pi. Detection still runs on its own interval.",
        );
        ui.allocate_ui_with_layout(
            Vec2::new(CONFIG_FIELD_W, ui.spacing().interact_size.y),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add(egui::Slider::new(&mut fps, 1.0..=25.0).integer().suffix(" fps"));
            },
        );
        self.cfg.camera.fps = fps as u32;

        ui.add_space(14.0);
        field_label(ui, "Status", "Live connection state for the Ethernet/RTSP camera.");
        ui.add_space(6.0);
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
        row_stat(
            ui,
            "Channel id",
            &self.cfg.camera.hikvision_channel_id().to_string(),
        );
    }

    /// SA828 intercom settings matched to the Peltor LiteCom Pro III headset.
    fn config_radio(&mut self, ui: &mut egui::Ui) {
        // Sync channel from stored frequency if needed.
        self.cfg.radio.channel = peltor::clamp_channel(self.cfg.radio.channel);
        if self.cfg.radio.channel == 0 {
            self.cfg.radio.channel = peltor::channel_for_freq(&self.cfg.radio.frequency_mhz);
        }

        field_label(
            ui,
            "Channel",
            "Peltor analog PMR446 channel 1–16. Drives SA828 pins 4, 5, 7 and 6 active-low (same as the channel knob). UART only stores the 16-slot plan. Digital DMR is not used.",
        );
        let mut ch = self.cfg.radio.channel;
        egui::ComboBox::from_id_salt("peltor_channel")
            .width(CONFIG_FIELD_W)
            .selected_text(peltor::channel_label(ch))
            .show_ui(ui, |ui| {
                for &(c, _) in &peltor::ANALOG_CHANNELS {
                    ui.selectable_value(&mut ch, c, peltor::channel_label(c));
                }
            });
        if ch != self.cfg.radio.channel {
            self.cfg.radio.channel = ch;
            self.apply_peltor_channel();
            let _ = self.cfg.save();
            match silo_alert::select_channel(ch) {
                Ok(()) => {
                    self.term_radio(&format!("Ch {ch} selected"));
                    self.note.clear();
                }
                Err(e) => {
                    self.term_radio(&e);
                    self.note = e;
                }
            }
        }
        ui.add_space(4.0);
        row_stat(ui, "SA828 freq", &self.cfg.radio.frequency_mhz);
        ui.add_space(10.0);

        {
            let mut sq = self.cfg.radio.squelch as f32;
            field_label(
                ui,
                "Squelch",
                "0 = open (hiss). 1 = normal. Higher = quieter until a strong signal.",
            );
            let sq_resp = ui
                .allocate_ui_with_layout(
                    Vec2::new(CONFIG_FIELD_W, ui.spacing().interact_size.y),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| ui.add(egui::Slider::new(&mut sq, 0.0..=8.0).integer()),
                )
                .inner;
            let new_sq = sq as u8;
            if new_sq != self.cfg.radio.squelch {
                self.cfg.radio.squelch = new_sq;
            }
            if sq_resp.drag_stopped() {
                let _ = self.program_sender();
            }
        }
        ui.add_space(10.0);

        self.cfg.radio.ctcss = peltor::clamp_ctcss(self.cfg.radio.ctcss);
        let prev_ctcss = self.cfg.radio.ctcss;
        field_label(
            ui,
            "CTCSS",
            "Privacy tone. Must match the headset if it uses CTCSS. Off = 0.",
        );
        egui::ComboBox::from_id_salt("peltor_ctcss")
            .width(CONFIG_FIELD_W)
            .selected_text(peltor::ctcss_label(self.cfg.radio.ctcss))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.cfg.radio.ctcss, 0, "Off");
                for &(idx, _) in &peltor::CTCSS_TONES {
                    let label = peltor::ctcss_label(idx);
                    ui.selectable_value(&mut self.cfg.radio.ctcss, idx, label);
                }
            });
        if self.cfg.radio.ctcss != prev_ctcss {
            if !self.program_sender() {
                self.cfg.radio.ctcss = prev_ctcss;
            }
        }

        ui.add_space(10.0);
        field_label(
            ui,
            "UART port",
            "Serial device for Program / Read. Pi 5 GPIO 14/15 is /dev/ttyAMA0 (not /dev/serial0).",
        );
        config_text_field(ui, "radio_uart", &mut self.cfg.radio.uart_port, "/dev/serial0", false);
        ui.add_space(10.0);
        {
            let mut ptt = self.cfg.radio.ptt_gpio as f32;
            field_label(
                ui,
                "PTT GPIO",
                "BCM pin to SA828 PTT (pin 20). 0 = off. LOW = TX, idle = High-Z (never drive 3.3 V).",
            );
            ui.allocate_ui_with_layout(
                Vec2::new(CONFIG_FIELD_W, ui.spacing().interact_size.y),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.add(egui::Slider::new(&mut ptt, 0.0..=27.0).integer());
                },
            );
            self.cfg.radio.ptt_gpio = ptt as u8;
            silo_alert::set_ptt_pin(self.cfg.radio.ptt_gpio);
        }
        ui.add_space(10.0);
        {
            let mut spk = self.cfg.radio.spken_gpio as f32;
            field_label(
                ui,
                "SPKEN GPIO",
                "Optional. SA828 SPKEN (pin 9) → this BCM pin. High = channel busy; waits before TX. 0 = off. Use 3.3 V module supply or a level shifter.",
            );
            ui.allocate_ui_with_layout(
                Vec2::new(CONFIG_FIELD_W, ui.spacing().interact_size.y),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.add(egui::Slider::new(&mut spk, 0.0..=27.0).integer());
                },
            );
            let new_spk = spk as u8;
            if new_spk != self.cfg.radio.spken_gpio {
                self.cfg.radio.spken_gpio = new_spk;
                silo_alert::set_spken_pin(new_spk);
            }
            ui.add_space(4.0);
            line_lamp(ui);
        }
        ui.add_space(10.0);
        field_label(
            ui,
            "Audio device",
            "ALSA device for the voice clip (3.5 mm → MIC). Volume is set on the Pi system, not by the app.",
        );
        config_text_field(
            ui,
            "radio_audio",
            &mut self.cfg.radio.audio_device,
            "plughw:CARD=Headphones,DEV=0",
            false,
        );
        ui.add_space(10.0);
        if ui
            .checkbox(&mut self.cfg.radio.muted, "Mute radio")
            .on_hover_text("Skip PTT + voice on empty alerts (Test radio still works).")
            .changed()
        {
            silo_alert::set_muted(self.cfg.radio.muted);
        }

        ui.add_space(10.0);
        ui.checkbox(
            &mut self.cfg.radio.volume_test_mode,
            "Volume test sounds",
        )
        .on_hover_text("Test radio plays a clip from audio/volume_tests/ instead of the production WAV.");
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
                field_label(
                    ui,
                    "Test clip",
                    "Only used by Test radio. Empty alerts still use the normal production clip.",
                );
                egui::ComboBox::from_id_salt("volume_test_clip")
                    .width(CONFIG_FIELD_W)
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
        let test_tip = if self.cfg.radio.volume_test_mode {
            "PTT LOW → play selected volume-test WAV → High-Z idle."
        } else {
            "PTT LOW → play production WAV once → High-Z idle."
        };
        if config_action_btn(ui, "Test radio", false)
            .on_hover_text(test_tip)
            .clicked()
        {
            self.test_radio();
        }
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
        field_label(
            ui,
            "Site ID",
            "Identity for Supabase events and stills (e.g. spectr-pi).",
        );
        config_text_field(ui, "supabase_site", &mut self.cfg.supabase.site_id, "spectr-pi", false);
        ui.add_space(12.0);

        ui.checkbox(&mut self.cfg.supabase.enabled, "Send events to Supabase")
            .on_hover_text(
                "Empty alerts → silo_empty (ts + image_path). Stills → silo-frames/{site}/alerts/{unix}.jpg. Offline rows stay queued.",
            );
        ui.add_space(10.0);
        row_stat(ui, "Cloud", &cloud_status(self.cloud.is_some()));
        row_stat(ui, "Queued", &supabase::pending().to_string());
        if supabase::schema_missing() {
            ui.add_space(8.0);
            ui.label(
                RichText::new("Supabase tables are missing. Run sql/supabase_silo.sql in the SQL Editor.")
                    .size(12.0)
                    .color(RED),
            );
        } else if let Some(err) = supabase::last_error() {
            ui.add_space(8.0);
            ui.label(RichText::new(err).size(11.0).color(RED));
        }

        ui.add_space(16.0);
        field_label(ui, "Monitoring", "Counters for this running session.");
        ui.add_space(6.0);
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
        field_label(ui, "Storage", "Local log path and number of reference images on disk.");
        ui.add_space(6.0);
        row_stat(ui, "Log", &self.log_path.display().to_string());
        row_stat(ui, "Images", &self.samples.len().to_string());
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
            if self.monitor { "Running" } else { "Stopped" },
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

    fn config_main(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(CARD)
            .corner_radius(16)
            .inner_margin(egui::Margin::same(24))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                scroll_y()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        match self.config_tab {
                            ConfigTab::Detection => self.config_detection(ui),
                            ConfigTab::Camera => self.config_camera(ui),
                            ConfigTab::Radio => self.config_radio(ui),
                            ConfigTab::App => self.config_app(ui),
                        }

                        ui.add_space(18.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            let save_tip = if self.config_tab == ConfigTab::Radio {
                                "Save settings, program the SA828, and read it back."
                            } else {
                                "Write settings to config.yaml."
                            };
                            if config_action_btn(ui, "Save", true)
                                .on_hover_text(save_tip)
                                .clicked()
                            {
                                let camera_tab = self.config_tab == ConfigTab::Camera;
                                self.save_config();
                                if camera_tab {
                                    self.reconnect_camera();
                                    if self.note.starts_with("Saved") || self.note.is_empty() {
                                        self.note = format!(
                                            "Saved — watching Camera {}",
                                            self.cfg.camera.channel.clamp(1, 16)
                                        );
                                    }
                                }
                            }
                            if self.config_tab == ConfigTab::Camera
                                && config_action_btn(ui, "Reconnect camera", false)
                                    .on_hover_text(
                                        "Restart ffmpeg against the current host / URL settings.",
                                    )
                                    .clicked()
                            {
                                self.reconnect_camera();
                            }
                        });
                        ui.add_space(10.0);
                        note_line(ui, &self.note);
                        ui.add_space(8.0);
                    });
            });
    }

    fn config_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Configure").size(22.0).color(TEXT).strong());
        ui.add_space(12.0);
        ui.spacing_mut().item_spacing.y = 6.0;
        for (t, label) in [
            (ConfigTab::Detection, "Detection"),
            (ConfigTab::Camera, "Camera"),
            (ConfigTab::Radio, "Radio"),
            (ConfigTab::App, "App"),
        ] {
            if config_tab_btn(ui, label, self.config_tab == t).clicked() {
                self.config_tab = t;
            }
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
                scroll_y()
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

    /// The frame that was just compared. Cloud keeps it. Disk does not,
    /// unless this check is the one that raises the empty alert.
    fn store_compared_frame(&self, w: u32, h: u32, rgb: &[u8], alert_unix: Option<u64>) {
        let Some(cloud) = &self.cloud else {
            return;
        };
        if let Ok(jpeg) = vision::rgb_to_jpeg_bytes(w, h, rgb, 75) {
            cloud.upload_jpeg(&cloud.latest_frame_path(), jpeg);
        }
        let Some(unix) = alert_unix else {
            return;
        };
        if let Ok(jpeg) = vision::rgb_to_jpeg_bytes(w, h, rgb, 75) {
            cloud.upload_jpeg(&cloud.alert_frame_path(unix), jpeg);
        }
    }

    fn term_line(&mut self, line: String) {
        append_log(&self.log_path, &line);
        self.terminal.push_back(TermEntry { text: line });
        while self.terminal.len() > TERM_MAX {
            self.terminal.pop_front();
        }
    }

    fn term_now(prefix: &str) -> String {
        let now = Local::now();
        format!("{}  {prefix}", now.format("%H:%M:%S"))
    }

    /// Multi-line radio / UART status → bottom terminal (not the Save note).
    fn term_radio(&mut self, msg: &str) {
        for line in msg.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            self.term_line(Self::term_now(&format!("radio  {line}")));
        }
    }

    /// One Live cycle: model decides empty/full from training only (no human confirm).
    fn run_check(&mut self, w: u32, h: u32, rgb: &[u8]) {
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
        let neg = reference.negative_similarity_pair(&feat, &feat_bw);
        self.last_match = Some(score);

        let threshold = self.match_threshold();
        // Enter empty at `threshold`; leave only clearly below it (hysteresis)
        // so flicker on the line cannot clear the radio latch in one frame.
        // Full feedback photos (negatives) must not look closer than empty.
        const CLEAR_HYSTERESIS: f32 = 0.03;
        const NEG_MARGIN: f32 = 0.02;
        let closer_to_full = !reference.negatives.is_empty() && neg + NEG_MARGIN >= score;
        let looks_empty = score >= threshold && !closer_to_full;
        let looks_full =
            score < (threshold - CLEAR_HYSTERESIS).max(0.0) || closer_to_full;

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

        let just_confirmed = self.monitor && confirmed && !self.empty;
        let just_filled = self.monitor && filled_confirmed && self.empty;

        let mut alerted = false;
        if just_confirmed {
            self.empty = true;
            self.events.empty_alert(unix);
            self.empty_alerts.push(unix);
            self.events.state(unix, "level_state", "OK", "EMPTY", "");
            if let Some(cloud) = &self.cloud {
                cloud.push_empty_alert(unix);
            }
            self.store_compared_frame(w, h, rgb, Some(unix));
            if let Ok(path) =
                vision::save_alert_evidence(w, h, rgb, roi, unix, score, threshold, self.empty_streak)
            {
                self.evidence_path = Some(path);
                self.evidence_tex = None;
                self.evidence_feedback = None;
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
            if let Some(cloud) = &self.cloud {
                cloud.push_event("full", Some(false), Some(score));
            }
        }

        if !just_confirmed {
            self.store_compared_frame(w, h, rgb, None);
        }

        if just_filled {
            self.last_full_note = Some(Instant::now());
        } else if !looks_empty {
            self.note_full_day();
        }

        let kind = if looks_empty { "empty" } else { "full" };
        let cooldown = looks_empty && self.empty && !alerted;
        let conf = self
            .confidence()
            .map(|c| format!("  conf {:.0}%", c * 100.0))
            .unwrap_or_default();
        self.term_line(format!(
            "{ts}  check  {kind}  match {pct:.0}%{conf}{}",
            if cooldown { "  (already alerted)" } else { "" }
        ));
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
                self.handle_region_drag(ui, &resp, img);
                self.draw_region_overlay(ui, img);
            }
            if self.marking_region {
                ui.painter().text(
                    Pos2::new(rect.left() + 12.0, rect.bottom() - 16.0),
                    egui::Align2::LEFT_BOTTOM,
                    "Drag a box on the view",
                    egui::FontId::proportional(12.0),
                    BLUE,
                );
            }

            if !split {
                return;
            }

            // —— What the comparison sees ——
            pane_with_caption(
                ui,
                pane,
                "FILTER",
                self.model_tex.as_ref(),
                "no frame yet",
            );
        });
    }

    fn begin_mark_region(&mut self) {
        if self.region.is_some() {
            self.clear_region();
        }
        self.marking_region = true;
    }

    fn cancel_mark_region(&mut self) {
        self.marking_region = false;
        self.box_drag_start = None;
        self.box_drag_now = None;
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

    /// Where the empty-match line sits: operator setting, or auto from
    /// how tightly the reference photos agree.
    fn match_threshold(&self) -> f32 {
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
            Ok(path) => {
                self.dataset_dirty = true;
                self.refresh_samples();
                if let Some(cloud) = &self.cloud {
                    if let Some(meta) = self
                        .samples
                        .iter()
                        .find(|s| s.path == path)
                        .cloned()
                    {
                        let sb = cloud.clone();
                        std::thread::spawn(move || sb.sync_reference_image(&meta));
                    }
                }
                self.rebuild_reference();
            }
            Err(e) => self.note = format!("Capture failed: {e}"),
        }
    }

    fn publish_reference(&self) {
        let Some(cloud) = &self.cloud else {
            return;
        };
        let Some(reference) = &self.reference else {
            return;
        };
        let Ok(bytes) = serde_json::to_vec(reference) else {
            return;
        };
        cloud.upload_bytes(&cloud.reference_path(), bytes, "application/json");
    }

    /// Recompute the reference from every photo on disk, then send it up.
    fn rebuild_reference(&mut self) {
        match vision::build_reference() {
            Ok(r) => {
                let n = r.count();
                self.stats.labels_empty = n as u64;
                self.stats.labels_full = self
                    .samples
                    .iter()
                    .filter(|s| s.label_empty == Some(false))
                    .count() as u64;
                self.stats.last_train_unix = Some(r.built_at_unix);
                self.stats.last_train_accuracy = Some(r.cohesion);
                self.stats.save();
                let warn = r.cohesion_warning();
                self.reference = Some(r);
                self.publish_reference();
                if let Some(cloud) = &self.cloud {
                    cloud.sync_all_reference_images();
                }
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

    /// Program SA828 TX/RX to the current channel frequency.
    /// Returns true only when UART readback confirms the new frequency.
    fn program_sender(&mut self) -> bool {
        self.apply_peltor_channel();
        let ch = self.cfg.radio.channel;
        match sa828::program_plan(
            self.cfg.radio.squelch,
            self.cfg.radio.ctcss,
            &self.cfg.radio.uart_port,
        ) {
            Ok(msg) => {
                let pins = silo_alert::select_channel(ch);
                let _ = self.cfg.save();
                let detail = match pins {
                    Ok(()) => format!("Ch {ch} selected.\n{msg}"),
                    Err(e) => format!("{msg}\n{e}"),
                };
                self.term_radio(&detail);
                self.note.clear();
                true
            }
            Err(e) => {
                self.term_radio(&format!("SA828 program failed (Ch {ch} not applied): {e}"));
                self.note.clear();
                false
            }
        }
    }

    fn save_config(&mut self) {
        self.apply_peltor_channel();
        self.cfg.radio.ctcss = peltor::clamp_ctcss(self.cfg.radio.ctcss);
        silo_alert::set_ptt_pin(self.cfg.radio.ptt_gpio);
        silo_alert::set_spken_pin(self.cfg.radio.spken_gpio);
        silo_alert::set_channel_pins(
            self.cfg.radio.ch_bit1_gpio,
            self.cfg.radio.ch_bit2_gpio,
            self.cfg.radio.ch_bit4_gpio,
            self.cfg.radio.ch_bit8_gpio,
        );
        let _ = silo_alert::select_channel(self.cfg.radio.channel);
        silo_alert::set_audio_device(&self.cfg.radio.audio_device);
        silo_alert::set_muted(self.cfg.radio.muted);
        match self.cfg.save() {
            Ok(()) => {
                // One Save: write config, program SA828, then read it back.
                match sa828::program_plan(
                    self.cfg.radio.squelch,
                    self.cfg.radio.ctcss,
                    &self.cfg.radio.uart_port,
                ) {
                    Ok(msg) => {
                        self.term_radio(&format!(
                            "Ch {} selected.\n{msg}",
                            self.cfg.radio.channel
                        ));
                        match sa828::read(&self.cfg.radio.uart_port) {
                            Ok(readback) => {
                                self.term_radio(&readback);
                                self.note = "Saved — module programmed and verified.".into();
                            }
                            Err(e) => {
                                self.term_radio(&e);
                                self.note =
                                    "Saved and programmed (readback failed — see terminal)."
                                        .into();
                            }
                        }
                    }
                    Err(e) => {
                        self.term_radio(&format!("SA828 program failed: {e}"));
                        self.note = "Saved (radio program failed — see terminal).".into();
                    }
                }
            }
            Err(e) => self.note = e,
        }
    }

    fn set_monitoring(&mut self, on: bool) {
        if on && !self.can_start() {
            self.note = "Mark a region and take empty photos first".into();
            return;
        }
        self.monitor = on;
        self.stats.armed = on;
        self.stats.monitor_paused = !on;
        if on {
            self.stats.wizard_done = true;
            self.term_line(Self::term_now("monitoring started"));
            if let Some(cloud) = &self.cloud {
                cloud.push_event("armed", Some(self.empty), None);
            }
        } else {
            self.term_line(Self::term_now("monitoring stopped"));
            // Do not clear announce latch — start again while still empty must not re-TX.
            if let Some(cloud) = &self.cloud {
                cloud.push_event("disarmed", Some(self.empty), None);
            }
        }
        self.stats.save();
    }

    fn can_start(&self) -> bool {
        self.region.is_some() && self.reference.as_ref().map(|r| r.count() >= 1).unwrap_or(false)
    }

    fn wizard_step(&self) -> Option<&'static str> {
        if self.monitor || self.stats.wizard_done {
            return None;
        }
        if self.region.is_none() {
            return Some("1  Mark the region");
        }
        let empty_n = self
            .samples
            .iter()
            .filter(|s| s.label_empty != Some(false))
            .count();
        if empty_n < 3 {
            return Some("2  Take 3 empty photos");
        }
        Some("3  Start when it looks right")
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

    /// Continuous learning: label the last alert Empty or Full and fine-tune.
    fn label_last_alert(&mut self, empty: bool) {
        let Some(thumb) = self
            .evidence_path
            .clone()
            .or_else(vision::latest_alert_evidence)
        else {
            self.note = "No alert to label".into();
            return;
        };

        // Prefer the full train frame. Older alerts only kept a ROI thumb —
        // recover a full frame from memory or cloud before labeling.
        if let Some(unix) = thumb
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
        {
            let train = std::path::PathBuf::from(vision::ALERTS_DIR)
                .join(format!("{unix}_train.jpg"));
            if !train.is_file() {
                let mut wrote = false;
                if let Some((w, h, rgb)) = self.last_rgb.as_ref() {
                    wrote = vision::save_rgb_jpeg(
                        train.to_str().unwrap_or("data/tmp_alert_train.jpg"),
                        *w,
                        *h,
                        rgb,
                    )
                    .is_ok();
                }
                if !wrote {
                    if let Some(cloud) = &self.cloud {
                        if let Ok(bytes) = cloud.download_object(&cloud.alert_frame_path(unix)) {
                            if let Ok(img) = image::load_from_memory(&bytes) {
                                let rgb8 = img.to_rgb8();
                                let (w, h) = rgb8.dimensions();
                                wrote = vision::save_rgb_jpeg(
                                    train.to_str().unwrap_or("data/tmp_alert_train.jpg"),
                                    w,
                                    h,
                                    &rgb8.into_raw(),
                                )
                                .is_ok();
                            }
                        }
                    }
                }
                if !wrote {
                    self.note =
                        "No full alert frame to train on — label the next alert instead".into();
                    return;
                }
            }
        }

        match vision::label_alert_at(&thumb, empty) {
            Ok(path) => {
                self.dataset_dirty = true;
                self.refresh_samples();

                // Select the new photo in Config → Detection library.
                let file_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                if let Some(i) = self
                    .samples
                    .iter()
                    .position(|s| s.file_name == file_name || s.path == path)
                {
                    self.load_sample_preview(i);
                }

                // Push colour + B&W + silo_reference_images row to Supabase.
                if let Some(cloud) = &self.cloud {
                    if let Some(meta) = self
                        .samples
                        .iter()
                        .find(|s| s.file_name == file_name || s.path == path)
                        .cloned()
                    {
                        let sb = cloud.clone();
                        std::thread::spawn(move || {
                            sb.sync_reference_image(&meta);
                            eprintln!(
                                "supabase: feedback {} → {}",
                                if empty { "empty" } else { "full" },
                                meta.file_name
                            );
                        });
                    }
                }

                if !empty {
                    // False empty alert — clear the sticky latch immediately.
                    self.empty = false;
                    self.empty_since = None;
                    self.empty_streak = 0;
                    self.full_streak = 0;
                    self.full_since = None;
                    self.stats.empty_state = false;
                    self.stats.empty_since_unix = None;
                    self.stats.save();
                    if let Some(alert) = &mut self.alert {
                        alert.on_filled();
                    }
                    if let Some(cloud) = &self.cloud {
                        cloud.push_event("full", Some(false), self.last_match);
                    }
                }

                // Rebuild model (includes Full negatives) and sync whole library.
                self.rebuild_reference();

                // Hide Last alert on Home — training is done for this frame.
                self.evidence_path = None;
                self.evidence_tex = None;
                self.evidence_feedback = Some(empty);

                let kind = if empty { "Empty" } else { "Full" };
                self.note = format!(
                    "Saved as {kind} — image sent to Config → Detection, model trained"
                );
                self.term_line(Self::term_now(&format!(
                    "feedback  {} → Config + model trained ({})",
                    if empty { "empty" } else { "full" },
                    file_name
                )));
                eprintln!(
                    "feedback: labeled last alert as {} ({})",
                    if empty { "empty" } else { "full" },
                    file_name
                );
            }
            Err(e) => {
                eprintln!("feedback: label failed: {e}");
                self.note = e;
            }
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
        if !self.monitor {
            return PlantState::Stopped;
        }
        if self.empty {
            PlantState::Empty
        } else {
            PlantState::Ok
        }
    }

    fn tick_host(&mut self) {
        self.host.tick();
        if let Some(bits) = self.host.snapshot().and_then(|s| s.throttled) {
            self.last_throttled = Some(bits);
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
        self.purge_old_logs();
    }

    fn purge_old_logs(&mut self) {
        let due = self
            .last_log_purge
            .map(|t| t.elapsed() >= Duration::from_secs(24 * 60 * 60))
            .unwrap_or(true);
        if !due {
            return;
        }
        self.last_log_purge = Some(Instant::now());
        if let Some(cloud) = &self.cloud {
            cloud.purge_expired_logs();
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
        self.report_power();
    }

    /// Read the Pi throttled register and store it in Supabase as kind `power`.
    fn report_power(&mut self) {
        let Some(bits) = host::read_throttled() else {
            return;
        };
        self.last_throttled = Some(bits);
        if let Some(cloud) = &self.cloud {
            cloud.push_power(bits);
        }
        if bits != 0 {
            self.term_line(Self::term_now(&format!("power  throttled 0x{bits:x}")));
        }
    }

    fn maybe_daily_power(&mut self) {
        let due = self
            .last_power_daily
            .map(|t| t.elapsed() >= Duration::from_secs(24 * 60 * 60))
            .unwrap_or(true);
        if !due {
            return;
        }
        self.last_power_daily = Some(Instant::now());
        self.report_power();
    }

    /// One row a day while the silo is full, so the day has a timestamp
    /// without a row for every check.
    fn note_full_day(&mut self) {
        if !self.monitor || self.empty || self.cam_down_since.is_some() {
            return;
        }
        let due = self
            .last_full_note
            .map(|t| t.elapsed() >= Duration::from_secs(24 * 60 * 60))
            .unwrap_or(true);
        if !due {
            return;
        }
        self.last_full_note = Some(Instant::now());
        if let Some(cloud) = &self.cloud {
            cloud.push_event("full", Some(false), self.last_match);
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
            self.sample_bw_tex = None;
            self.sample_view = None;
        }
        self.stats.labels_empty = self
            .samples
            .iter()
            .filter(|s| s.label_empty != Some(false))
            .count() as u64;
        self.stats.labels_full = self
            .samples
            .iter()
            .filter(|s| s.label_empty == Some(false))
            .count() as u64;
    }

    /// Label the selected photo Empty or Not empty and rebuild the match set.
    fn set_selected_empty(&mut self, empty: bool) {
        let Some(idx) = self.sample_idx.filter(|i| *i < self.samples.len()) else {
            return;
        };
        let meta = self.samples[idx].clone();
        match vision::set_sample_empty(&meta.path, empty) {
            Ok(_) => {
                let keep = meta.file_name.clone();
                self.refresh_samples();
                if let Some(i) = self.samples.iter().position(|s| s.file_name == keep) {
                    self.load_sample_preview(i);
                }
                if let Some(cloud) = &self.cloud {
                    if let Some(updated) = self.samples.iter().find(|s| s.file_name == keep).cloned()
                    {
                        let sb = cloud.clone();
                        std::thread::spawn(move || sb.sync_reference_image(&updated));
                    }
                }
                self.rebuild_reference();
                self.note = if empty {
                    "Labeled Empty — used for detection".into()
                } else {
                    "Labeled Not empty — kept for reference only".into()
                };
            }
            Err(e) => self.note = e,
        }
    }

    fn load_sample_preview(&mut self, idx: usize) {
        if idx >= self.samples.len() {
            return;
        }
        self.sample_idx = Some(idx);
        let meta = self.samples[idx].clone();
        // Force textures to refresh for the newly selected photo.
        self.sample_tex = None;
        self.sample_bw_tex = None;
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
        if self.sample_tex.is_none() {
            let img = egui::ColorImage::from_rgb(
                [view.width as usize, view.height as usize],
                &view.rgb,
            );
            self.sample_tex = Some(ctx.load_texture("sample", img, Default::default()));
        }
        if self.sample_bw_tex.is_none() {
            if let Some(i) = self.sample_idx {
                if let Some(meta) = self.samples.get(i) {
                    let bw_path = vision::bw_path_for(&meta.file_name);
                    if let Ok((w, h, rgba)) = vision::load_thumb_rgba(&bw_path, 960) {
                        let img = egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            &rgba,
                        );
                        self.sample_bw_tex =
                            Some(ctx.load_texture("sample_bw", img, Default::default()));
                    }
                }
            }
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
                if let Some(cloud) = &self.cloud {
                    cloud.delete_reference_image(&meta.file_name);
                }
                self.thumb_tex.remove(&key);
                self.thumb_tex.remove(&format!("bw:{}", meta.file_name));
                self.sample_view = None;
                self.sample_tex = None;
                self.sample_bw_tex = None;
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

    /// Every empty-silo photo, camera view on top and B&W below.
    fn draw_library_grid(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let idxs: Vec<usize> = (0..self.samples.len()).collect();
        for &i in &idxs {
            let meta = self.samples[i].clone();
            self.ensure_thumb(ctx, &meta);
        }

        let mut clicked: Option<usize> = None;
        let total = idxs.len();
        ui.label(
            RichText::new(format!("{total} images"))
                .size(12.0)
                .color(MUTED),
        );
        ui.add_space(10.0);

        if idxs.is_empty() {
            ui.label(
                RichText::new("Nothing here — take reference photos on Home.")
                    .size(13.0)
                    .color(MUTED),
            );
            return;
        }

        let cell_w = 160.0;
        let cell_h = 230.0;
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
                    let state = if meta.label_empty == Some(false) {
                        "Not empty"
                    } else {
                        "Empty"
                    };
                    let caption = format!("{state}  ·  {}", fmt_ms(meta.captured_ms));
                    let (rect, resp) =
                        ui.allocate_exact_size(Vec2::new(cell_w, cell_h), Sense::click());
                    ui.painter().rect_filled(
                        rect,
                        CornerRadius::same(10),
                        Color32::from_rgb(18, 18, 20),
                    );

                    let half = (cell_h - 52.0) * 0.5;
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
                        let img_rect = egui::Rect::from_center_size(slot.center(), draw);
                        ui.painter().image(tex.id(), img_rect, uv, Color32::WHITE);
                    }

                    ui.painter().text(
                        Pos2::new(rect.left() + 10.0, rect.bottom() - 28.0),
                        egui::Align2::LEFT_CENTER,
                        format!("#{}", i + 1),
                        egui::FontId::proportional(11.0),
                        BLUE,
                    );
                    ui.painter().text(
                        Pos2::new(rect.left() + 10.0, rect.bottom() - 12.0),
                        egui::Align2::LEFT_CENTER,
                        truncate_chars(&caption, 22),
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
    Stopped,
    Empty,
    Ok,
}

impl PlantState {
    fn label(self) -> &'static str {
        match self {
            PlantState::CameraFault => "CAMERA FAULT",
            PlantState::RadioFault => "RADIO FAULT",
            PlantState::Stopped => "STOPPED",
            PlantState::Empty => "EMPTY",
            PlantState::Ok => "OK",
        }
    }

    fn color(self) -> Color32 {
        match self {
            PlantState::CameraFault | PlantState::RadioFault | PlantState::Empty => RED,
            PlantState::Stopped => MUTED,
            PlantState::Ok => TEXT,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        pet_watchdog();
        // Linux IME events can swallow TextEdit keystrokes (host/user/password).
        ctx.input_mut(|i| {
            i.events
                .retain(|e| !matches!(e, egui::Event::Ime(_)));
        });
        style(ctx);
        self.ensure_logo(ctx);
        self.take_radio_status();
        if self.dataset_dirty {
            self.refresh_samples();
        }
        self.pull_frame(Some(ctx));
        self.sync_model_view(ctx);
        self.tick_heartbeat();
        self.maybe_daily_power();
        self.tick_host();
        if self.page == Page::Config
            && self.config_tab == ConfigTab::Detection
            && self.sample_idx.is_some()
        {
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
                let mut toggle_monitor = false;
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
                    // Green play / red pause — always on the right of the top bar.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(14.0);
                        if monitor_play_btn(ui, self.monitor).clicked() {
                            toggle_monitor = true;
                        }
                    });
                });
                if toggle_monitor {
                    self.set_monitoring(!self.monitor);
                }
                // Bottom hairline across the bar
                let r = ui.max_rect();
                ui.painter()
                    .hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0_f32, HAIRLINE));
            });

        if self.page == Page::Config {
            egui::SidePanel::left("config_side")
                .exact_width(168.0)
                .resizable(false)
                .frame(
                    egui::Frame::new()
                        .fill(CARD)
                        .inner_margin(egui::Margin {
                            left: 14,
                            right: 14,
                            top: 22,
                            bottom: 22,
                        }),
                )
                .show(ctx, |ui| {
                    self.config_sidebar(ui);
                });
        }

        if self.page != Page::Config {
            egui::SidePanel::right("side")
                .exact_width(356.0)
                .resizable(false)
                .frame(
                    egui::Frame::new()
                        .fill(CARD)
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
                        scroll_y()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                        let interval = self.cfg.level.check_interval_seconds.max(1.0);
                        let next_in = self
                            .last_check_at
                            .map(|t| (interval - t.elapsed().as_secs_f32()).max(0.0).ceil() as u32)
                            .unwrap_or(0);
                        let state = self.plant_state();

                        ui.label(RichText::new("Home").size(22.0).color(TEXT).strong());
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
                        ui.add_space(14.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(8.0, 0.0);
                            let side = 40.0;
                            if self.marking_region {
                                if icon_btn(ui, side, true, LiveIcon::Cancel, "Cancel mark")
                                    .clicked()
                                {
                                    self.cancel_mark_region();
                                }
                            } else if icon_btn(ui, side, false, LiveIcon::Crop, "Mark region")
                                .clicked()
                            {
                                self.begin_mark_region();
                            }
                            if self.region.is_some() && !self.marking_region {
                                if icon_btn(ui, side, false, LiveIcon::Clear, "Clear region")
                                    .clicked()
                                {
                                    self.clear_region();
                                }
                            }
                            if icon_btn(
                                ui,
                                side,
                                self.show_model_view,
                                LiveIcon::Filter,
                                "Filter",
                            )
                            .clicked()
                            {
                                self.show_model_view = !self.show_model_view;
                            }
                        });

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
                                        ui.spacing_mut().item_spacing.x = 12.0;
                                        ui.label(
                                            RichText::new("Confidence")
                                                .size(12.0)
                                                .color(MUTED),
                                        );
                                        ui.label(
                                            RichText::new(format!("{:.0}%", c * 100.0))
                                                .size(12.0)
                                                .color(TEXT),
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

                        if self.empty && self.monitor {
                            ui.add_space(12.0);
                            row_stat(ui, "Alert", "Sent for this empty");
                        }

                        // Already-labeled alerts stay off Home; only unlabeled
                        // evidence asks Empty/Full for continuous learning.
                        if self.evidence_feedback.is_none() {
                            self.ensure_evidence(ui.ctx());
                        }
                        let evidence = self
                            .evidence_feedback
                            .is_none()
                            .then(|| {
                                self.evidence_tex.as_ref().map(|tex| {
                                    let size = tex.size_vec2();
                                    let w = ui.available_width().min(200.0);
                                    let scale = w / size.x.max(1.0);
                                    (tex.id(), size * scale)
                                })
                            })
                            .flatten();
                        let mut pick: Option<bool> = None;
                        if let Some((id, draw)) = evidence {
                            ui.add_space(8.0);
                            ui.label(RichText::new("Last alert").size(12.0).color(MUTED));
                            ui.add_space(4.0);
                            ui.image((id, draw));

                            ui.add_space(10.0);
                            ui.label(
                                RichText::new("Was it empty or full?")
                                    .size(12.0)
                                    .color(MUTED),
                            );
                            ui.add_space(6.0);
                            // Full-width stacked pills — same pattern as Take reference
                            // so touch taps register (side-by-side + scroll stole clicks).
                            if feedback_pill(ui, "Empty", BTN).clicked() {
                                eprintln!("ui: Home feedback Empty clicked");
                                pick = Some(true);
                            }
                            ui.add_space(8.0);
                            if feedback_pill(ui, "Full", BTN).clicked() {
                                eprintln!("ui: Home feedback Full clicked");
                                pick = Some(false);
                            }
                        }
                        if let Some(empty) = pick {
                            self.label_last_alert(empty);
                        }

                        ui.add_space(14.0);
                        if pill_accent(ui, "Take reference").clicked() {
                            self.add_reference_photo();
                        }

                        ui.add_space(14.0);
                        live_host_stats(ui, self.host.snapshot());
                        row_stat(
                            ui,
                            "Last radio call",
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
                            } else if self.stats.last_radio_ok {
                                "OK"
                            } else {
                                "—"
                            },
                        );

                        ui.add_space(12.0);
                        note_line(ui, &self.note);
                            });
                    }
                    Page::Stats => {
                        scroll_y()
                            .auto_shrink([false, false])
                            .show(ui, |ui| self.stats_sidebar(ui));
                    }
                    Page::Config => {}
                    }
                });
        }

        if self.page != Page::Config {
            self.draw_terminal_panel(ctx);
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                match self.page {
                    Page::Live => self.draw_live_view(ui),
                    Page::Stats => self.stats_main(ui),
                    Page::Config => self.config_main(ui),
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

    ctx.style_mut(|s| {
        s.spacing.scroll.floating = true;
        s.spacing.scroll.bar_width = 0.0;
        s.spacing.scroll.floating_width = 0.0;
        s.spacing.scroll.floating_allocated_width = 0.0;
        s.spacing.scroll.bar_inner_margin = 0.0;
        s.spacing.scroll.bar_outer_margin = 0.0;
        s.spacing.scroll.dormant_background_opacity = 0.0;
        s.spacing.scroll.active_background_opacity = 0.0;
        s.spacing.scroll.interact_background_opacity = 0.0;
        s.spacing.scroll.dormant_handle_opacity = 0.0;
        s.spacing.scroll.active_handle_opacity = 0.0;
        s.spacing.scroll.interact_handle_opacity = 0.0;
    });
}

fn scroll_y() -> egui::ScrollArea {
    egui::ScrollArea::vertical()
        // Touch on the Pi otherwise turns a tap into a scroll drag and
        // Empty/Full never see clicked().
        .drag_to_scroll(false)
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
}

/// Top-bar play / pause. Green = start monitoring, red = stop.
fn monitor_play_btn(ui: &mut egui::Ui, running: bool) -> egui::Response {
    let size = 40.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let fill = if running { RED } else { GREEN };
    let hover = if running {
        Color32::from_rgb(255, 99, 88)
    } else {
        Color32::from_rgb(72, 225, 112)
    };
    ui.painter().circle_filled(
        rect.center(),
        size * 0.5,
        if resp.hovered() { hover } else { fill },
    );
    let c = rect.center();
    if running {
        // Pause: two vertical bars
        let bar_w = 4.0;
        let bar_h = 14.0;
        let gap = 5.0;
        let y0 = c.y - bar_h * 0.5;
        ui.painter().rect_filled(
            egui::Rect::from_min_size(
                Pos2::new(c.x - gap * 0.5 - bar_w, y0),
                Vec2::new(bar_w, bar_h),
            ),
            CornerRadius::same(1),
            Color32::WHITE,
        );
        ui.painter().rect_filled(
            egui::Rect::from_min_size(
                Pos2::new(c.x + gap * 0.5, y0),
                Vec2::new(bar_w, bar_h),
            ),
            CornerRadius::same(1),
            Color32::WHITE,
        );
    } else {
        // Play: triangle pointing right
        let left = c.x - 4.0;
        let right = c.x + 7.0;
        let top = c.y - 8.0;
        let bot = c.y + 8.0;
        ui.painter().add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(left, top),
                Pos2::new(right, c.y),
                Pos2::new(left, bot),
            ],
            Color32::WHITE,
            Stroke::NONE,
        ));
    }
    resp.on_hover_text(if running {
        "Stop monitoring"
    } else {
        "Start monitoring"
    })
}

fn nav_bar(ui: &mut egui::Ui, page: &mut Page) {
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    let items = [
        (Page::Live, "Home"),
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

#[derive(Clone, Copy)]
enum LiveIcon {
    Crop,
    Cancel,
    Clear,
    Filter,
}

fn icon_btn(
    ui: &mut egui::Ui,
    side: f32,
    on: bool,
    icon: LiveIcon,
    tip: &str,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(side), Sense::click());
    let fill = if on {
        BLUE
    } else if resp.hovered() {
        Color32::from_rgb(58, 58, 60)
    } else {
        BTN
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(10), fill);
    let ink = if on { Color32::WHITE } else { TEXT };
    paint_live_icon(ui.painter(), rect.center(), icon, ink);
    resp.on_hover_text(tip)
}

fn paint_live_icon(painter: &egui::Painter, c: Pos2, icon: LiveIcon, color: Color32) {
    let s = Stroke::new(1.8_f32, color);
    match icon {
        LiveIcon::Crop => {
            // Selection box — the watched region.
            let r = egui::Rect::from_center_size(c, Vec2::splat(16.0));
            painter.rect_stroke(r, CornerRadius::same(3), s, egui::StrokeKind::Inside);
            painter.line_segment(
                [Pos2::new(r.left() + 4.0, r.top()), Pos2::new(r.left() + 4.0, r.top() + 4.0)],
                s,
            );
            painter.line_segment(
                [Pos2::new(r.left(), r.top() + 4.0), Pos2::new(r.left() + 4.0, r.top() + 4.0)],
                s,
            );
            painter.line_segment(
                [Pos2::new(r.right() - 4.0, r.bottom()), Pos2::new(r.right() - 4.0, r.bottom() - 4.0)],
                s,
            );
            painter.line_segment(
                [Pos2::new(r.right(), r.bottom() - 4.0), Pos2::new(r.right() - 4.0, r.bottom() - 4.0)],
                s,
            );
        }
        LiveIcon::Cancel | LiveIcon::Clear => {
            let d = 6.0;
            painter.line_segment(
                [Pos2::new(c.x - d, c.y - d), Pos2::new(c.x + d, c.y + d)],
                s,
            );
            painter.line_segment(
                [Pos2::new(c.x + d, c.y - d), Pos2::new(c.x - d, c.y + d)],
                s,
            );
        }
        LiveIcon::Filter => {
            // Three sliders — the processed filter view.
            for (i, knob) in [0.28_f32, 0.68, 0.45].iter().enumerate() {
                let y = c.y - 7.0 + i as f32 * 7.0;
                let x0 = c.x - 8.0;
                let x1 = c.x + 8.0;
                painter.line_segment([Pos2::new(x0, y), Pos2::new(x1, y)], s);
                painter.circle_filled(Pos2::new(x0 + knob * 16.0, y), 2.6, color);
            }
        }
    }
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

/// Home continuous-learning tap target (full width, tall enough for touch).
fn feedback_pill(ui: &mut egui::Ui, text: &str, fill: Color32) -> egui::Response {
    centered_btn(
        ui,
        text,
        16.0,
        Color32::WHITE,
        fill,
        10,
        Vec2::new(ui.available_width(), 48.0),
    )
}

fn config_action_btn(ui: &mut egui::Ui, text: &str, accent: bool) -> egui::Response {
    let w = (text.len() as f32 * 7.5 + 28.0).clamp(72.0, 180.0);
    centered_btn(
        ui,
        text,
        13.0,
        if accent { Color32::WHITE } else { TEXT },
        if accent { BLUE } else { BTN },
        8,
        Vec2::new(w, 32.0),
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

/// Supabase holds the reference. Memory keeps whichever copy is newer.
/// A local file is only the cache used when the network is down.
fn sync_reference(cloud: Option<&Supabase>, local: Option<Reference>) -> Option<Reference> {
    let Some(cloud) = cloud else {
        return local;
    };
    let remote = cloud
        .download_object(&cloud.reference_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Reference>(&bytes).ok())
        .filter(|r| !r.frames.is_empty());
    match (local, remote) {
        (Some(local), Some(remote)) if remote.built_at_unix > local.built_at_unix => {
            let _ = remote.save();
            eprintln!("supabase: reference pulled ({})", remote.built_at_unix);
            Some(remote)
        }
        (Some(local), remote) => {
            let remote_ts = remote.as_ref().map(|r| r.built_at_unix).unwrap_or(0);
            if local.built_at_unix > remote_ts {
                if let Ok(bytes) = serde_json::to_vec(&local) {
                    cloud.upload_bytes(&cloud.reference_path(), bytes, "application/json");
                    eprintln!("supabase: reference pushed ({})", local.built_at_unix);
                }
            }
            Some(local)
        }
        (None, Some(remote)) => {
            let _ = remote.save();
            eprintln!("supabase: reference pulled ({})", remote.built_at_unix);
            Some(remote)
        }
        (None, None) => None,
    }
}

fn line_lamp(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Line").size(13.0).color(MUTED));
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
        let color = match silo_alert::line_lamp() {
            silo_alert::LineLamp::Tx => RED,
            silo_alert::LineLamp::Busy => BLUE,
            silo_alert::LineLamp::Off => Color32::TRANSPARENT,
        };
        if color != Color32::TRANSPARENT {
            ui.painter().circle_filled(rect.center(), 5.5, color);
        }
    })
    .response
    .on_hover_text("Blue = channel busy (SPKEN). Red = this Pi is transmitting. Dark = clear.");
}

fn row_stat(ui: &mut egui::Ui, k: &str, v: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        ui.label(RichText::new(k).size(13.0).color(MUTED));
        ui.label(RichText::new(v).size(13.0).color(TEXT));
    });
    ui.add_space(6.0);
}

/// Field header with an optional `!` tip (hover for description).
fn field_label(ui: &mut egui::Ui, label: &str, tip: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(12.0).color(MUTED));
        if !tip.is_empty() {
            ui.add_space(2.0);
            ui.label(RichText::new("!").size(11.0).color(BLUE).strong())
                .on_hover_text(tip);
        }
    });
}

/// Fixed-size Config text field with a stable id so focus/typing stay reliable.
fn config_text_field(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut String,
    hint: &str,
    password: bool,
) -> egui::Response {
    let height = ui.spacing().interact_size.y.max(28.0);
    let resp = ui
        .allocate_ui_with_layout(
            Vec2::new(CONFIG_FIELD_W, height),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add(
                    egui::TextEdit::singleline(value)
                        .id_source(id)
                        .desired_width(CONFIG_FIELD_W)
                        .frame(true)
                        .password(password)
                        .hint_text(hint)
                        .clip_text(true),
                )
            },
        )
        .inner;
    if resp.clicked() && !resp.has_focus() {
        resp.request_focus();
    }
    resp
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

fn labeled_slider(
    ui: &mut egui::Ui,
    label: &str,
    tip: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    unit: &str,
) {
    field_label(ui, label, tip);
    ui.allocate_ui_with_layout(
        Vec2::new(CONFIG_FIELD_W, ui.spacing().interact_size.y),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.add(egui::Slider::new(value, range).suffix(format!(" {unit}")));
        },
    );
    ui.add_space(6.0);
}

fn live_host_stats(ui: &mut egui::Ui, snap: Option<&HostSnapshot>) {
    let Some(s) = snap else {
        row_stat(ui, "Temp", "—");
        row_stat(ui, "CPU", "—");
        row_stat(ui, "RAM", "—");
        row_stat(ui, "Power", "—");
        return;
    };
    row_stat(
        ui,
        "Temp",
        &s.temp_c
            .map(|t| format!("{t:.0}°C"))
            .unwrap_or_else(|| "—".into()),
    );
    row_stat(
        ui,
        "CPU",
        &s.cpu
            .map(|c| format!("{:.0}%", c * 100.0))
            .unwrap_or_else(|| "—".into()),
    );
    row_stat(
        ui,
        "RAM",
        &s.ram_used_gb()
            .map(|(u, t)| format!("{u:.1} / {t:.1} GB"))
            .unwrap_or_else(|| "—".into()),
    );
    row_stat(
        ui,
        "Power",
        &s.throttled
            .map(host::live_power_label)
            .unwrap_or_else(|| "—".into()),
    );
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

fn config_tab_btn(ui: &mut egui::Ui, text: &str, on: bool) -> egui::Response {
    let w = ui.available_width();
    centered_btn(
        ui,
        text,
        13.0,
        if on { Color32::WHITE } else { MUTED },
        if on { BLUE } else { BTN },
        8,
        Vec2::new(w, 36.0),
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

fn truncate_chars(s: &str, max: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn draw_preview_pane(
    ui: &mut egui::Ui,
    tex: Option<&egui::TextureHandle>,
    title: &str,
    w: f32,
    h: f32,
) {
    ui.vertical(|ui| {
        ui.label(RichText::new(title).size(12.0).color(MUTED));
        ui.add_space(4.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), VIDEO_BG);
        if let Some(tex) = tex {
            let size = tex.size_vec2();
            let scale = (rect.width() / size.x)
                .min(rect.height() / size.y)
                .max(0.01);
            let draw = size * scale;
            let img_rect = egui::Rect::from_center_size(rect.center(), draw);
            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            ui.painter().image(tex.id(), img_rect, uv, Color32::WHITE);
        } else {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "No preview",
                egui::FontId::proportional(13.0),
                MUTED,
            );
        }
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(10),
            Stroke::new(1.0_f32, HAIRLINE),
            egui::StrokeKind::Inside,
        );
    });
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

/// Tell systemd the plant loop is still running. No-op outside the service.
fn pet_watchdog() {
    use std::sync::Mutex;
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let Ok(mut last) = LAST.lock() else {
        return;
    };
    let due = last
        .map(|t| t.elapsed() >= Duration::from_secs(10))
        .unwrap_or(true);
    if !due {
        return;
    }
    *last = Some(Instant::now());
    sd_notify("WATCHDOG=1");
}

fn sd_notify(state: &str) {
    let Ok(addr) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    if addr.is_empty() {
        return;
    }
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let Ok(sock) = UnixDatagram::unbound() else {
        return;
    };
    let dest = if let Some(name) = addr.strip_prefix('@') {
        let Ok(sa) = SocketAddr::from_abstract_name(name.as_bytes()) else {
            return;
        };
        sa
    } else {
        let Ok(sa) = SocketAddr::from_pathname(&addr) else {
            return;
        };
        sa
    };
    let _ = sock.send_to_addr(state.as_bytes(), &dest);
}

fn main() -> eframe::Result {
    load_dotenv();
    // First ping before camera and cloud setup, so a slow start still
    // fits inside WatchdogSec.
    sd_notify("WATCHDOG=1");
    // Use X11/Xwayland. Native Wayland can freeze the pointer under labwc + Pi Connect.
    if std::env::var_os("WINIT_UNIX_BACKEND").is_none() {
        unsafe { std::env::set_var("WINIT_UNIX_BACKEND", "x11") };
    }

    // Unkey PTT before anything else — power-up must not leave the radio keyed.
    {
        let cfg = Config::load();
        silo_alert::set_ptt_pin(cfg.radio.ptt_gpio);
        silo_alert::set_spken_pin(cfg.radio.spken_gpio);
        silo_alert::set_channel_pins(
            cfg.radio.ch_bit1_gpio,
            cfg.radio.ch_bit2_gpio,
            cfg.radio.ch_bit4_gpio,
            cfg.radio.ch_bit8_gpio,
        );
        let _ = silo_alert::select_channel(cfg.radio.channel);
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

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 720.0])
            .with_title("Industrial Smart System"),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    loop {
        sd_notify("WATCHDOG=1");
        if display_ready() {
            let started = Instant::now();
            let cam = match camera::Cam::open(&Config::load().camera) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("camera open failed (will keep retrying): {e}");
                    None
                }
            };
            let backend = std::env::var("WINIT_UNIX_BACKEND").unwrap_or_else(|_| "?".into());
            eprintln!("opening Industrial Smart System (backend={backend})");
            match eframe::run_native(
                "Industrial Smart System",
                opts.clone(),
                Box::new(move |_cc| Ok(Box::new(App::new(cam)))),
            ) {
                Ok(()) => eprintln!("gui closed — plant keeps running"),
                Err(e) => {
                    eprintln!("gui failed — plant keeps running: {e}");
                    // Wayland can fail on some sessions; fall back to X11/Xwayland.
                    if std::env::var_os("WINIT_UNIX_BACKEND").as_deref()
                        == Some(std::ffi::OsStr::new("wayland"))
                        && x11_ready()
                    {
                        eprintln!("gui: falling back to X11 backend");
                        unsafe { std::env::set_var("WINIT_UNIX_BACKEND", "x11") };
                    }
                }
            }
            if started.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_secs(3));
            }
        }
        run_headless_until_display();
    }
}

/// True when a Wayland or X11 display socket is available for the operator UI.
fn display_ready() -> bool {
    wayland_ready() || x11_ready()
}

fn wayland_ready() -> bool {
    let Some(name) = std::env::var_os("WAYLAND_DISPLAY") else {
        // systemd may not pass WAYLAND_DISPLAY; labwc default socket.
        return std::path::Path::new("/run/user/1000/wayland-0").exists();
    };
    if name.is_empty() {
        return false;
    }
    let name = name.to_string_lossy();
    if std::path::Path::new(name.as_ref()).is_absolute() {
        return std::path::Path::new(name.as_ref()).exists();
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
    std::path::Path::new(&runtime).join(name.as_ref()).exists()
}

fn x11_ready() -> bool {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    let n = display
        .trim_start_matches(':')
        .chars()
        .next()
        .filter(|c| c.is_ascii_digit())
        .unwrap_or('0');
    std::path::Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
}

/// Camera, comparison and radio with no window. Returns when the desktop is back.
fn run_headless_until_display() {
    eprintln!("plant: desktop is down — monitoring continues");
    let mut app = App::new(None);
    loop {
        if display_ready() {
            eprintln!("plant: desktop is back");
            return;
        }
        app.tick_plant();
        std::thread::sleep(Duration::from_millis(200));
    }
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
