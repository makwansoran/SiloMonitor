mod camera;
mod config;
mod eventlog;
mod sa828;
mod silo_alert;
mod stats;
mod vision;

use chrono::{Local, TimeZone};
use config::Config;
use eframe::egui::{self, Color32, CornerRadius, Pos2, RichText, Sense, Stroke, Vec2};
use eventlog::EventLog;
use silo_alert::SiloAlert;
use stats::Stats;
use vision::{Model, Prediction, SampleAnalysis, SampleMeta};

const BG: Color32 = Color32::from_rgb(0, 0, 0);
const CARD: Color32 = Color32::from_rgb(28, 28, 30);
const TEXT: Color32 = Color32::from_rgb(245, 245, 247);
const MUTED: Color32 = Color32::from_rgb(142, 142, 147);
const BLUE: Color32 = Color32::from_rgb(10, 132, 255);
const RED: Color32 = Color32::from_rgb(255, 69, 58);
const FILL: Color32 = Color32::from_rgb(22, 48, 84);
const BTN: Color32 = Color32::from_rgb(44, 44, 46);
const VIDEO_BG: Color32 = Color32::from_rgb(10, 10, 12);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Live,
    Train,
    Config,
}

struct App {
    cam: Option<camera::Cam>,
    live_tex: Option<egui::TextureHandle>,
    last_rgb: Option<(u32, u32, Vec<u8>)>,
    model: Option<Model>,
    last_pred: Option<Prediction>,
    alert: Option<SiloAlert>,
    stats: Stats,
    monitor: bool,
    page: Page,
    cfg: Config,
    events: EventLog,
    empty_alerts: Vec<u64>,
    empty_since: Option<u64>,
    empty: bool,
    note: String,
    samples: Vec<SampleMeta>,
    sample_idx: Option<usize>,
    sample_tex: Option<egui::TextureHandle>,
    sample_view: Option<SampleAnalysis>,
    dataset_dirty: bool,
    filter_empty: Option<bool>,
    logo_tex: Option<egui::TextureHandle>,
    radio_freq: String,
}

impl App {
    fn new(cam: Option<camera::Cam>) -> Self {
        vision::ensure_dirs();
        let cfg = Config::load();
        let mut stats = Stats::load();
        let (ne, nf) = vision::count_labels();
        stats.labels_empty = ne as u64;
        stats.labels_full = nf as u64;
        let model = Model::load(vision::MODEL_PATH);
        if model.is_none() {
            stats.last_train_unix = None;
            stats.last_train_accuracy = None;
        }
        silo_alert::set_ptt_pin(cfg.radio.ptt_gpio);
        let mut alert = SiloAlert::new();
        alert.restore_last_alert_unix(stats.last_alert_unix);
        let log_path = cfg.resolve_db_path().with_extension("jsonl");
        let empty_alerts = EventLog::load_empty_alerts(&log_path);
        let has_cam = cam.is_some();
        Self {
            cam,
            live_tex: None,
            last_rgb: None,
            model,
            last_pred: None,
            alert: Some(alert),
            stats,
            monitor: has_cam,
            page: Page::Live,
            events: EventLog::open(log_path, cfg.storage.level_log_interval_seconds),
            empty_alerts,
            empty_since: None,
            empty: false,
            note: String::new(),
            samples: vision::list_samples(),
            sample_idx: None,
            sample_tex: None,
            sample_view: None,
            dataset_dirty: false,
            filter_empty: None,
            logo_tex: None,
            radio_freq: cfg.radio.frequency_mhz.clone(),
            cfg,
        }
    }

