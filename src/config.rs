//! User configuration, persisted at `~/.config/omacapture/config.toml`.

use anyhow::Result;
use gtk::{gio, glib};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    #[default]
    Png,
    Jpg,
    Webp,
}

impl ImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpg => "jpg",
            ImageFormat::Webp => "webp",
        }
    }
}

/// What happens after a capture finishes, per capture mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct AfterCapture {
    pub save: bool,
    pub copy: bool,
    pub quick_access: bool,
    pub annotate: bool,
}

impl Default for AfterCapture {
    fn default() -> Self {
        Self { save: true, copy: true, quick_access: true, annotate: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Folder screenshots are written to.
    pub save_folder: PathBuf,
    /// strftime-style pattern used for filenames (without extension).
    pub filename_pattern: String,
    pub format: ImageFormat,
    /// JPG / WebP quality, 1-100.
    pub quality: u8,
    pub include_cursor: bool,
    /// Play the shutter sound.
    pub sound: bool,
    pub notifications: bool,
    /// Remember the last selected region and offer it on the next area capture.
    pub remember_last_area: bool,
    /// Milliseconds to wait between the hotkey and the capture (0 = none).
    pub delay_ms: u32,
}

impl Default for General {
    fn default() -> Self {
        Self {
            // Follow Omarchy's own screenshot tooling when it is configured.
            save_folder: std::env::var_os("OMARCHY_SCREENSHOT_DIR").map(PathBuf::from).unwrap_or_else(|| {
                dirs::picture_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Pictures")).join("Screenshots")
            }),
            filename_pattern: "Screenshot %Y-%m-%d at %H.%M.%S".into(),
            format: ImageFormat::Png,
            quality: 90,
            include_cursor: false,
            sound: true,
            notifications: true,
            remember_last_area: true,
            delay_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PostCapture {
    pub fullscreen: AfterCapture,
    pub area: AfterCapture,
    pub window: AfterCapture,
    pub annotate_export: AfterCapture,
}

impl Default for PostCapture {
    fn default() -> Self {
        Self {
            fullscreen: AfterCapture::default(),
            area: AfterCapture::default(),
            window: AfterCapture::default(),
            annotate_export: AfterCapture { save: true, copy: true, quick_access: false, annotate: false },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
}

/// Keys that act on Quick Access cards. Hover keys are GDK key names that work
/// while the pointer is over a card; global chords are Hyprland bindings that
/// exist only while a card is visible. An empty string disables a binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QuickAccessShortcuts {
    pub hover_copy: String,
    pub hover_edit: String,
    pub hover_open: String,
    pub hover_delete: String,
    pub hover_dismiss: String,
    pub global_edit: String,
    pub global_copy: String,
    pub global_delete: String,
    pub global_open: String,
}

impl Default for QuickAccessShortcuts {
    fn default() -> Self {
        Self {
            hover_copy: "c".into(),
            hover_edit: "e".into(),
            hover_open: "o".into(),
            hover_delete: "Delete".into(),
            hover_dismiss: "Escape".into(),
            global_edit: "SUPER + E".into(),
            global_copy: "SUPER + D".into(),
            global_delete: "SUPER + DELETE".into(),
            global_open: String::new(),
        }
    }
}

/// A Hyprland chord like "SUPER + SHIFT + E"; empty means unbound.
pub fn validate_chord(chord: &str) -> Result<()> {
    let c = chord.trim();
    if c.is_empty() {
        return Ok(());
    }
    if c.contains('"') || c.contains('\\') || c.contains('\n') {
        anyhow::bail!("shortcut {c:?} contains an invalid character");
    }
    let parts: Vec<&str> = c.split('+').map(|p| p.trim()).collect();
    if parts.iter().any(|p| p.is_empty()) {
        anyhow::bail!("shortcut {c:?} has an empty part");
    }
    let (mods, key) = parts.split_at(parts.len() - 1);
    for m in mods {
        if !matches!(m.to_ascii_uppercase().as_str(), "SUPER" | "SHIFT" | "CTRL" | "ALT") {
            anyhow::bail!("shortcut {c:?}: unknown modifier {m:?} (use SUPER, SHIFT, CTRL, ALT)");
        }
    }
    if !key[0].chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        anyhow::bail!("shortcut {c:?}: key {:?} must be a plain key name", key[0]);
    }
    Ok(())
}

/// A single GDK key name such as "c", "Delete", or "F5"; empty means unbound.
pub fn validate_hover_key(name: &str) -> Result<()> {
    let n = name.trim();
    if n.is_empty() {
        return Ok(());
    }
    if gtk::gdk::Key::from_name(n).is_none() {
        anyhow::bail!("{n:?} is not a key name (examples: c, Delete, Escape, F5)");
    }
    Ok(())
}

impl QuickAccessShortcuts {
    pub fn validate(&self) -> Result<()> {
        for k in [&self.hover_copy, &self.hover_edit, &self.hover_open, &self.hover_delete, &self.hover_dismiss] {
            validate_hover_key(k)?;
        }
        for c in [&self.global_edit, &self.global_copy, &self.global_delete, &self.global_open] {
            validate_chord(c)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QuickAccess {
    pub enabled: bool,
    pub corner: Corner,
    /// Seconds before a card dismisses itself; 0 keeps it until dismissed.
    pub auto_dismiss_secs: u32,
    pub max_cards: usize,
    pub thumbnail_width: i32,
    /// Keep the editor open after dragging out of Quick Access.
    pub keep_editing_after_drag: bool,
    /// While a card is visible, register the global chords below with Hyprland
    /// for the newest capture. Removed again as soon as the last card goes away.
    pub global_shortcuts: bool,
    pub shortcuts: QuickAccessShortcuts,
}

impl Default for QuickAccess {
    fn default() -> Self {
        Self {
            enabled: true,
            corner: Corner::BottomRight,
            auto_dismiss_secs: 8,
            max_cards: 4,
            thumbnail_width: 240,
            keep_editing_after_drag: false,
            global_shortcuts: true,
            shortcuts: QuickAccessShortcuts::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Annotate {
    pub stroke_color: String,
    pub fill_color: String,
    pub text_color: String,
    pub stroke_width: f64,
    pub font_family: String,
    pub font_size: f64,
    pub blur_style: String,
    pub blur_strength: f64,
    pub corner_radius: f64,
    /// Auto-crop transparent edges when exporting a remove-background result.
    pub auto_crop: bool,
    /// Automatically redact things that look like secrets when opening the editor.
    pub auto_redact: bool,
    pub watermark_text: String,
    pub watermark_image: Option<PathBuf>,
    /// Frame new captures in the editor: "none", "wallpaper" (the Omarchy frame), or "blurred".
    pub default_background: String,
    /// Padding for that frame; 0 lets the preset pick from the capture size.
    pub default_padding: f64,
}

impl Default for Annotate {
    fn default() -> Self {
        Self {
            stroke_color: "#ff3b30".into(),
            fill_color: "#ff3b3080".into(),
            text_color: "#ff3b30".into(),
            stroke_width: 4.0,
            font_family: "Sans".into(),
            font_size: 28.0,
            blur_style: "pixelate".into(),
            blur_strength: 12.0,
            corner_radius: 8.0,
            auto_crop: true,
            auto_redact: false,
            watermark_text: String::new(),
            watermark_image: None,
            default_background: "none".into(),
            default_padding: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    pub enabled: bool,
    /// Days to keep entries; 0 keeps forever.
    pub retention_days: u32,
    pub max_entries: u32,
}

impl Default for History {
    fn default() -> Self {
        Self { enabled: true, retention_days: 30, max_entries: 500 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Ocr {
    /// Tesseract language codes, e.g. "eng" or "eng+deu".
    pub languages: String,
    pub copy_to_clipboard: bool,
}

impl Default for Ocr {
    fn default() -> Self {
        Self { languages: std::env::var("OMARCHY_OCR_LANGS").unwrap_or_else(|_| "eng".into()), copy_to_clipboard: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Mcp {
    /// Extra directories the MCP server may write images into, on top of the
    /// save folder, the capture cache, and the source image's own directory.
    pub allowed_write_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Keybinds {
    /// Last successfully installed Hyprland preset: "print" or "super-i".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
}

impl Keybinds {
    fn is_unset(&self) -> bool {
        self.preset.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub post_capture: PostCapture,
    pub quick_access: QuickAccess,
    pub annotate: Annotate,
    pub history: History,
    pub ocr: Ocr,
    pub mcp: Mcp,
    #[serde(default, skip_serializing_if = "Keybinds::is_unset")]
    pub keybinds: Keybinds,
}

impl Config {
    pub fn load() -> Self {
        let path = crate::paths::config_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(mut cfg) => {
                    if let Err(e) = cfg.validate() {
                        tracing::warn!("config value rejected ({e}); using the default for it");
                        let d = Config::default();
                        if validate_filename_pattern(&cfg.general.filename_pattern).is_err() {
                            cfg.general.filename_pattern = d.general.filename_pattern;
                        }
                        if !cfg.general.save_folder.is_absolute() {
                            cfg.general.save_folder = d.general.save_folder;
                        }
                        cfg.general.quality = cfg.general.quality.clamp(1, 100);
                        cfg.quick_access.thumbnail_width = cfg.quick_access.thumbnail_width.clamp(60, 2000);
                        cfg.quick_access.max_cards = cfg.quick_access.max_cards.clamp(1, 10);
                        cfg.annotate.blur_strength = cfg.annotate.blur_strength.clamp(1.0, 20.0);
                        cfg.annotate.stroke_width = cfg.annotate.stroke_width.clamp(0.5, 64.0);
                        cfg.annotate.font_size = cfg.annotate.font_size.clamp(4.0, 400.0);
                        cfg.general.delay_ms = cfg.general.delay_ms.min(60_000);
                        if cfg.quick_access.shortcuts.validate().is_err() {
                            cfg.quick_access.shortcuts = QuickAccessShortcuts::default();
                        }
                    }
                    cfg
                }
                Err(e) => {
                    tracing::warn!("config parse error in {}: {e}; using defaults", path.display());
                    Config::default()
                }
            },
            Err(_) => {
                let cfg = Config::default();
                if let Err(e) = cfg.save() {
                    tracing::warn!("could not write default config: {e}");
                }
                cfg
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        crate::paths::ensure_dirs();
        let text = toml::to_string_pretty(self)?;
        std::fs::write(crate::paths::config_file(), text)?;
        Ok(())
    }
}

/// Shared handle to the live configuration.
#[derive(Clone)]
pub struct ConfigHandle(Arc<RwLock<Config>>);

impl ConfigHandle {
    pub fn new(cfg: Config) -> Self {
        Self(Arc::new(RwLock::new(cfg)))
    }
    pub fn get(&self) -> Config {
        self.0.read().unwrap().clone()
    }
    pub fn update(&self, f: impl FnOnce(&mut Config)) {
        let mut guard = self.0.write().unwrap();
        f(&mut guard);
        if let Err(e) = guard.save() {
            tracing::warn!("saving config failed: {e}");
        }
    }

    /// Re-read the file (after an external edit) if it parses.
    pub fn reload(&self) -> bool {
        let path = crate::paths::config_file();
        match std::fs::read_to_string(&path).ok().and_then(|t| toml::from_str::<Config>(&t).ok()).filter(|c| c.validate().is_ok()) {
            Some(cfg) => {
                *self.0.write().unwrap() = cfg;
                tracing::info!("config reloaded from {}", path.display());
                true
            }
            None => false,
        }
    }

    /// Watch the config file and its directory so manual or MCP edits apply live.
    pub fn watch(&self) -> Option<gio::FileMonitor> {
        use gio::prelude::*;
        let dir = gio::File::for_path(crate::paths::config_dir());
        let monitor = dir.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE).ok()?;
        monitor.set_rate_limit(100);
        let handle = self.clone();
        monitor.connect_changed(move |_, file, _, event| {
            let is_config = file.basename().map(|b| b == std::path::Path::new("config.toml")).unwrap_or(false);
            if is_config
                && matches!(
                    event,
                    gio::FileMonitorEvent::ChangesDoneHint
                        | gio::FileMonitorEvent::Changed
                        | gio::FileMonitorEvent::Created
                        | gio::FileMonitorEvent::MovedIn
                        | gio::FileMonitorEvent::Renamed
                )
            {
                let h = handle.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                    h.reload();
                });
            }
        });
        Some(monitor)
    }
}

impl Config {
    /// Every key with its current value, as TOML text.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Merge a JSON object of overrides (nested by section) into this config.
    pub fn merge_json(&mut self, patch: &serde_json::Value) -> Result<()> {
        let mut current = serde_json::to_value(&*self)?;
        deep_merge(&mut current, patch);
        let candidate: Config = serde_json::from_value(current)?;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    /// Reject values that would crash the daemon or write outside the save folder.
    pub fn validate(&self) -> Result<()> {
        validate_filename_pattern(&self.general.filename_pattern)?;
        if !self.general.save_folder.is_absolute() {
            anyhow::bail!("general.save_folder must be an absolute path");
        }
        if !(1..=100).contains(&self.general.quality) {
            anyhow::bail!("general.quality must be 1-100");
        }
        if !(60..=2000).contains(&self.quick_access.thumbnail_width) {
            anyhow::bail!("quick_access.thumbnail_width must be 60-2000");
        }
        if !(1..=10).contains(&self.quick_access.max_cards) {
            anyhow::bail!("quick_access.max_cards must be 1-10");
        }
        if !(1.0..=20.0).contains(&self.annotate.blur_strength) {
            anyhow::bail!("annotate.blur_strength must be 1-20");
        }
        if !(0.5..=64.0).contains(&self.annotate.stroke_width) {
            anyhow::bail!("annotate.stroke_width must be 0.5-64");
        }
        if !(4.0..=400.0).contains(&self.annotate.font_size) {
            anyhow::bail!("annotate.font_size must be 4-400");
        }
        if self.general.delay_ms > 60_000 {
            anyhow::bail!("general.delay_ms must be at most 60000");
        }
        self.quick_access.shortcuts.validate()?;
        if !matches!(self.annotate.default_background.as_str(), "none" | "wallpaper" | "blurred") {
            anyhow::bail!("annotate.default_background must be none, wallpaper, or blurred");
        }
        if !(0.0..=1024.0).contains(&self.annotate.default_padding) {
            anyhow::bail!("annotate.default_padding must be 0-1024");
        }
        Ok(())
    }
}

/// A strftime pattern must be well formed and must stay inside the save folder.
pub fn validate_filename_pattern(pattern: &str) -> Result<()> {
    use chrono::format::{Item, StrftimeItems};
    if pattern.trim().is_empty() {
        anyhow::bail!("filename_pattern must not be empty");
    }
    if pattern.contains('/') || pattern.contains('\\') || pattern.contains("..") {
        anyhow::bail!("filename_pattern must not contain path separators or '..'");
    }
    if StrftimeItems::new(pattern).any(|i| matches!(i, Item::Error)) {
        anyhow::bail!("filename_pattern contains an invalid strftime specifier");
    }
    Ok(())
}

fn deep_merge(target: &mut serde_json::Value, patch: &serde_json::Value) {
    match (target, patch) {
        (serde_json::Value::Object(t), serde_json::Value::Object(p)) => {
            for (k, v) in p {
                match t.get_mut(k) {
                    Some(existing) if existing.is_object() && v.is_object() => deep_merge(existing, v),
                    _ => {
                        t.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (t, p) => *t = p.clone(),
    }
}
