//! Everything OS-specific: config file, autostart, single instance, tray, global shortcut.
//!
//! Linux (esp. Wayland) can't hide a window, so there the tray lives in a small resident process
//! (`transly --resident`) and "Open" starts a window process that exits when closed.
//! macOS keeps a single process that hides/shows its window.

use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};

pub const SHORTCUT_HINT: &str = "Ctrl+Shift+Space";

// ---------- config ----------

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub api_key: String,
    pub target: String, // language code, see gemini::LANGS
    pub autostart: bool,
}

impl Config {
    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("transly/config.json"))
    }

    pub fn load() -> Self {
        let mut c: Config = Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if c.target.is_empty() {
            c.target = "en".into();
        }
        c
    }

    pub fn save(&self) -> Result<(), String> {
        let p = Self::path().ok_or("no config directory")?;
        std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
        // write-then-rename so a crash can't leave a truncated file; the key is private to the user
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap()).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
    }
}

// ---------- fonts ----------

/// egui and PDF output ship no CJK glyphs; borrow whichever of these the OS has.
pub fn cjk_fonts() -> impl Iterator<Item = &'static str> {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/AppleSDGothicNeo.ttc",
        "/System/Library/Fonts/\u{30d2}\u{30e9}\u{30ae}\u{30ce}\u{89d2}\u{30b4}\u{30b7}\u{30c3}\u{30af} W3.ttc",
    ];
    CANDIDATES.iter().copied().filter(|p| std::path::Path::new(p).exists())
}

// ---------- icon ----------

/// A blue disc with a white "T", drawn in code so there's no asset to ship.
pub fn icon_rgba(n: u32) -> Vec<u8> {
    let mut px = vec![0u8; (n * n * 4) as usize];
    let c = n as f32 / 2.0;
    for y in 0..n {
        for x in 0..n {
            let (px_x, px_y) = (x as f32 + 0.5, y as f32 + 0.5);
            if ((px_x - c).powi(2) + (px_y - c).powi(2)).sqrt() > c - 0.5 {
                continue;
            }
            let (fx, fy) = (px_x / n as f32, px_y / n as f32);
            let white = (0.25..0.75).contains(&fx) && (0.25..0.40).contains(&fy)
                || (0.43..0.57).contains(&fx) && (0.40..0.78).contains(&fy);
            let rgb = if white { [255, 255, 255] } else { [47, 111, 237] };
            let i = ((y * n + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    }
    px
}

// ---------- single instance (a unix socket per role; a connection means "show yourself") ----------

fn sock_path(name: &str) -> PathBuf {
    dirs::runtime_dir().unwrap_or_else(std::env::temp_dir).join(format!("transly-{name}.sock"))
}

/// Holds the socket for this role. Claim it at startup, before slow work, so a second launch
/// that arrives meanwhile is queued instead of starting a duplicate.
pub struct Instance(Option<UnixListener>);

/// `None`: another instance with this role is running and has been told to show itself.
pub fn claim(name: &str) -> Option<Instance> {
    let p = sock_path(name);
    if UnixStream::connect(&p).is_ok() {
        return None;
    }
    let _ = std::fs::remove_file(&p); // stale socket from a crashed run
    // if binding fails (odd runtime dir) we still run, just without single-instance
    Some(Instance(UnixListener::bind(&p).ok()))
}

impl Instance {
    /// Call `on_show` whenever another launch of this role is attempted.
    pub fn serve(self, on_show: impl Fn() + Send + 'static) {
        let Some(l) = self.0 else { return };
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                drop(s);
                on_show();
            }
        });
    }
}

#[cfg(target_os = "linux")]
fn spawn_self(arg: Option<&str>) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = Command::new(exe);
    cmd.args(arg).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait(); // reap
        });
    }
}

// ---------- autostart ----------

pub fn set_autostart(on: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.to_string_lossy();
    let (path, body) = if cfg!(target_os = "macos") {
        let exe = quick_xml::escape::escape(&*exe);
        (
            dirs::home_dir().map(|h| h.join("Library/LaunchAgents/dev.transly.plist")),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                 <plist version=\"1.0\"><dict>\n\
                 <key>Label</key><string>dev.transly</string>\n\
                 <key>ProgramArguments</key><array><string>{exe}</string><string>--resident</string></array>\n\
                 <key>RunAtLoad</key><true/>\n\
                 </dict></plist>\n"
            ),
        )
    } else {
        (
            dirs::config_dir().map(|c| c.join("autostart/transly.desktop")),
            format!(
                "[Desktop Entry]\nType=Application\nName=Transly\nComment=Translator\n\
                 Exec=\"{exe}\" --resident\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
            ),
        )
    };
    let path = path.ok_or("can't find the home directory")?;
    if on {
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(&path, body).map_err(|e| e.to_string())
    } else {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        }
    }
}

