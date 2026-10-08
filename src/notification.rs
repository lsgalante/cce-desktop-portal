//! `org.freedesktop.impl.portal.Notification`: portal notifications as
//! cce-notifier cards, and clicks on them back to the app.
//!
//! cce-notifier is the desktop's `org.freedesktop.Notifications` server, so
//! each portal call becomes one call on that interface:
//!
//! - `AddNotification(app_id, id, n)` → `Notify`, with the app's desktop-entry
//!   name as `app_name`, `title`/`body` (or `markup-body`, tags stripped —
//!   the cards draw plain text), the icon as `app_icon` (a theme name, or a
//!   file written under `$XDG_RUNTIME_DIR` for bytes/fd icons), `priority` as
//!   `urgency`, and the actions: `default-action` as the `default` key (the
//!   click on the card), each button as `b<index>`. Re-adding an id replaces
//!   its card (`replaces_id`).
//! - `RemoveNotification` → `CloseNotification`.
//! - The server's `ActionInvoked` is mapped back through the key to the
//!   portal action and its target, and re-emitted as the portal's
//!   `ActionInvoked(app_id, id, action, [target])`; `NotificationClosed`
//!   forgets the card.
//!
//! Before this backend, gtk did the same forwarding; but cce-notifier then
//! ignored actions and themed icons, so a portal notification could be read
//! and never acted on.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::Mutex;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{interface, Connection};

const FDO_NAME: &str = "org.freedesktop.Notifications";
const FDO_PATH: &str = "/org/freedesktop/Notifications";

/// What a card's keys mean, kept until the card closes.
struct Entry {
    fdo_id: u32,
    /// `default-action` and its target.
    default: Option<(String, Option<OwnedValue>)>,
    /// Each button's action and target, by index (`b<i>`).
    buttons: Vec<(String, Option<OwnedValue>)>,
}

#[derive(Default)]
pub struct Cards {
    by_key: HashMap<(String, String), Entry>,
    by_fdo: HashMap<u32, (String, String)>,
}

pub type SharedCards = Arc<Mutex<Cards>>;

pub struct NotificationPortal {
    pub cards: SharedCards,
}

// ── parsing the portal's vardict ───────────────────────────────────────────

fn str_of(n: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    match n.get(key).map(|v| &**v) {
        Some(Value::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// The text a card can draw of a `markup-body`: tags dropped, the five XML
/// entities decoded. cce-notifier draws plain text.
pub fn strip_markup(markup: &str) -> String {
    let mut out = String::with_capacity(markup.len());
    let mut in_tag = false;
    for c in markup.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// `priority` → the freedesktop `urgency` byte. `high` stays normal: the
/// difference cce-notifier draws is whether a card expires, and only
/// `urgent` should outstay its welcome.
pub fn urgency(priority: Option<&str>) -> u8 {
    match priority {
        Some("low") => 0,
        Some("urgent") => 2,
        _ => 1,
    }
}

/// The notification's `icon`, a serialized GIcon `(sv)`: `("themed", as)`,
/// `("bytes", ay)` or `("file-descriptor", h)`. A theme name is passed on as
/// is; image data is written to a file named after its contents (so a
/// repeated icon is written once) and passed as a path.
fn icon_of(n: &HashMap<String, OwnedValue>) -> Option<String> {
    let Some(Value::Structure(s)) = n.get("icon").map(|v| &**v) else { return None };
    let fields = s.fields();
    let (Some(Value::Str(kind)), Some(payload)) = (fields.first(), fields.get(1)) else { return None };
    let payload = match payload {
        Value::Value(inner) => &**inner,
        other => other,
    };
    match (kind.as_str(), payload) {
        ("themed", Value::Array(names)) => names.iter().find_map(|v| match v {
            Value::Str(s) if !s.is_empty() => Some(s.to_string()),
            _ => None,
        }),
        ("bytes", Value::Array(bytes)) => {
            let data: Vec<u8> = bytes.iter().filter_map(|v| if let Value::U8(b) = v { Some(*b) } else { None }).collect();
            write_icon(&data)
        }
        ("file-descriptor", Value::Fd(fd)) => {
            use std::io::Read;
            use std::os::fd::AsFd;
            let owned = fd.as_fd().try_clone_to_owned().ok()?;
            let mut data = Vec::new();
            std::fs::File::from(owned).read_to_end(&mut data).ok()?;
            write_icon(&data)
        }
        _ => None,
    }
}

/// An extension `cce_ui::icon` can decode, by the data's own signature.
pub fn icon_ext(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG") {
        Some("png")
    } else if data.windows(4).take(1024).any(|w| w == b"<svg") {
        Some("svg")
    } else {
        None
    }
}

fn write_icon(data: &[u8]) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let ext = icon_ext(data)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    let dir = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("cce-desktop-portal");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("icon-{:016x}.{ext}", h.finish()));
    if !path.exists() {
        std::fs::write(&path, data).ok()?;
    }
    Some(path.to_string_lossy().into_owned())
}

/// `buttons`: `aa{sv}`, each with `label`, `action` and an optional `target`.
fn buttons_of(n: &HashMap<String, OwnedValue>) -> Vec<(String, String, Option<OwnedValue>)> {
    let Some(Value::Array(list)) = n.get("buttons").map(|v| &**v) else { return Vec::new() };
    list.iter()
        .filter_map(|b| {
            let Value::Dict(d) = b else { return None };
            // Values in an `a{sv}` arrive wrapped in a variant; unwrap one level.
            let get = |k: &str| {
                d.iter().find_map(|(key, val)| match key {
                    Value::Str(s) if s.as_str() == k => Some(match val {
                        Value::Value(inner) => &**inner,
                        v => v,
                    }),
                    _ => None,
                })
            };
            let label = match get("label") {
                Some(Value::Str(s)) => s.to_string(),
                _ => return None,
            };
            let action = match get("action") {
                Some(Value::Str(s)) => s.to_string(),
                _ => return None,
            };
            let target = get("target").and_then(|v| v.try_to_owned().ok());
            Some((label, action, target))
        })
        .collect()
}

/// `Name=` and `Icon=` of the app's desktop entry, from the first
/// applications dir that has `<app_id>.desktop`.
fn desktop_entry(app_id: &str) -> (Option<String>, Option<String>) {
    if app_id.is_empty() {
        return (None, None);
    }
    let home_data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")));
    let data_dirs = std::env::var("XDG_DATA_DIRS").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let dirs = home_data.into_iter().chain(std::env::split_paths(&data_dirs));
    for dir in dirs {
        let Ok(text) = std::fs::read_to_string(dir.join("applications").join(format!("{app_id}.desktop"))) else { continue };
        let (mut name, mut icon) = (None, None);
        let mut in_entry = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_entry = line == "[Desktop Entry]";
            } else if in_entry {
                if let Some(v) = line.strip_prefix("Name=") {
                    name.get_or_insert_with(|| v.to_string());
                } else if let Some(v) = line.strip_prefix("Icon=") {
                    icon.get_or_insert_with(|| v.to_string());
                }
            }
        }
        return (name, icon);
    }
    (None, None)
}

