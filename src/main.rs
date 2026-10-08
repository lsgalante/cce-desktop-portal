//! cce-desktop-portal — the cce desktop's own `Settings`, `Inhibit`,
//! `AppChooser` and `Notification` portal backends, and
//! `org.freedesktop.ScreenSaver`.
//!
//! Settings and Inhibit used to fall to xdg-desktop-portal-gtk (`default=gtk` in
//! `cce-portals.conf`): Settings published GNOME's GSettings rather than cce's
//! config, and Inhibit forwarded to session services no cce session runs, so
//! it silently did nothing. AppChooser ("Open with…") was a GTK window; it is
//! now cce-cloud's chooser mode. Notification forwarded to cce-notifier
//! through gtk, which kept the text and lost the actions; it now maps them
//! both ways. See `settings.rs`, `inhibit.rs`, `app_chooser.rs` and
//! `notification.rs`.
//!
//! One bus-activated process owns both names; whichever is asked for first
//! starts it. It lives for the session — Settings has to be there to signal
//! changes — and costs nothing at rest: config changes arrive by inotify,
//! and the lease-renewal loop sleeps while nothing is held.

mod app_chooser;
mod inhibit;
mod notification;
mod settings;

use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::RwLock;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::names::BusName;

const PORTAL_NAME: &str = "org.freedesktop.impl.portal.desktop.cce-desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENSAVER_NAME: &str = "org.freedesktop.ScreenSaver";

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(e) = run().await {
        log::error!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let appearance = Arc::new(RwLock::new(settings::load(&settings::config_path())));
    log::info!("appearance: {:?}", *appearance.read().await);
    let inhibitor = inhibit::Inhibitor::new();
    let cards = notification::SharedCards::default();

    // Leases a predecessor held die with it here rather than after their
    // ttl. An older compositor answers an error; external inhibitors then
    // wait for a newer one (see `Inhibitor::send_lease`).
    match inhibit::ctl("idle inhibit-clear").await {
        Ok(r) if r.starts_with("ok") => {}
        Ok(r) => log::warn!("compositor has no external inhibitors yet: {}", r.trim()),
        Err(e) => log::warn!("compositor control socket: {e}"),
    }

    let screensaver = |i: &inhibit::Inhibitor| inhibit::ScreenSaver { inhibitor: i.clone() };
    let conn = zbus::connection::Builder::session()?
        .serve_at(PORTAL_PATH, settings::SettingsPortal { state: appearance.clone() })?
        .serve_at(PORTAL_PATH, inhibit::InhibitPortal { inhibitor: inhibitor.clone() })?
        .serve_at(PORTAL_PATH, app_chooser::AppChooser::default())?
        .serve_at(PORTAL_PATH, notification::NotificationPortal { cards: cards.clone() })?
        // Both paths are in use in the wild (KDE answered at /ScreenSaver).
        .serve_at("/org/freedesktop/ScreenSaver", screensaver(&inhibitor))?
        .serve_at("/ScreenSaver", screensaver(&inhibitor))?
        .build()
        .await?;

    // Objects first, names second: a call can only arrive once a name is
    // owned, and by then every interface is there to answer it.
    let flags = RequestNameFlags::DoNotQueue.into();
    match conn.request_name_with_flags(PORTAL_NAME, flags).await? {
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {}
        // A racing activation won; it serves both names.
        _ => {
            log::info!("{PORTAL_NAME} is already owned; exiting");
            return Ok(());
        }
    }
    match conn.request_name_with_flags(SCREENSAVER_NAME, flags).await {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
        other => log::warn!("{SCREENSAVER_NAME} not taken ({other:?}); serving the portal only"),
    }
    log::info!("serving {PORTAL_NAME} and {SCREENSAVER_NAME}");

    tokio::spawn(inhibitor.clone().renew_loop());
    {
        let conn = conn.clone();
        tokio::spawn(async move {
            if let Err(e) = notification::relay(conn, PORTAL_PATH, cards).await {
                log::error!("notification relay stopped: {e}");
            }
        });
    }
    {
        let (conn, appearance) = (conn.clone(), appearance.clone());
        tokio::spawn(async move {
            if let Err(e) = settings::watch(conn, PORTAL_PATH, appearance).await {
                log::error!("appearance watch stopped: {e}");
            }
        });
    }

    // A ScreenSaver caller that leaves the bus without UnInhibit (a crash,
    // a kill) releases what it held.
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut owners = dbus.receive_name_owner_changed().await?;
    while let Some(signal) = owners.next().await {
        let Ok(args) = signal.args() else { continue };
        if args.new_owner().is_none() {
            if let BusName::Unique(name) = args.name() {
                inhibitor.owner_gone(name).await;
            }
        }
    }
    Ok(())
}