    fn ensure_logo(&mut self, ctx: &egui::Context) {
        if self.logo_tex.is_some() {
            return;
        }
        const LOGO: &[u8] = include_bytes!("../assets/spectr-mark.jpg");
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

    fn pull_frame(&mut self, ctx: &egui::Context) {
        let Some(cam) = self.cam.as_mut() else {
            return;
        };
        let Some((w, h, rgb)) = cam.frame() else {
            return;
        };
        if self.monitor {
            self.run_model(w, h, &rgb);
        }
        let img = egui::ColorImage::from_rgb([w as usize, h as usize], &rgb);
        match &mut self.live_tex {
            Some(t) => t.set(img, Default::default()),
            None => self.live_tex = Some(ctx.load_texture("live", img, Default::default())),
        }
        self.last_rgb = Some((w, h, rgb));
    }

    fn run_model(&mut self, w: u32, h: u32, rgb: &[u8]) {
        let Some(model) = self.model.as_ref() else {
            self.last_pred = None;
            self.empty = false;
            self.empty_since = None;
            return;
        };
        let feat = vision::features_from_rgb(w, h, rgb);
        let pred = model.predict(&feat);
        self.last_pred = Some(pred);

        let unix = eventlog::now_unix();
        let need = self.cfg.level.empty_confirmation_seconds.max(0.0) as u64;

        if pred.empty {
            if self.empty_since.is_none() {
                self.empty_since = Some(unix);
            }
        } else {
            self.empty_since = None;
        }

        let confirmed = self
            .empty_since
            .map(|t| unix.saturating_sub(t) >= need)
            .unwrap_or(false);

        if confirmed && !self.empty {
            self.empty = true;
            self.events.empty_alert(unix);
            self.empty_alerts.push(unix);
            self.events.state(unix, "level_state", "OK", "EMPTY", "");
        } else if !pred.empty && self.empty {
            self.empty = false;
            self.events.state(unix, "level_state", "EMPTY", "OK", "");
        }

        if let Some(alert) = &mut self.alert {
            if alert.update(self.empty) {
                self.stats.alerts_sent += 1;
                self.stats.last_alert_unix = alert.last_alert_unix();
                self.stats.save();
            }
        }
    }

    fn save_label(&mut self, empty: bool) {
        let Some((w, h, rgb)) = self.last_rgb.clone() else {
            self.note = "No frame yet".into();
            return;
        };
        match vision::save_label(empty, w, h, &rgb) {
            Ok(_) => {
                if empty {
                    self.stats.labels_empty += 1;
                } else {
                    self.stats.labels_full += 1;
                }
                self.stats.save();
                self.note = if empty {
                    "Saved empty".into()
                } else {
                    "Saved full".into()
                };
                self.dataset_dirty = true;
            }
            Err(e) => self.note = format!("Save failed: {e}"),
        }
    }

    fn test_radio(&mut self) {
        let result = match self.alert.as_mut() {
            Some(a) => a.test_transmit(),
            None => silo_alert::play_voice(),
        };
        self.note = match result {
            Ok(()) => "Transmitting silo is empty".into(),
            Err(e) => e,
        };
    }

    fn program_sender(&mut self) {
        match sa828::program(
            &self.radio_freq,
            self.cfg.radio.squelch,
            &self.cfg.radio.uart_port,
        ) {
            Ok(msg) => {
                self.cfg.radio.frequency_mhz = self.radio_freq.clone();
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

    fn do_train(&mut self) {
        match vision::train() {
            Ok(m) => {
                self.stats.last_train_unix = Some(m.trained_at_unix);
                self.stats.last_train_accuracy = Some(m.train_accuracy);
                self.stats.labels_empty = m.n_empty as u64;
                self.stats.labels_full = m.n_full as u64;
                self.stats.save();
                self.note = format!("Trained  {:.0}%", m.train_accuracy * 100.0);
                self.model = Some(m);
                self.dataset_dirty = true;
            }
            Err(e) => self.note = e,
        }
    }

    fn save_config(&mut self) {
        self.cfg.radio.frequency_mhz = self.radio_freq.clone();
        match self.cfg.save() {
            Ok(()) => self.note = "Saved".into(),
            Err(e) => self.note = e,
        }
    }

    fn refresh_samples(&mut self) {
        let keep_name = self
            .sample_idx
            .and_then(|i| self.samples.get(i).map(|s| s.file_name.clone()));
        self.samples = vision::list_samples();
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
        let (ne, nf) = vision::count_labels();
        self.stats.labels_empty = ne as u64;
        self.stats.labels_full = nf as u64;
    }

    fn load_sample_preview(&mut self, idx: usize) {
        if idx >= self.samples.len() {
            return;
        }
        self.sample_idx = Some(idx);
        let meta = self.samples[idx].clone();
        match vision::analyze_sample(&meta.path, meta.label_empty, self.model.as_ref()) {
            Ok(view) => {
                self.sample_view = Some(view);
                self.note.clear();
            }
            Err(e) => {
                self.sample_view = None;
                self.note = e;
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

    fn delete_selected(&mut self) {
        let Some(idx) = self.sample_idx.filter(|i| *i < self.samples.len()) else {
            self.note = "Nothing selected".into();
            return;
        };
        let meta = self.samples[idx].clone();
        match vision::delete_sample(&meta.path) {
            Ok(()) => {
                self.sample_idx = if idx > 0 { Some(idx - 1) } else { None };
                self.sample_view = None;
                self.sample_tex = None;
                self.dataset_dirty = true;
                self.note = format!("Deleted {}", meta.file_name);
            }
            Err(e) => self.note = e,
        }
    }

    fn relabel_selected(&mut self, empty: bool) {
        let Some(idx) = self.sample_idx.filter(|i| *i < self.samples.len()) else {
            return;
        };
        let path = self.samples[idx].path.clone();
        match vision::relabel_sample(&path, empty) {
            Ok(_) => {
                self.dataset_dirty = true;
                self.note = if empty {
                    "Moved to empty".into()
                } else {
                    "Moved to full".into()
                };
            }
            Err(e) => self.note = e,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        style(ctx);
        self.ensure_logo(ctx);
        if self.dataset_dirty {
            self.refresh_samples();
        }
        self.pull_frame(ctx);
        if self.page == Page::Train && self.sample_idx.is_some() {
            self.sync_sample_tex(ctx);
        }
        ctx.request_repaint();

        let trained = self.model.is_some();
        let days = eventlog::bucket_counts(&self.empty_alerts, 7, 86400);

        egui::SidePanel::right("side")
            .exact_width(300.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(CARD)
                    .inner_margin(egui::Margin::symmetric(22, 22)),
            )
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    if let Some(logo) = &self.logo_tex {
                        ui.add(
                            egui::Image::new(logo)
                                .fit_to_exact_size(Vec2::splat(40.0))
                                .bg_fill(Color32::TRANSPARENT),
                        );
                    }
                });
                ui.add_space(16.0);
                ui.vertical_centered(|ui| {
                    nav(ui, &mut self.page);
                });
                ui.add_space(22.0);

                match self.page {
                    Page::Live => {
                        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                            ui.set_width(ui.available_width());
                            if !trained {
                                ui.label(
                                    RichText::new("Not trained")
                                        .size(32.0)
                                        .color(TEXT)
                                        .strong(),
                                );
                                ui.add_space(8.0);
                                ui.label(
                                    RichText::new("Open Train, label empty and full frames, then train the model.")
                                        .size(13.0)
                                        .color(MUTED),
                                );
                                ui.add_space(16.0);
                                if pill_accent(ui, "Go to Train").clicked() {
                                    self.page = Page::Train;
                                }
                            } else {
                                let (label, color) = if self.empty {
                                    ("Empty", RED)
                                } else {
                                    ("OK", TEXT)
                                };
                                ui.label(RichText::new(label).size(36.0).color(color).strong());
                                ui.add_space(4.0);
                                let conf = self
                                    .last_pred
                                    .map(|p| format!("{:.0}% sure", p.confidence() * 100.0))
                                    .unwrap_or_else(|| "—".into());
                                ui.label(RichText::new(conf).size(16.0).color(MUTED));
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(format!("{} alerts", self.stats.alerts_sent))
                                        .size(13.0)
                                        .color(MUTED),
                                );
                                ui.add_space(24.0);
                                ui.label(RichText::new("Empty alerts").size(12.0).color(MUTED));
                                ui.add_space(8.0);
                                line_chart(ui, &days);
                            }
                            ui.add_space(20.0);
                            if pill_accent(ui, "Test radio").clicked() {
                                self.test_radio();
                            }
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(
                                    "Plays silo is empty. GPIO17 keys PTT.",
                                )
                                .size(12.0)
                                .color(MUTED),
                            );
                        });
                        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                            note_line(ui, &self.note);
                            ui.add_space(8.0);
                            let pause = if self.monitor { "Pause" } else { "Resume" };
                            if pill(ui, pause).clicked() {
                                self.monitor = !self.monitor;
                            }
                        });
                    }
                    Page::Train => {
                        ui.label(RichText::new("Training").size(22.0).color(TEXT).strong());
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(if trained {
                                "Label new frames, or open one below."
                            } else {
                                "Not trained yet. Label this camera frame."
                            })
                            .size(13.0)
                            .color(MUTED),
                        );
                        ui.add_space(16.0);
                        row_stat(ui, "Empty", &self.stats.labels_empty.to_string());
                        row_stat(ui, "Full", &self.stats.labels_full.to_string());
                        let acc = self
                            .stats
                            .last_train_accuracy
                            .map(|a| format!("{:.0}%", a * 100.0))
                            .unwrap_or_else(|| "Not trained".into());
                        row_stat(ui, "Accuracy", &acc);

                        if self.sample_idx.is_none() {
                            ui.add_space(12.0);
                            if pill(ui, "This is empty").clicked() {
                                self.save_label(true);
                            }
                            ui.add_space(8.0);
                            if pill(ui, "This is full").clicked() {
                                self.save_label(false);
                            }
                            ui.add_space(8.0);
                            if pill_accent(ui, "Train model").clicked() {
                                self.do_train();
                            }
                        } else {
                            let i = self.sample_idx.unwrap();
                            let meta = self.samples.get(i).cloned();
                            let view = self.sample_view.clone();
                            if let (Some(meta), Some(view)) = (meta, view) {
                                ui.add_space(8.0);
                                row_stat(ui, "Label", if meta.label_empty { "Empty" } else { "Full" });
                                row_stat(ui, "When", &fmt_ms(meta.captured_ms));
                                row_stat(ui, "Size", &format!("{}×{}", view.width, view.height));
                                row_stat(ui, "File", &format_bytes(meta.bytes));
                                row_stat(
                                    ui,
                                    "Path",
                                    &format!(
                                        "data/{}/{}",
                                        if meta.label_empty { "empty" } else { "full" },
                                        meta.file_name
                                    ),
                                );
                                row_stat(ui, "Brightness", &format!("{:.0}%", view.mean_brightness * 100.0));
                                if let Some(p) = view.pred {
                                    row_stat(
                                        ui,
                                        "Model",
                                        &format!(
                                            "{}  {:.0}%",
                                            if p.empty { "Empty" } else { "Full" },
                                            p.confidence() * 100.0
                                        ),
                                    );
                                    row_stat(
                                        ui,
                                        "Match",
                                        match view.agrees {
                                            Some(true) => "Agrees",
                                            Some(false) => "Disagrees",
                                            None => "—",
                                        },
                                    );
                                } else {
                                    row_stat(ui, "Model", "Not trained");
                                }
                                ui.add_space(10.0);
                                ui.horizontal(|ui| {
                                    if quiet_btn(ui, "Empty").clicked() {
                                        self.relabel_selected(true);
                                    }
                                    if quiet_btn(ui, "Full").clicked() {
                                        self.relabel_selected(false);
                                    }
                                    if quiet_btn(ui, "Delete").clicked() {
                                        self.delete_selected();
                                    }
                                });
                                ui.add_space(8.0);
                                if pill(ui, "Back to camera").clicked() {
                                    self.sample_idx = None;
                                    self.sample_view = None;
                                }
                            }
                        }

                        ui.add_space(14.0);
                        ui.label(RichText::new("Labeled").size(12.0).color(MUTED));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if filter_chip(ui, "All", self.filter_empty.is_none()).clicked() {
                                self.filter_empty = None;
                            }
                            if filter_chip(ui, "Empty", self.filter_empty == Some(true)).clicked() {
                                self.filter_empty = Some(true);
                            }
                            if filter_chip(ui, "Full", self.filter_empty == Some(false)).clicked() {
                                self.filter_empty = Some(false);
                            }
                        });
                        ui.add_space(6.0);
                        let filter = self.filter_empty;
                        let selected = self.sample_idx;
                        let rows: Vec<(usize, String, bool)> = self
                            .samples
                            .iter()
                            .enumerate()
                            .filter(|(_, s)| filter.map(|e| s.label_empty == e).unwrap_or(true))
                            .map(|(i, s)| {
                                (
                                    i,
                                    format!(
                                        "{}  {}",
                                        if s.label_empty { "Empty" } else { "Full" },
                                        fmt_ms(s.captured_ms)
                                    ),
                                    selected == Some(i),
                                )
                            })
                            .collect();
                        egui::ScrollArea::vertical()
                            .max_height(180.0)
                            .show(ui, |ui| {
                                if rows.is_empty() {
                                    ui.label(
                                        RichText::new("No labeled images yet")
                                            .size(13.0)
                                            .color(MUTED),
                                    );
                                }
                                for (i, label, on) in rows {
                                    if centered_btn(
                                        ui,
                                        &label,
                                        13.0,
                                        if on { Color32::WHITE } else { TEXT },
                                        if on { BLUE } else { Color32::TRANSPARENT },
                                        6,
                                        Vec2::new(ui.available_width(), 28.0),
                                    )
                                    .clicked()
                                    {
                                        self.load_sample_preview(i);
                                    }
                                }
                            });
                        ui.add_space(8.0);
                        note_line(ui, &self.note);
                    }
                    Page::Config => {
                        ui.label(RichText::new("Configure").size(22.0).color(TEXT).strong());
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new("How long it must look empty before an alert.")
                                .size(13.0)
                                .color(MUTED),
                        );
                        ui.add_space(18.0);
                        labeled_slider(
                            ui,
                            "Confirm empty",
                            &mut self.cfg.level.empty_confirmation_seconds,
                            1.0..=60.0,
                            "s",
                        );
                        ui.add_space(14.0);
                        if pill_accent(ui, "Save").clicked() {
                            self.save_config();
                        }
                        ui.add_space(24.0);
                        ui.label(RichText::new("Radio").size(12.0).color(MUTED));
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(
                                "UART: SA828 TXD→Pi pin 10, RXD→Pi pin 8, GND→pin 6. PTT→pin 11 (GPIO17). Power from 5 V PSU only. Voice = jack + pot.",
                            )
                            .size(13.0)
                            .color(MUTED),
                        );
                        ui.add_space(8.0);
                        ui.label(RichText::new("Frequency (MHz)").size(12.0).color(MUTED));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.radio_freq)
                                .desired_width(ui.available_width())
                                .hint_text("446.0062"),
                        );
                        ui.add_space(8.0);
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
                        ui.add_space(8.0);
                        ui.label(RichText::new("UART port").size(12.0).color(MUTED));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.radio.uart_port)
                                .desired_width(ui.available_width())
                                .hint_text("/dev/serial0"),
                        );
                        ui.add_space(10.0);
                        if pill_accent(ui, "Program module").clicked() {
                            self.program_sender();
                        }
                        ui.add_space(8.0);
                        if pill(ui, "Read module").clicked() {
                            self.read_radio();
                        }
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(
                                "Test radio keys PTT (GPIO17) then plays silo is empty on the jack.",
                            )
                            .size(12.0)
                            .color(MUTED),
                        );
                        ui.add_space(14.0);
                        ui.label(
                            RichText::new(
                                "Test radio plays silo is empty on the jack so every analog headset on this channel can hear it.",
                            )
                            .size(13.0)
                            .color(MUTED),
                        );
                        ui.add_space(12.0);
                        if pill(ui, "Test radio").clicked() {
                            self.test_radio();
                        }
                        ui.add_space(10.0);
                        note_line(ui, &self.note);
                    }
                }
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                let reviewing = self.page == Page::Train && self.sample_idx.is_some();
                let tex = if reviewing {
                    self.sample_tex.as_ref()
                } else {
                    self.live_tex.as_ref()
                };
                let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(16), VIDEO_BG);
                if let Some(tex) = tex {
                    let size = tex.size_vec2();
                    let scale = (rect.width() / size.x).min(rect.height() / size.y);
                    let draw = size * scale;
                    let img_rect = egui::Rect::from_center_size(rect.center(), draw);
                    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    ui.painter().image(tex.id(), img_rect, uv, Color32::WHITE);
                }
            });
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
}

