mod files;
mod gemini;
mod system;

use eframe::egui::{self, Key, Modifiers, ViewportCommand};
use std::any::Any;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use system::Config;

const KEY_URL: &str = "https://aistudio.google.com/apikey";

/// Everything background threads (workers, tray, shortcut, second launch) tell the UI.
enum Ev {
    Show,
    Translated(Result<String, String>),
    Progress(String),
    FileDone(Result<Vec<PathBuf>, String>),
    KeyChecked(String, Result<(), String>),
}

#[derive(PartialEq)]
enum Tab {
    Text,
    Files,
    Settings,
}

struct App {
    cfg: Config,
    tx: mpsc::Sender<Ev>,
    rx: mpsc::Receiver<Ev>,
    tab: Tab,
    input: String,
    output: String,
    text_busy: bool,
    text_msg: String,
    file: Option<PathBuf>,
    file_busy: bool,
    file_msg: String,
    key_input: String,
    key_busy: bool,
    key_msg: String,
    hide_first: bool, // macOS autostart: start in the tray
    _keep: Box<dyn Any>, // tray/hotkey handles
}

fn main() -> eframe::Result {
    let resident = std::env::args().any(|a| a == "--resident");

    #[cfg(target_os = "linux")]
    if resident {
        system::run_tray_daemon();
        return Ok(());
    }
    // already open? it was told to come to the front
    let Some(instance) = system::claim("window") else { return Ok(()) };

    let (tx, rx) = mpsc::channel();
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Transly")
            .with_app_id("transly")
            .with_inner_size([900.0, 560.0])
            .with_min_inner_size([560.0, 360.0])
            .with_icon(egui::IconData { rgba: system::icon_rgba(64), width: 64, height: 64 }),
        ..Default::default()
    };
    eframe::run_native("Transly", opts, Box::new(move |cc| Ok(Box::new(App::new(cc, tx, rx, resident, instance)))))
}

fn lang_pair(code: &str) -> (&'static str, &'static str) {
    *gemini::LANGS.iter().find(|l| l.1 == code).unwrap_or(&gemini::LANGS[0])
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (i, path) in system::cjk_fonts().enumerate() {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let name = format!("system-{i}");
        fonts.font_data.insert(name.clone(), Arc::new(egui::FontData::from_owned(bytes)));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.get_mut(&fam).unwrap().push(name.clone());
        }
    }
    ctx.set_fonts(fonts);
}

fn lang_combo(ui: &mut egui::Ui, id: &str, code: &mut String, with_auto: bool) {
    let shown = if code == "auto" { "Detect language" } else { lang_pair(code).0 };
    egui::ComboBox::from_id_salt(id).selected_text(shown).show_ui(ui, |ui| {
        if with_auto && ui.selectable_label(code == "auto", "Detect language").clicked() {
            *code = "auto".into();
        }
        for (name, c) in gemini::LANGS {
            if ui.selectable_label(code == c, *name).clicked() {
                *code = c.to_string();
            }
        }
    });
}

/// `[Source v] <-> [Target v]`. Returns (anything changed, swap button pressed).
fn lang_bar(ui: &mut egui::Ui, cfg: &mut Config) -> (bool, bool) {
    let before = (cfg.source.clone(), cfg.target.clone());
    lang_combo(ui, "src", &mut cfg.source, true);
    let swapped = ui.button("\u{21C4}").on_hover_text("Swap languages").clicked();
    if swapped {
        cfg.swap();
    }
    lang_combo(ui, "dst", &mut cfg.target, false);
    ((cfg.source.clone(), cfg.target.clone()) != before, swapped)
}

/// "Higher quality" checkbox. Returns true when toggled.
fn quality_box(ui: &mut egui::Ui, cfg: &mut Config) -> bool {
    let changed = ui
        .checkbox(&mut cfg.hq, "Higher quality")
        .on_hover_text("Slower. Uses a stronger model (about 20 requests a day), then falls back to the fast one.")
        .changed();
    if cfg.hq && gemini::strong_paused() {
        ui.weak("(quota used or busy: using the fast model)");
    }
    changed
}

