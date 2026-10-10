//! Keeping the display awake for apps that ask over D-Bus rather than with a
//! Wayland `idle-inhibit` surface: the portal's `Inhibit` interface (what
//! sandboxed and portal-aware apps call) and `org.freedesktop.ScreenSaver`
//! (what Steam, browsers and video players call directly).
//!
//! Before this, the portal's Inhibit went to xdg-desktop-portal-gtk, which
//! hands it to `org.gnome.SessionManager` or `org.freedesktop.ScreenSaver` —
//! and nothing on a cce session bus owns either, so every request was
//! accepted and did nothing.
//!
//! Every holder becomes a **lease** in the compositor (`idle inhibit <token>
//! <ttl_s> <who>` on the control socket; `idle.rs` in cce-compositor). Leases
//! lapse unless renewed, so this process renews every live one well inside
//! the ttl; if it dies, the compositor drops them within [`LEASE_TTL_S`]
//! instead of keeping the display on for good. A holder ends with its request
//! being closed (portal), `UnInhibit` (ScreenSaver), or its D-Bus connection
//! going away (both — a crashed player releases what it held).

use std::collections::HashMap;
use std::sync::Arc;

use cce_core::ipc::ctl::{IdleRequest, Request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify};
use zbus::message::Header;
use zbus::names::{OwnedUniqueName, UniqueName};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection, ObjectServer};

/// A lease's lifetime in the compositor, and how often live ones are renewed.
pub const LEASE_TTL_S: u64 = 60;
const RENEW_EVERY: std::time::Duration = std::time::Duration::from_secs(20);

/// Portal Inhibit flags (the spec's bitmask). Only these two are about the
/// idle timers; logout and user-switch have nothing to hold in cce.
const FLAG_SUSPEND: u32 = 4;
const FLAG_IDLE: u32 = 8;

// ── compositor control socket ──────────────────────────────────────────────

fn ctl_socket_path() -> String {
    cce_core::ipc::ctl::control_socket()
}

/// One request/reply round; the compositor answers one line and closes.
/// The line is cce-core's `ctl::Request`, the grammar the compositor parses.
pub async fn ctl(req: &Request) -> std::io::Result<String> {
    let mut stream = tokio::net::UnixStream::connect(ctl_socket_path()).await?;
    stream.write_all(format!("{req}\n").as_bytes()).await?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await?;
    Ok(reply)
}

/// The control socket splits on whitespace and reads one line: a holder's
/// name keeps its words but loses anything that could end the line.
pub fn sanitize_who(who: &str) -> String {
    let cleaned: String = who.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() { "unknown".to_string() } else { cleaned }
}

// ── the registry ───────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Holder {
    who: String,
    /// The D-Bus connection that asked; its disappearance ends the hold.
    owner: Option<OwnedUniqueName>,
}

#[derive(Default)]
pub struct Registry {
    holders: HashMap<String, Holder>,
    next_cookie: u32,
    /// The compositor refused `idle inhibit` (too old): logged once, and
    /// every hold is still tracked so a newer compositor gets them on renew.
    unsupported_logged: bool,
}

#[derive(Clone)]
pub struct Inhibitor {
    reg: Arc<Mutex<Registry>>,
    wake: Arc<Notify>,
}

impl Inhibitor {
    pub fn new() -> Self {
        Inhibitor { reg: Arc::new(Mutex::new(Registry::default())), wake: Arc::new(Notify::new()) }
    }

    async fn grant(&self, token: String, who: String, owner: Option<OwnedUniqueName>) {
        let who = sanitize_who(&who);
        log::info!("inhibit {token} for {who}");
        self.reg.lock().await.holders.insert(token.clone(), Holder { who: who.clone(), owner });
        self.send_lease(&token, &who).await;
        self.wake.notify_one();
    }

    async fn release(&self, token: &str) {
        if self.reg.lock().await.holders.remove(token).is_some() {
            log::info!("release {token}");
            if let Err(e) = ctl(&Request::Idle(IdleRequest::Uninhibit { token: token.to_string() })).await {
                log::warn!("control socket: {e}");
            }
        }
    }

