# CLAUDE.md

Read `../cce-compositor/WORKSPACE.md` first: this crate is one member of the
cce workspace and follows its multi-repo, `ccebuild` and concurrent-session
rules.

## What this is

`cce-desktop-portal` is cce's own backend for two portal interfaces that
used to fall to xdg-desktop-portal-gtk through `default=gtk`, plus the
`org.freedesktop.ScreenSaver` service apps call directly:

- **`org.freedesktop.impl.portal.Settings`** (`src/settings.rs`) — the
  `org.freedesktop.appearance` namespace from cce's config instead of GNOME's
  GSettings: `color-scheme` from `style { color_scheme }` (absent = dark),
  `accent-color` from `style { highlight primary }`, `reduced-motion` from the
  animations switch `/run/cce/animations`, `contrast` always normal. Only that
  namespace: the frontend merges every backend `cce-portals.conf` lists for
  Settings (`cce-desktop;gtk`), first answer per key wins, and an unknown key
  must fail with `org.freedesktop.portal.Error.NotFound` so the frontend
  moves on to gtk for the `org.gnome.*` keys sandboxed GTK apps read.
  Changes are pushed by inotify on the two directories (not the files —
  config.kdl is replaced by rename), and only keys whose value moved signal.
- **`org.freedesktop.impl.portal.Inhibit`** and **`org.freedesktop.ScreenSaver`**
  (`src/inhibit.rs`) — keeping the display awake for apps that ask over
  D-Bus. gtk's Inhibit forwarded to `org.gnome.SessionManager` /
  `org.freedesktop.ScreenSaver`, neither of which a cce session runs, so every
  request was accepted and did nothing. Each holder here becomes a **lease**
  in the compositor: `idle inhibit <token> <ttl_s> <who>` on the control
  socket (`src/server/idle.rs` in cce-compositor is the other half). Leases
  lapse unless renewed, so the backend renews every live one every 20 s
  against a 60 s ttl; if it dies, the compositor drops them within a minute.
  A hold ends when the portal request is closed (the frontend does that when
  the app exits), on `UnInhibit`, or — ScreenSaver — when the caller's bus
  connection goes away. Only the portal's idle (8) and suspend (4) flags
  hold anything; logout and user-switch have nothing to hold in cce.

`idle status` names holders as `portal:<who>`.

## Lifecycle

One process, bus-activated by either name (two files in `dbus/`); whichever
is asked for first starts it and it claims both. A racing second activation
finds the portal name taken and exits. It begins with `idle inhibit-clear`
(a predecessor's leases die now rather than at their ttl; an older
compositor answers an error, and leases then wait for a newer one — they
are still tracked and sent on every renew). It lives for the session:
Settings must be there to signal changes. At rest it does nothing — inotify
for the config, and the renew loop sleeps while nothing is held.

Reads `WAYLAND_DISPLAY` for the control socket path exactly as `ccectl`
does, so the bus activation environment must carry it (startcce imports it).

## Wiring

- `portals/cce-desktop.portal` → `$XDG_DATA_HOME/xdg-desktop-portal/portals/`
- `dbus/*.service` → `~/.local/share/dbus-1/services/` (absolute `Exec`)
- **`~/.config/xdg-desktop-portal/cce-portals.conf`** (user config, not
  versioned) must say `org.freedesktop.impl.portal.Settings=cce-desktop;gtk`
  and `org.freedesktop.impl.portal.Inhibit=cce-desktop`.

The frontend reads portal files and the conf at startup. Restarting it
(`systemctl --user restart xdg-desktop-portal`) drops every app's portal
sessions — 1Password's global shortcut among them, until it is restarted —
so prefer letting the next login pick changes up.

## Verifying

The session bus is shared with the live session (a shadow has none of its
own), so check first that `org.freedesktop.ScreenSaver` and the portal name
are unowned, run the backend through `cce-shadow run` so its control socket
is the shadow's, and kill it after. Leases can be driven directly with
`cce-shadow ctl idle inhibit t1 2 tester` (watch it lapse in `idle status`).
A ScreenSaver holder that must outlive one call needs a client that keeps
its connection (`busctl call` exits at once and its hold is released with
it — correctly). To test the frontend's Settings merge without restarting
the live one, run a private `dbus-run-session` with this backend,
`/usr/lib/xdg-desktop-portal-gtk` and `xdg-desktop-portal -r` under an
`XDG_CONFIG_HOME` holding a test `cce-portals.conf`, and kill everything
the private bus activated afterwards (cce-shortcuts-portal attaches to the
shadow and keeps `dbus-run-session` alive).