fn direction(cfg: &Config) -> gemini::Dir<'static> {
    gemini::Dir {
        from: (cfg.source != "auto").then(|| lang_pair(&cfg.source).0),
        to: lang_pair(&cfg.target).0,
        strong: cfg.hq,
    }
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        tx: mpsc::Sender<Ev>,
        rx: mpsc::Receiver<Ev>,
        resident: bool,
        instance: system::Instance,
    ) -> Self {
        install_fonts(&cc.egui_ctx);

        let show: Arc<dyn Fn() + Send + Sync> = {
            let (tx, ctx) = (tx.clone(), cc.egui_ctx.clone());
            Arc::new(move || {
                let _ = tx.send(Ev::Show);
                ctx.request_repaint();
            })
        };
        let s = show.clone();
        instance.serve(move || s());
        #[cfg(target_os = "macos")]
        let keep = system::mac_tray_and_hotkey(show);
        #[cfg(not(target_os = "macos"))]
        let keep: Box<dyn Any> = Box::new(());

        Self {
            cfg: Config::load(),
            tx,
            rx,
            tab: Tab::Text,
            input: String::new(),
            output: String::new(),
            text_busy: false,
            text_msg: String::new(),
            file: None,
            file_busy: false,
            file_msg: String::new(),
            key_input: String::new(),
            key_busy: false,
            key_msg: String::new(),
            hide_first: cfg!(target_os = "macos") && resident,
            _keep: keep,
        }
    }

    /// Runs `job` on a thread; whatever it sends wakes the UI.
    fn spawn(&self, ctx: &egui::Context, job: impl FnOnce(&dyn Fn(Ev)) + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            job(&|ev| {
                let _ = tx.send(ev);
                ctx.request_repaint();
            })
        });
    }

    fn save_cfg(&mut self) -> Result<(), String> {
        self.cfg.save().map_err(|e| format!("Can't save settings: {e}"))
    }

    fn translate_text(&mut self, ctx: &egui::Context) {
        if self.text_busy || self.input.trim().is_empty() {
            return;
        }
        self.text_busy = true;
        self.text_msg.clear();
        let (key, text, dir) = (self.cfg.api_key.clone(), self.input.clone(), direction(&self.cfg));
        self.spawn(ctx, move |send| {
            send(Ev::Translated(files::translate_text(&text, &mut |c, prev| gemini::translate(&key, c, dir, prev), &|_| {})))
        });
    }

    fn translate_file(&mut self, ctx: &egui::Context) {
        let Some(path) = self.file.clone() else { return };
        if self.file_busy {
            return;
        }
        self.file_busy = true;
        self.file_msg = "Working…".into();
        let (key, dir, code) = (self.cfg.api_key.clone(), direction(&self.cfg), lang_pair(&self.cfg.target).1);
        self.spawn(ctx, move |send| {
            let r = files::run(&key, &path, dir, code, &|m| send(Ev::Progress(m)));
            send(Ev::FileDone(r));
        });
    }

    fn check_key(&mut self, ctx: &egui::Context) {
        let key = self.key_input.trim().to_string();
        if key.is_empty() || self.key_busy {
            return;
        }
        self.key_busy = true;
        self.key_msg = "Checking the key…".into();
        self.spawn(ctx, move |send| {
            let r = gemini::translate(&key, "Hello", gemini::Dir { from: None, to: "German", strong: false }, None).map(|_| ());
            send(Ev::KeyChecked(key, r));
        });
    }

    fn key_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, center: bool) {
        ui.horizontal(|ui| {
            if center {
                ui.add_space(((ui.available_width() - 470.0) / 2.0).max(0.0));
            }
            let edit = egui::TextEdit::singleline(&mut self.key_input)
                .password(true)
                .hint_text("Paste your API key")
                .desired_width(360.0);
            let r = ui.add(edit);
            let can = !self.key_busy && !self.key_input.trim().is_empty();
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if ui.add_enabled(can, egui::Button::new("Test & save")).clicked() || (enter && can) {
                self.check_key(ctx);
            }
        });
        if !self.key_msg.is_empty() {
            ui.label(&self.key_msg);
        }
    }

    fn key_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.heading("Welcome to Transly");
                ui.add_space(10.0);
                ui.label("Transly translates text and files with Google Gemini. It needs a free API key:");
                ui.hyperlink_to(KEY_URL, KEY_URL);
                ui.label("Sign in with a Google account, click “Create API key”, then paste it below.");
                ui.add_space(14.0);
                self.key_row(ui, ctx, true);
            });
        });
    }

    fn text_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal_wrapped(|ui| {
            let (changed, swapped) = lang_bar(ui, &mut self.cfg);
            let hq_changed = quality_box(ui, &mut self.cfg);
            if changed || hq_changed {
                let _ = self.save_cfg();
            }
            if swapped && !self.output.is_empty() {
                self.input = std::mem::take(&mut self.output); // the translation becomes the text to translate back
            }
            let can = !self.text_busy && !self.input.trim().is_empty();
            let label = if self.text_busy { "Translating…" } else { "Translate" };
            let hotkey = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));
            if (ui.add_enabled(can, egui::Button::new(label)).clicked() || hotkey) && can {
                self.translate_text(ctx);
            }
            if ui.add_enabled(!self.output.is_empty(), egui::Button::new("Copy")).clicked() {
                ctx.copy_text(self.output.clone());
            }
            if !self.text_msg.is_empty() {
                ui.colored_label(ui.visuals().error_fg_color, &self.text_msg);
            }
        });
        ui.separator();

        let h = ui.available_height();
        let rows = ((h / ui.text_style_height(&egui::TextStyle::Body)) as usize).max(4) - 1;
        ui.columns(2, |cols| {
            egui::ScrollArea::vertical().id_salt("in").max_height(h).show(&mut cols[0], |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.input)
                        .hint_text("Type or paste text. The language is detected automatically.")
                        .desired_width(f32::INFINITY)
                        .desired_rows(rows),
                );
            });
            egui::ScrollArea::vertical().id_salt("out").max_height(h).show(&mut cols[1], |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.output.as_str())
                        .hint_text("Translation")
                        .desired_width(f32::INFINITY)
                        .desired_rows(rows),
                );
            });
        });
    }

    fn files_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if let Some(p) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf())) {
            self.file = Some(p);
            self.file_msg.clear();
        }
        ui.add_space(8.0);
        ui.label("Text files (.txt, .md, .srt, …), Word (.docx) and PDF (.pdf, saved as Markdown).");
        ui.label("The translation is saved next to the original, e.g. report.de.docx.");
        ui.add_space(8.0);
        if ui.button("Choose file…").clicked()
            && let Some(p) = rfd::FileDialog::new().pick_file()
        {
            self.file = Some(p);
            self.file_msg.clear();
        }
        ui.label("…or drop a file onto this window.");
        if let Some(p) = &self.file {
            ui.add_space(8.0);
            ui.label(format!("Selected: {}", p.display()));
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            let (changed, _) = lang_bar(ui, &mut self.cfg);
            if changed | quality_box(ui, &mut self.cfg) {
                let _ = self.save_cfg();
            }
            let can = self.file.is_some() && !self.file_busy;
            if ui.add_enabled(can, egui::Button::new("Translate file")).clicked() {
                self.translate_file(ctx);
            }
            if self.file_busy {
                ui.spinner();
            }
        });
        if !self.file_msg.is_empty() {
            ui.add_space(8.0);
            ui.label(&self.file_msg);
        }
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(8.0);
        ui.heading("Gemini API key");
        ui.label("A key is saved. To replace it, paste a new one:");
        self.key_row(ui, ctx, false);
        ui.hyperlink_to("Get a free key", KEY_URL);
        ui.separator();
        if ui.checkbox(&mut self.cfg.autostart, "Start Transly when I log in").changed() {
            let r = system::set_autostart(self.cfg.autostart).and_then(|_| self.save_cfg());
            self.key_msg = r.err().unwrap_or_default();
        }
        ui.label(format!("Open from anywhere: {} (tray icon: Open Transly)", system::SHORTCUT_HINT));
        ui.separator();
        ui.weak("Free-tier Gemini requests may be used by Google to improve its products. Don't translate confidential text.");
    }

    fn main_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Text, "Text");
                ui.selectable_value(&mut self.tab, Tab::Files, "Files");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
            });
        });
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Text => self.text_tab(ui, ctx),
            Tab::Files => self.files_tab(ui, ctx),
            Tab::Settings => self.settings_tab(ui, ctx),
        });
    }
}

