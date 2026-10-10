//! `org.freedesktop.impl.portal.Settings`: the `org.freedesktop.appearance`
//! namespace, taken from cce's own config instead of GNOME's.
//!
//! Before this backend the namespace came from xdg-desktop-portal-gtk, which
//! reads GSettings — `org.gnome.desktop.interface color-scheme`, a key nothing
//! in cce writes, set once by hand. Now:
//!
//! - **color-scheme** — `style { color_scheme "dark" | "light" | "default" }`
//!   in config.kdl. cce is a dark desktop, so an absent key means dark.
//! - **accent-color** — `style { highlight primary=(rgb)"#rrggbb" }`, the
//!   colour cce-ui already highlights with.
//! - **reduced-motion** — the DE's animations switch, `/run/cce/animations`
//!   (`off` = reduce), which `cce-power-apply` flips per power mode.
//! - **contrast** — always normal: cce has no high-contrast mode.
//!
//! Only this namespace is served. The portal frontend merges Settings from
//! every backend `cce-portals.conf` lists (`cce-desktop;gtk`), first one
//! wins per key, so the `org.gnome.*` keys sandboxed GTK apps read still come
//! from gtk.
//!
//! Changes are pushed, not polled: inotify on the config directory and on
//! `/run/cce` re-reads both, and only keys whose value moved are signalled.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use tokio::sync::RwLock;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{interface, Connection};

pub const APPEARANCE: &str = "org.freedesktop.appearance";

/// The values `org.freedesktop.appearance` publishes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Appearance {
    /// 0 no preference, 1 prefer dark, 2 prefer light.
    pub color_scheme: u32,
    /// sRGB, each channel 0..=1. `None` publishes no accent at all (the
    /// spec's "unset"), rather than an invented one.
    pub accent: Option<(f64, f64, f64)>,
    /// 0 normal, 1 high.
    pub contrast: u32,
    /// 0 no preference, 1 reduce.
    pub reduced_motion: u32,
}

impl Default for Appearance {
    fn default() -> Self {
        Appearance { color_scheme: 1, accent: None, contrast: 0, reduced_motion: 0 }
    }
}

impl Appearance {
    /// The namespace's keys and values. The accent is left out while unset.
    pub fn entries(&self) -> Vec<(&'static str, OwnedValue)> {
        let mut out = vec![
            ("color-scheme", OwnedValue::from(self.color_scheme)),
            ("contrast", OwnedValue::from(self.contrast)),
            ("reduced-motion", OwnedValue::from(self.reduced_motion)),
        ];
        if let Some(rgb) = self.accent {
            let v = Value::from(rgb);
            out.push(("accent-color", v.try_to_owned().expect("a (ddd) has no fds")));
        }
        out
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("cce").join("config.kdl")
}

/// The animations switch's state file (cce-core `plan`, written as root by
/// cce-power-apply). Missing means on.
pub const ANIMATIONS_FILE: &str = cce_core::plan::ANIMATIONS_PATH;

/// `color-scheme` and `accent-color` from config.kdl's `style { }` block.
/// Anything unreadable falls back to the defaults, never to an error: a
/// half-saved config must not take the setting away from every app.
pub fn parse_config(text: &str) -> (u32, Option<(f64, f64, f64)>) {
    let mut scheme = Appearance::default().color_scheme;
    let mut accent = None;
    let Ok(doc) = text.parse::<kdl::KdlDocument>() else {
        return (scheme, accent);
    };
    let Some(style) = doc.get("style").and_then(|n| n.children()) else {
        return (scheme, accent);
    };
    if let Some(v) = style.get_arg("color_scheme").and_then(|v| v.as_string()) {
        scheme = match v {
            "light" | "prefer-light" => 2,
            "default" | "none" => 0,
            _ => 1,
        };
    }
    if let Some(hex) = style.get("highlight").and_then(|n| n.get("primary")).and_then(|e| e.value().as_string()) {
        accent = parse_hex_rgb(hex);
    }
    (scheme, accent)
}

/// `#rgb`, `#rrggbb` or `#rrggbbaa` (alpha ignored) as 0..=1 channels.
pub fn parse_hex_rgb(hex: &str) -> Option<(f64, f64, f64)> {
    let h = hex.trim().strip_prefix('#')?;
    let channel = |s: &str| u8::from_str_radix(s, 16).ok().map(|v| v as f64 / 255.0);
    match h.len() {
        3 => {
            let d = |i: usize| channel(&h[i..i + 1].repeat(2));
            Some((d(0)?, d(1)?, d(2)?))
        }
        6 | 8 => Some((channel(&h[0..2])?, channel(&h[2..4])?, channel(&h[4..6])?)),
        _ => None,
    }
}

/// `reduced-motion` from the animations file's contents (None = absent).
pub fn reduced_motion(animations: Option<&str>) -> u32 {
    match animations.map(str::trim) {
        Some("off") | Some("0") | Some("false") => 1,
        _ => 0,
    }
}

pub fn load(config: &Path) -> Appearance {
    let (color_scheme, accent) = std::fs::read_to_string(config).map(|t| parse_config(&t)).unwrap_or((1, None));
    let animations = std::fs::read_to_string(ANIMATIONS_FILE).ok();
    Appearance { color_scheme, accent, contrast: 0, reduced_motion: reduced_motion(animations.as_deref()) }
}

/// Does `namespaces` (ReadAll's argument) ask for `ns`? Empty asks for
/// everything; an entry may end in `*` to match a prefix.
pub fn namespace_wanted(namespaces: &[String], ns: &str) -> bool {
    namespaces.is_empty()
        || namespaces.iter().any(|want| match want.strip_suffix('*') {
            Some(prefix) => ns.starts_with(prefix),
            None => want == ns,
        })
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.portal.Error")]
pub enum PortalError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotFound(String),
}

pub struct SettingsPortal {
    pub state: Arc<RwLock<Appearance>>,
}

#[interface(name = "org.freedesktop.impl.portal.Settings")]
impl SettingsPortal {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    async fn read_all(&self, namespaces: Vec<String>) -> HashMap<String, HashMap<String, OwnedValue>> {
        let mut out = HashMap::new();
        if namespace_wanted(&namespaces, APPEARANCE) {
            let entries = self.state.read().await.entries();
            out.insert(APPEARANCE.to_string(), entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
        }
        out
    }