/// The fdo `actions` list for a card: `default` first, then `b<i>` per
/// button, each followed by its label.
pub fn fdo_actions(has_default: bool, labels: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if has_default {
        out.extend(["default".to_string(), String::new()]);
    }
    for (i, label) in labels.iter().enumerate() {
        out.extend([format!("b{i}"), label.clone()]);
    }
    out
}

#[interface(name = "org.freedesktop.impl.portal.Notification")]
impl NotificationPortal {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    /// No categories or custom sounds: cce-notifier has neither.
    #[zbus(property, name = "SupportedOptions")]
    fn supported_options(&self) -> HashMap<String, OwnedValue> {
        HashMap::new()
    }

    async fn add_notification(
        &self,
        #[zbus(connection)] conn: &Connection,
        app_id: String,
        id: String,
        notification: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        let title = str_of(&notification, "title").unwrap_or_default();
        let body = str_of(&notification, "markup-body")
            .map(|m| strip_markup(&m))
            .or_else(|| str_of(&notification, "body"))
            .unwrap_or_default();
        let (entry_name, entry_icon) = desktop_entry(&app_id);
        let app_name = entry_name.unwrap_or_else(|| app_id.clone());
        let icon = icon_of(&notification).or(entry_icon).unwrap_or_default();
        let default = str_of(&notification, "default-action")
            .map(|a| (a, notification.get("default-action-target").and_then(|v| v.try_clone().ok())));
        let buttons = buttons_of(&notification);
        let labels: Vec<String> = buttons.iter().map(|(l, _, _)| l.clone()).collect();
        let actions = fdo_actions(default.is_some(), &labels);

        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("urgency", Value::U8(urgency(str_of(&notification, "priority").as_deref())));
        if !app_id.is_empty() {
            hints.insert("desktop-entry", Value::from(app_id.clone()));
        }
        if let Some(category) = str_of(&notification, "category") {
            hints.insert("category", Value::from(category));
        }

        let key = (app_id.clone(), id.clone());
        let replaces = self.cards.lock().await.by_key.get(&key).map_or(0, |e| e.fdo_id);
        let reply = conn
            .call_method(
                Some(FDO_NAME),
                FDO_PATH,
                Some(FDO_NAME),
                "Notify",
                &(&app_name, replaces, &icon, &title, &body, &actions, &hints, -1i32),
            )
            .await?;
        let fdo_id: u32 = reply.body().deserialize()?;
        log::info!("AddNotification {app_id}/{id} → card {fdo_id} ({} button(s), default {})", labels.len(), default.is_some());

        let mut cards = self.cards.lock().await;
        if let Some(old) = cards.by_key.remove(&key) {
            cards.by_fdo.remove(&old.fdo_id);
        }
        cards.by_fdo.insert(fdo_id, key.clone());
        cards.by_key.insert(key, Entry { fdo_id, default, buttons: buttons.into_iter().map(|(_, a, t)| (a, t)).collect() });
        Ok(())
    }