fn nav(ui: &mut egui::Ui, page: &mut Page) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for (p, label) in [
            (Page::Live, "Live"),
            (Page::Train, "Train"),
            (Page::Config, "Config"),
        ] {
            let on = *page == p;
            let fill = if on { BLUE } else { BTN };
            let color = if on { Color32::WHITE } else { MUTED };
            if ui
                .allocate_ui_with_layout(
                    Vec2::new(78.0, 32.0),
                    egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                    |ui| {
                        ui.add(
                            egui::Button::new(RichText::new(label).size(13.0).color(color))
                                .fill(fill)
                                .corner_radius(8)
                                .stroke(Stroke::NONE),
                        )
                    },
                )
                .inner
                .clicked()
            {
                *page = p;
            }
        }
    });
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

fn labeled_slider(ui: &mut egui::Ui, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>, unit: &str) {
    ui.label(RichText::new(label).size(12.0).color(MUTED));
    ui.add(egui::Slider::new(value, range).suffix(format!(" {unit}")));
    ui.add_space(6.0);
}

fn quiet_btn(ui: &mut egui::Ui, text: &str) -> egui::Response {
    centered_btn(ui, text, 13.0, TEXT, BTN, 8, Vec2::new(64.0, 30.0))
}

fn filter_chip(ui: &mut egui::Ui, text: &str, on: bool) -> egui::Response {
    centered_btn(
        ui,
        text,
        12.0,
        if on { Color32::WHITE } else { MUTED },
        if on { BLUE } else { BTN },
        8,
        Vec2::new(56.0, 26.0),
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

fn line_chart(ui: &mut egui::Ui, values: &[u32]) {
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
    let end = (now / 86400) * 86400;
    for i in 0..n {
        let t = end.saturating_sub((n as u64 - 1 - i as u64) * 86400);
        let letter = match Local.timestamp_opt(t as i64, 0) {
            chrono::LocalResult::Single(dt) => dt.format("%a").to_string(),
            _ => String::new(),
        };
        let letter = letter.chars().next().unwrap_or(' ');
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
    if std::env::var_os("WINIT_UNIX_BACKEND").is_none() {
        unsafe { std::env::set_var("WINIT_UNIX_BACKEND", "x11") };
    }

    let cam = match camera::Cam::open() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("camera open failed (continuing without camera): {e}");
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
