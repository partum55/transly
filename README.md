# Transly

A small DeepL-style translator for the desktop (Linux + macOS), powered by the free Gemini API.
Translates typed text and files: text formats (.txt .md .srt …), Word (.docx) and PDF.

## Install
Download from the repository's **Releases** page:
- **Linux (Debian/Ubuntu):** `sudo apt install ./transly_*.deb`, then open "Transly" from the app menu.
- **macOS:** open `Transly-*.dmg` and drag Transly to Applications. The app is not notarized, so the first time
  right-click it, choose **Open**, then **Open** again (or run `xattr -dr com.apple.quarantine /Applications/Transly.app`).

**First run:** it asks for a Gemini API key (free: https://aistudio.google.com/apikey). After the key is
accepted it enables start-at-login, adds a tray icon and registers the shortcut **Ctrl+Shift+Space** (GNOME).

Build from source: `cargo build --release` (Linux needs `libgtk-3-dev libayatana-appindicator3-dev`).
Releases are built by GitHub Actions when a tag like `v0.1.0` is pushed.

## How it behaves
- Linux: a tiny tray process starts at login; "Open Transly" or the shortcut opens a window, closing it quits
  that window (Wayland can't hide windows). The shortcut is registered for GNOME only.
- macOS: one process; closing the window hides it to the menu bar.
- Files are saved next to the original as `name.<lang>.ext`. A PDF is translated to Markdown and then rendered
  to `name.<lang>.pdf`; the `.md` is kept alongside it.
- Config (with the API key, mode 0600): `~/.config/transly/config.json` (macOS: `~/Library/Application Support/transly/`).

## Limits
- The free Gemini tier has a small daily request quota and Google may use free-tier data to improve its
  products. Don't translate confidential files.
- DOCX: only the body text is translated (not headers/footers); formatting inside a paragraph is flattened.
- PDF: the result is a clean re-typeset document (headings, lists, tables), not a copy of the original layout
  or images, and very long PDFs can exceed the model's output limit. Chinese/Japanese/Korean needs a system CJK font.

## Dev
`cargo test`. Live API check: `GEMINI_API_KEY=… cargo test live -- --ignored --nocapture`.
