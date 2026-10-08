# CLAUDE.md

Read `../cce-compositor/WORKSPACE.md` first: this crate is one member of the
cce workspace and follows its multi-repo, `ccebuild` and concurrent-session
rules.

## What this is

`cce-desktop-portal` is cce's own backend for five portal interfaces that
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

- **`org.freedesktop.impl.portal.AppChooser`** (`src/app_chooser.rs`) — the
  "Open with…" dialog. The frontend passes the handler IDs; this runs
  `cce-cloud --choose -p "Open <file> with: " [-s <last_choice>]` with the IDs
  on stdin (cce-cloud's chooser mode shows each entry's name and icon and
  prints the picked ID — see its CLAUDE.md) and answers `{choice: <id>}`, or
  cancelled on Escape. The request's `Close` kills the cce-cloud client; the
  cce-cloud daemon closes a popup whose client hung up. `UpdateChoices` is
  only logged. `CCE_CHOOSER_BIN` points a run at another cce-cloud build.
- **`org.freedesktop.impl.portal.Notification`** (`src/notification.rs`) —
  portal notifications as cce-notifier cards: `AddNotification` becomes
  `Notify` on `org.freedesktop.Notifications` (desktop-entry name as
  app_name, `markup-body` stripped, the icon as a theme name or a file under
  `$XDG_RUNTIME_DIR/cce-desktop-portal/` for bytes/fd icons, `priority` →
  urgency, `default-action` as the `default` key and buttons as `b<i>`);
  the server's `ActionInvoked` comes back through those keys as the
  portal's `ActionInvoked(app_id, id, action, [target])`. Needs cce-notifier
  with actions and signals (its `feat: clickable cards…` commit) — before
  that, cards dropped every action.
- **`org.freedesktop.impl.portal.Print`** (`src/print.rs`) — the dialog is a
  `cce-cloud --json` panel: a destination list ("Save as PDF", then each
  CUPS queue from `lpstat -e`, default first) leading to one options page
  per destination (copies, all pages / from–to, two-sided where `lpoptions
  -l` shows a Duplex option), each ending in its own `print:<i>` button —
  the panel reports only the closing button and every control's value, so
  per-destination pages and `<i>.`-prefixed ids are what say which
  destination was chosen. `PreparePrint` answers GtkPrintSettings (ranges
  0-based) and a token; `Print` with that token sends the fd's document
  with `lp -d … -n … [-o sides=two-sided-long-edge]` (ranges are already
  rendered by the app), or for Save as PDF copies it to the path
  `cce-files --save` returned. A `Print` without a token asks first.
  `CCE_PRINT_DRY_RUN` logs the `lp` command instead of running it;
  `CCE_FILES_BIN` swaps the save dialog (a stub that echoes a path is how
  the save branch is tested — typing `/` in cce-files opens its location
  search, so a path cannot be typed into its name box). The CUPS branch
  was verified against a cups-pdf queue (`sudo lpadmin -p cce-test-pdf -E
  -v cups-pdf:/ -m CUPS-PDF_opt.ppd`; output in
  `/var/spool/cups-pdf/$USER/`): 2 copies came out as a 2-page PDF titled
  after the job. cups-pdf has no Duplex option, so its page shows no
  two-sided box.

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
  versioned) must say `org.freedesktop.impl.portal.Settings=cce-desktop;gtk`,
  `org.freedesktop.impl.portal.Inhibit=cce-desktop`,
  `org.freedesktop.impl.portal.AppChooser=cce-desktop` and
  `org.freedesktop.impl.portal.Notification=cce-desktop`.

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

The chooser can be driven end to end the same way, never on the live bus:
a private `dbus-run-session` running this backend with
`WAYLAND_DISPLAY=<shadow's>` and `CCE_CHOOSER_BIN=<tree cce-cloud>`, a
`gdbus call … AppChooser.ChooseApplication /test/r/1 app "" "['gimp',…]"
"{'filename': <'/x/a.pdf'>}"` in the background, then `cce-shadow ctl
keypress` (108 Down, 28 Enter, 1 Escape) and read gdbus's reply. cce-cloud's
client socket is keyed by `WAYLAND_DISPLAY`, so a shadow's chooser runs
standalone (or under a shadow daemon) and never reaches the live one.

Notifications the same way: the private bus also runs a tree
`cce-notifier` (it owns `org.freedesktop.Notifications` there, so the live
one is never asked), `gdbus monitor --dest` each name into a file, post
with `AddNotification`, and click the cards with `cce-shadow ctl
pointer-move-to` / `pointer-click [right]`. The notifier reads the real
config, so its bell may sound. When cleaning up, match the private bus by
its socket path in `/proc/<pid>/environ` — a `pkill -f` on that path also
matches the shell running it.