    async fn remove_notification(&self, #[zbus(connection)] conn: &Connection, app_id: String, id: String) {
        let entry = {
            let mut cards = self.cards.lock().await;
            let entry = cards.by_key.remove(&(app_id.clone(), id.clone()));
            if let Some(e) = &entry {
                cards.by_fdo.remove(&e.fdo_id);
            }
            entry
        };
        if let Some(e) = entry {
            log::info!("RemoveNotification {app_id}/{id} (card {})", e.fdo_id);
            if let Err(err) = conn.call_method(Some(FDO_NAME), FDO_PATH, Some(FDO_NAME), "CloseNotification", &(e.fdo_id,)).await {
                log::warn!("closing card {}: {err}", e.fdo_id);
            }
        }
    }

    #[zbus(signal)]
    async fn action_invoked(emitter: &SignalEmitter<'_>, app_id: &str, id: &str, action: &str, parameter: Vec<Value<'_>>) -> zbus::Result<()>;
}

/// Follow cce-notifier's signals: a click on one of our cards becomes the
/// portal's `ActionInvoked`; a closed card is forgotten.
pub async fn relay(conn: Connection, portal_path: &str, cards: SharedCards) -> zbus::Result<()> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(FDO_NAME)?
        .path(FDO_PATH)?
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, &conn, None).await?;
    let emitter = SignalEmitter::new(&conn, portal_path.to_string())?;
    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else { continue };
        let header = msg.header();
        match header.member().map(|m| m.as_str()) {
            Some("ActionInvoked") => {
                let Ok((fdo_id, key)) = msg.body().deserialize::<(u32, String)>() else { continue };
                let cards = cards.lock().await;
                let Some((app_id, id)) = cards.by_fdo.get(&fdo_id) else { continue };
                let Some(entry) = cards.by_key.get(&(app_id.clone(), id.clone())) else { continue };
                let chosen = if key == "default" {
                    entry.default.as_ref()
                } else {
                    key.strip_prefix('b').and_then(|i| i.parse::<usize>().ok()).and_then(|i| entry.buttons.get(i))
                };
                let Some((action, target)) = chosen else { continue };
                let parameter: Vec<Value> = target.iter().filter_map(|t| t.try_clone().ok()).map(Value::from).collect();
                log::info!("card {fdo_id}: {app_id}/{id} action {action:?}");
                if let Err(e) = NotificationPortal::action_invoked(&emitter, app_id, id, action, parameter).await {
                    log::warn!("emitting ActionInvoked: {e}");
                }
            }
            Some("NotificationClosed") => {
                let Ok((fdo_id, _reason)) = msg.body().deserialize::<(u32, u32)>() else { continue };
                let mut cards = cards.lock().await;
                if let Some(key) = cards.by_fdo.remove(&fdo_id) {
                    cards.by_key.remove(&key);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_becomes_plain_text() {
        assert_eq!(strip_markup("<b>Build</b> finished &amp; <i>passed</i> &lt;3"), "Build finished & passed <3");
        assert_eq!(strip_markup("a <a href=\"x\">link</a>"), "a link");
    }

    #[test]
    fn priority_to_urgency() {
        assert_eq!(urgency(Some("low")), 0);
        assert_eq!(urgency(Some("normal")), 1);
        assert_eq!(urgency(Some("high")), 1);
        assert_eq!(urgency(Some("urgent")), 2);
        assert_eq!(urgency(None), 1);
    }

    #[test]
    fn actions_are_default_then_indexed_buttons() {
        assert_eq!(fdo_actions(true, &["Reply".into(), "Mute".into()]), ["default", "", "b0", "Reply", "b1", "Mute"]);
        assert_eq!(fdo_actions(false, &[]), Vec::<String>::new());
    }

    #[test]
    fn icon_signatures() {
        assert_eq!(icon_ext(b"\x89PNG\r\n\x1a\n...."), Some("png"));
        assert_eq!(icon_ext(b"<?xml version='1.0'?>\n<svg xmlns='...'/>"), Some("svg"));
        assert_eq!(icon_ext(b"GIF89a"), None);
    }

    #[test]
    fn themed_icon_is_its_first_name() {
        let icon = Value::from(("themed", Value::from(vec!["org.example.App-symbolic", "org.example.App"])));
        let n = HashMap::from([("icon".to_string(), icon.try_to_owned().unwrap())]);
        assert_eq!(icon_of(&n).as_deref(), Some("org.example.App-symbolic"));
    }
}