    async fn send_lease(&self, token: &str, who: &str) {
        let lease = IdleRequest::Inhibit { token: token.to_string(), ttl_s: LEASE_TTL_S, who: who.to_string() };
        match ctl(&Request::Idle(lease)).await {
            Ok(reply) if reply.starts_with("ok") => {}
            Ok(reply) => {
                let mut reg = self.reg.lock().await;
                if !reg.unsupported_logged {
                    reg.unsupported_logged = true;
                    log::warn!("compositor refused the lease ({}); it predates external inhibitors — they take effect after a restart into a newer cce-fx", reply.trim());
                }
            }
            Err(e) => log::warn!("control socket {}: {e}", ctl_socket_path()),
        }
    }

    /// Renew every live lease well inside its ttl; sleep while there are none.
    pub async fn renew_loop(self) {
        loop {
            let live: Vec<(String, String)> =
                self.reg.lock().await.holders.iter().map(|(t, h)| (t.clone(), h.who.clone())).collect();
            if live.is_empty() {
                self.wake.notified().await;
                continue;
            }
            tokio::time::sleep(RENEW_EVERY).await;
            for (token, who) in live {
                // Released while we slept: renewing would resurrect it.
                if self.reg.lock().await.holders.contains_key(&token) {
                    self.send_lease(&token, &who).await;
                }
            }
        }
    }

    /// Drop every hold a vanished D-Bus connection left behind.
    pub async fn owner_gone(&self, name: &UniqueName<'_>) {
        let tokens: Vec<String> = self
            .reg
            .lock()
            .await
            .holders
            .iter()
            .filter(|(_, h)| h.owner.as_ref().is_some_and(|o| o.as_str() == name.as_str()))
            .map(|(t, _)| t.clone())
            .collect();
        for token in tokens {
            log::info!("{name} left the bus");
            self.release(&token).await;
        }
    }

    async fn next_cookie(&self) -> u32 {
        let mut reg = self.reg.lock().await;
        reg.next_cookie = reg.next_cookie.wrapping_add(1).max(1);
        reg.next_cookie
    }
}

/// The portal request path is unique per request; a lease token must be one
/// whitespace-free word, so it is the path with its slashes swapped.
pub fn portal_token(handle: &str) -> String {
    format!("portal{}", handle.replace('/', "-"))
}

// ── org.freedesktop.impl.portal.Inhibit ────────────────────────────────────

pub struct InhibitPortal {
    pub inhibitor: Inhibitor,
}