impl eframe::App for App {
    /// Also runs while the window is hidden, so tray/shortcut events are handled here.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Show => {
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
                Ev::Translated(r) => {
                    self.text_busy = false;
                    match r {
                        Ok(t) => self.output = t,
                        Err(e) => self.text_msg = e,
                    }
                }
                Ev::Progress(m) => self.file_msg = m,
                Ev::FileDone(r) => {
                    self.file_busy = false;
                    self.file_msg = match r {
                        Ok(paths) => {
                            let list: Vec<_> = paths.iter().map(|p| p.display().to_string()).collect();
                            format!("Saved:\n{}", list.join("\n"))
                        }
                        Err(e) => e,
                    };
                }
                Ev::KeyChecked(key, r) => {
                    self.key_busy = false;
                    self.key_msg = match r {
                        Ok(()) => {
                            let first = self.cfg.api_key.is_empty();
                            self.cfg.api_key = key;
                            self.cfg.autostart |= first;
                            self.key_input.clear();
                            let setup = if first { system::first_run_setup() } else { Ok(()) };
                            match setup.and_then(|_| self.save_cfg()) {
                                Ok(()) => "Key saved.".into(),
                                Err(e) => e,
                            }
                        }
                        Err(e) => e,
                    };
                }
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if std::mem::take(&mut self.hide_first) {
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }
        // macOS keeps running in the tray; on Linux closing the window ends this process.
        #[cfg(target_os = "macos")]
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }

        if self.cfg.api_key.is_empty() {
            self.key_screen(ui, &ctx);
        } else {
            self.main_screen(ui, &ctx);
        }
    }
}