    async fn read(&self, namespace: &str, key: &str) -> Result<OwnedValue, PortalError> {
        if namespace == APPEARANCE {
            if let Some((_, v)) = self.state.read().await.entries().into_iter().find(|(k, _)| *k == key) {
                return Ok(v);
            }
        }
        // The frontend moves on to the next backend (gtk) on NotFound.
        Err(PortalError::NotFound(format!("{namespace} {key}")))
    }

    #[zbus(signal)]
    pub async fn setting_changed(emitter: &SignalEmitter<'_>, namespace: &str, key: &str, value: Value<'_>) -> zbus::Result<()>;
}

/// Watch the config directory and `/run/cce`, re-read on any change to the
/// two files, and signal each key whose value moved.
pub async fn watch(conn: Connection, path: &str, state: Arc<RwLock<Appearance>>) -> std::io::Result<()> {
    let config = config_path();
    let inotify = Inotify::init()?;
    let mask = WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::CREATE | WatchMask::DELETE;
    // Directories, not the files: editors and cce-ui replace config.kdl by
    // rename, which a watch on the old inode would never see.
    if let Some(dir) = config.parent() {
        inotify.watches().add(dir, mask)?;
    }
    let anim = Path::new(ANIMATIONS_FILE);
    if let Some(dir) = anim.parent() {
        if let Err(e) = inotify.watches().add(dir, mask) {
            log::warn!("not watching {}: {e} (reduced-motion is read once)", dir.display());
        }
    }
    let config_name = config.file_name().map(|n| n.to_os_string());
    let anim_name = anim.file_name().map(|n| n.to_os_string());
    let emitter = SignalEmitter::new(&conn, path.to_string()).map_err(std::io::Error::other)?;
    let mut events = inotify.into_event_stream([0u8; 4096])?;
    while let Some(event) = events.next().await {
        let event = event?;
        let relevant = event.name.as_ref().is_some_and(|n| Some(n) == config_name.as_ref() || Some(n) == anim_name.as_ref());
        if !relevant {
            continue;
        }
        let fresh = load(&config);
        let old = std::mem::replace(&mut *state.write().await, fresh);
        if old == fresh {
            continue;
        }
        log::info!("appearance changed: {:?}", fresh);
        let before: HashMap<_, _> = old.entries().into_iter().collect();
        for (key, value) in fresh.entries() {
            if before.get(key) != Some(&value) {
                if let Err(e) = SettingsPortal::setting_changed(&emitter, APPEARANCE, key, Value::from(value)).await {
                    log::warn!("signalling {key}: {e}");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE_SHAPE: &str = r##"
bell "dialog"
style {
    list corner_radius=(i64)12 font="Berkeley Mono 14"
    highlight primary=(rgb)"#7dffff"
    layout {
        column gap=(i64)16
    }
}
"##;

    #[test]
    fn the_live_config_shape_is_dark_with_the_highlight_accent() {
        let (scheme, accent) = parse_config(LIVE_SHAPE);
        assert_eq!(scheme, 1, "no color_scheme key means dark");
        let (r, g, b) = accent.unwrap();
        assert!((r - 125.0 / 255.0).abs() < 1e-9 && g == 1.0 && b == 1.0);
    }

    #[test]
    fn color_scheme_key_and_fallbacks() {
        let with = |v: &str| parse_config(&format!("style {{\n    color_scheme \"{v}\"\n}}\n")).0;
        assert_eq!(with("light"), 2);
        assert_eq!(with("dark"), 1);
        assert_eq!(with("default"), 0);
        assert_eq!(parse_config("style { this is not { kdl").0, 1, "a broken config keeps the default");
        assert_eq!(parse_config("").1, None);
    }

    #[test]
    fn hex_forms() {
        assert_eq!(parse_hex_rgb("#fff"), Some((1.0, 1.0, 1.0)));
        assert_eq!(parse_hex_rgb("#00000080"), Some((0.0, 0.0, 0.0)));
        assert_eq!(parse_hex_rgb("7dffff"), None);
        assert_eq!(parse_hex_rgb("#12345"), None);
    }

    #[test]
    fn reduced_motion_follows_the_animations_switch() {
        assert_eq!(reduced_motion(Some("off\n")), 1);
        assert_eq!(reduced_motion(Some("on")), 0);
        assert_eq!(reduced_motion(None), 0, "missing means animations on");
    }

    #[test]
    fn namespace_globs() {
        assert!(namespace_wanted(&[], APPEARANCE));
        assert!(namespace_wanted(&["org.freedesktop.*".into()], APPEARANCE));
        assert!(namespace_wanted(&[APPEARANCE.into()], APPEARANCE));
        assert!(!namespace_wanted(&["org.gnome.*".into()], APPEARANCE));
    }

    #[test]
    fn an_unset_accent_is_left_out() {
        let keys = |a: Appearance| a.entries().into_iter().map(|(k, _)| k).collect::<Vec<_>>();
        assert!(!keys(Appearance::default()).contains(&"accent-color"));
        assert!(keys(Appearance { accent: Some((1.0, 0.0, 0.0)), ..Default::default() }).contains(&"accent-color"));
    }
}