#[interface(name = "org.freedesktop.impl.portal.Inhibit")]
impl InhibitPortal {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        3
    }

    async fn inhibit(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        app_id: String,
        _window: String,
        flags: u32,
        options: HashMap<String, OwnedValue>,
    ) {
        let reason = match options.get("reason").map(|v| &**v) {
            Some(Value::Str(s)) => s.to_string(),
            _ => String::new(),
        };
        let token = portal_token(handle.as_str());
        log::info!("portal Inhibit {handle} app={app_id:?} flags={flags} reason={reason:?}");
        if flags & (FLAG_IDLE | FLAG_SUSPEND) != 0 {
            let who = if app_id.is_empty() { "portal-app".to_string() } else { app_id.clone() };
            // No owner to watch: the caller is the portal frontend, not the
            // app, and the frontend closes the request when the app goes.
            self.inhibitor.grant(token.clone(), who, None).await;
        }
        let request = RequestObj { token, inhibitor: self.inhibitor.clone(), path: handle.clone().into() };
        if let Err(e) = server.at(&handle, request).await {
            log::warn!("exporting request {handle}: {e}");
        }
    }

    async fn create_monitor(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(connection)] conn: &Connection,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: String,
        _window: String,
    ) -> u32 {
        log::info!("CreateMonitor {session_handle} for {app_id:?}");
        let path: OwnedObjectPath = session_handle.clone().into();
        if let Err(e) = server.at(&path, MonitorSession { path: path.clone() }).await {
            log::warn!("exporting monitor {path}: {e}");
            return 2;
        }
        // The initial state: running, screensaver off. cce has no
        // query-end, so it never changes from here.
        let conn = conn.clone();
        tokio::spawn(async move {
            if let Ok(emitter) = SignalEmitter::new(&conn, "/org/freedesktop/portal/desktop") {
                let state = HashMap::from([
                    ("screensaver-active", Value::from(false)),
                    ("session-state", Value::from(1u32)),
                ]);
                let _ = InhibitPortal::state_changed(&emitter, path.as_ref(), state).await;
            }
        });
        0
    }

    async fn query_end_response(&self, _session_handle: ObjectPath<'_>) {}

    #[zbus(signal)]
    async fn state_changed(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, state: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}

/// `org.freedesktop.impl.portal.Request` at the handle the frontend chose:
/// its `Close` is how the app (or the frontend, when the app exits) ends the
/// inhibition.
struct RequestObj {
    token: String,
    inhibitor: Inhibitor,
    path: OwnedObjectPath,
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestObj {
    async fn close(&self, #[zbus(connection)] conn: &Connection) {
        self.inhibitor.release(&self.token).await;
        remove_later::<RequestObj>(conn, self.path.clone());
    }
}

struct MonitorSession {
    path: OwnedObjectPath,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl MonitorSession {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }

    async fn close(&self, #[zbus(connection)] conn: &Connection) {
        remove_later::<MonitorSession>(conn, self.path.clone());
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// An object cannot remove itself inside its own method call (the server
/// holds it for the call), so it goes on the next turn of the loop.
fn remove_later<I: zbus::object_server::Interface>(conn: &Connection, path: OwnedObjectPath) {
    let conn = conn.clone();
    tokio::spawn(async move {
        let _ = conn.object_server().remove::<I, _>(&path).await;
    });
}

// ── org.freedesktop.ScreenSaver ────────────────────────────────────────────

pub struct ScreenSaver {
    pub inhibitor: Inhibitor,
}

#[interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaver {
    async fn inhibit(&self, #[zbus(header)] header: Header<'_>, application_name: String, reason_for_inhibit: String) -> u32 {
        let cookie = self.inhibitor.next_cookie().await;
        let owner = header.sender().map(|s| OwnedUniqueName::from(s.to_owned()));
        log::info!("ScreenSaver.Inhibit {cookie} app={application_name:?} reason={reason_for_inhibit:?} from {owner:?}");
        self.inhibitor.grant(format!("screensaver-{cookie}"), application_name, owner).await;
        cookie
    }

    async fn un_inhibit(&self, cookie: u32) {
        self.inhibitor.release(&format!("screensaver-{cookie}")).await;
    }

    /// Activity, as if the user touched the input: resets both idle timers
    /// and wakes darkened displays. Older players call this on a timer
    /// instead of inhibiting.
    async fn simulate_user_activity(&self) {
        if let Err(e) = ctl(&Request::Idle(IdleRequest::Wake)).await {
            log::warn!("control socket: {e}");
        }
    }

    async fn lock(&self) {
        if let Err(e) = ctl(&Request::Lock).await {
            log::warn!("control socket: {e}");
        }
    }

    fn get_active(&self) -> bool {
        false
    }

    fn get_active_time(&self) -> u32 {
        0
    }

    fn get_session_idle_time(&self) -> u32 {
        0
    }

    #[zbus(signal)]
    async fn active_changed(emitter: &SignalEmitter<'_>, active: bool) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_is_one_line_of_words() {
        assert_eq!(sanitize_who("Steam"), "Steam");
        assert_eq!(sanitize_who("  Firefox \n video\tplayback "), "Firefox video playback");
        assert_eq!(sanitize_who("\n\t"), "unknown");
    }

    #[test]
    fn portal_tokens_are_one_word() {
        let t = portal_token("/org/freedesktop/portal/desktop/request/1_42/t0k3n");
        assert!(!t.contains(char::is_whitespace) && !t.contains('/'));
        assert_eq!(t, "portal-org-freedesktop-portal-desktop-request-1_42-t0k3n");
    }
}