/// Runs once, right after the first API key is accepted.
pub fn first_run_setup() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        register_shortcut();
        spawn_self(Some("--resident")); // tray appears now instead of at next login
    }
    set_autostart(true)
}

// ---------- Linux: GNOME shortcut + tray daemon ----------

#[cfg(target_os = "linux")]
const KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";
#[cfg(target_os = "linux")]
const KEY_PATH: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/transly/";

/// `cur` is gsettings' printed list, e.g. `@as []` or `['/a/', '/b/']`.
#[cfg(any(target_os = "linux", test))]
fn add_to_list(cur: &str, item: &str) -> String {
    if cur.contains(item) {
        cur.to_string()
    } else if cur.contains("'/") {
        format!("{}, '{item}']", cur.trim_end().trim_end_matches(']'))
    } else {
        format!("['{item}']")
    }
}

/// GNOME custom shortcut that runs `transly` (which opens or focuses the window). Best effort.
#[cfg(target_os = "linux")]
fn register_shortcut() {
    let gs = |args: &[&str]| {
        Command::new("gsettings")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(cur) = gs(&["get", KEYS, "custom-keybindings"]) else { return }; // not GNOME
    let schema = format!("{KEYS}.custom-keybinding:{KEY_PATH}");
    gs(&["set", &schema, "name", "Transly"]);
    gs(&["set", &schema, "command", &exe.to_string_lossy()]);
    gs(&["set", &schema, "binding", "<Primary><Shift>space"]);
    gs(&["set", KEYS, "custom-keybindings", &add_to_list(&cur, KEY_PATH)]);
}

/// Blocks forever: tray icon with Open/Quit. Needs a StatusNotifier/AppIndicator host (Ubuntu has one).
#[cfg(target_os = "linux")]
pub fn run_tray_daemon() {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    let Some(instance) = claim("resident") else { return }; // another tray already runs
    gtk::init().expect("GTK init failed (no display?)");
    let open = MenuItem::new("Open Transly", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit]).unwrap();
    let (open_id, quit_id) = (open.id().clone(), quit.id().clone());

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Transly")
        .with_icon(Icon::from_rgba(icon_rgba(64), 64, 64).unwrap())
        .build()
        .expect("tray icon failed");
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == open_id {
            spawn_self(None);
        } else if e.id == quit_id {
            std::process::exit(0);
        }
    }));
    instance.serve(|| spawn_self(None));
    gtk::main();
}

// ---------- macOS: tray + global hotkey inside the window process ----------

/// Must run on the main thread after the event loop started. Keep the result alive.
#[cfg(target_os = "macos")]
pub fn mac_tray_and_hotkey(show: std::sync::Arc<dyn Fn() + Send + Sync>) -> Box<dyn std::any::Any> {
    use global_hotkey::hotkey::{Code, HotKey, Modifiers};
    use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    let open = MenuItem::new("Open Transly", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open, &quit]).unwrap();
    let (open_id, quit_id) = (open.id().clone(), quit.id().clone());

    let s = show.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == open_id {
            s();
        } else if e.id == quit_id {
            std::process::exit(0);
        }
    }));
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Transly")
        .with_icon(Icon::from_rgba(icon_rgba(64), 64, 64).unwrap())
        .build()
        .ok();

    let mgr = GlobalHotKeyManager::new().ok();
    if let Some(m) = &mgr {
        let _ = m.register(HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space));
    }
    GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
        if e.state == HotKeyState::Pressed {
            show();
        }
    }));
    Box::new((tray, mgr, open, quit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_list_merge() {
        assert_eq!(add_to_list("@as []", "/t/"), "['/t/']");
        assert_eq!(add_to_list("['/a/']", "/t/"), "['/a/', '/t/']");
        assert_eq!(add_to_list("['/a/', '/t/']", "/t/"), "['/a/', '/t/']");
    }

    #[test]
    fn config_roundtrip_defaults() {
        let c: Config = serde_json::from_str(r#"{"api_key":"k"}"#).unwrap();
        assert_eq!((c.api_key.as_str(), c.autostart), ("k", false));
    }
}
