//! `org.freedesktop.impl.portal.AppChooser`: the "Open with…" dialog, as a
//! cce popup instead of a GTK window.
//!
//! The frontend has already done the hard part — it hands over `choices`, the
//! desktop-file IDs that handle the content — and asks only which one. This
//! backend runs `cce-cloud --choose` (the launcher's chooser mode: rows are
//! each ID's name and icon, the answer is the ID) with the IDs on stdin, the
//! way the file chooser runs `cce-files --select`. The frontend launches the
//! app itself once it has the answer.
//!
//! The request object at `handle` lets the app (or the frontend, when the app
//! exits) take the question back: `Close` kills the cce-cloud client, and the
//! cce-cloud daemon closes a popup whose client hung up.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::sync::{oneshot, Mutex};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection, ObjectServer};

const RESPONSE_OK: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_OTHER: u32 = 2;

/// Longest a path or URI may run in the prompt before it is shortened.
const PROMPT_SUBJECT_MAX: usize = 48;

type Pending = Arc<Mutex<HashMap<OwnedObjectPath, oneshot::Sender<()>>>>;

#[derive(Default)]
pub struct AppChooser {
    pending: Pending,
}

fn str_opt(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    match options.get(key).map(|v| &**v) {
        Some(Value::Str(s)) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    }
}

/// The popup's prompt: what is being opened, as briefly as says it. cce-cloud
/// draws it straight before the query (or its placeholder), so it carries
/// its own separator.
pub fn prompt(filename: Option<&str>, uri: Option<&str>) -> String {
    let subject = filename
        .map(|f| f.rsplit('/').next().unwrap_or(f).to_string())
        .or_else(|| uri.map(str::to_string));
    match subject {
        Some(s) if s.chars().count() > PROMPT_SUBJECT_MAX => {
            let head: String = s.chars().take(PROMPT_SUBJECT_MAX - 1).collect();
            format!("Open {head}… with: ")
        }
        Some(s) => format!("Open {s} with: "),
        None => "Open with: ".to_string(),
    }
}

/// The chooser's answer as the portal response: an ID it was offered, or
/// cancelled (Escape answers an empty line; anything else was not a choice).
pub fn response(stdout: &str, choices: &[String]) -> (u32, Option<String>) {
    let pick = stdout.lines().next().unwrap_or("").trim();
    if !pick.is_empty() && choices.iter().any(|c| c == pick) {
        (RESPONSE_OK, Some(pick.to_string()))
    } else {
        (RESPONSE_CANCELLED, None)
    }
}

/// cce-cloud from the install location: the bus activates this backend with
/// the session's environment, whose PATH need not include ~/.local/bin.
/// `CCE_CHOOSER_BIN` points one run at another build, for testing.
fn cce_cloud() -> std::path::PathBuf {
    if let Some(bin) = std::env::var_os("CCE_CHOOSER_BIN").filter(|b| !b.is_empty()) {
        return bin.into();
    }
    let installed = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".local/bin/cce-cloud"))
        .filter(|p| p.is_file());
    installed.unwrap_or_else(|| "cce-cloud".into())
}

#[interface(name = "org.freedesktop.impl.portal.AppChooser")]
impl AppChooser {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    async fn choose_application(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        choices: Vec<String>,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let path: OwnedObjectPath = handle.clone().into();
        let (filename, uri) = (str_opt(&options, "filename"), str_opt(&options, "uri"));
        let last = str_opt(&options, "last_choice");
        log::info!("ChooseApplication {path} for {app_id:?}: {} choices, last {last:?}", choices.len());
        if choices.is_empty() {
            return (RESPONSE_CANCELLED, HashMap::new());
        }

        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.pending.lock().await.insert(path.clone(), cancel_tx);
        if let Err(e) = server.at(&path, ChooserRequest { path: path.clone(), pending: self.pending.clone() }).await {
            log::warn!("exporting request {path}: {e}");
        }

        let mut cmd = tokio::process::Command::new(cce_cloud());
        cmd.arg("--choose").arg("-p").arg(prompt(filename.as_deref(), uri.as_deref()));
        if let Some(last) = last.as_ref().filter(|l| choices.contains(l)) {
            cmd.arg("-s").arg(last);
        }
        // Killing the client on cancel is the whole cancel path: the daemon
        // closes a popup whose client hung up.
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true);

        let outcome = match cmd.spawn() {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    let feed = choices.join("\n") + "\n";
                    if let Err(e) = stdin.write_all(feed.as_bytes()).await {
                        log::warn!("feeding the chooser: {e}");
                    }
                    // Dropped here: EOF tells cce-cloud the list is complete.
                }
                tokio::select! {
                    out = child.wait_with_output() => Some(out),
                    _ = cancel_rx => None,
                }
            }
            Err(e) => Some(Err(e)),
        };

        self.pending.lock().await.remove(&path);
        let _ = server.remove::<ChooserRequest, _>(&path).await;

        match outcome {
            None => {
                log::info!("{path}: closed by the caller");
                (RESPONSE_CANCELLED, HashMap::new())
            }
            Some(Err(e)) => {
                log::error!("running {}: {e}", cce_cloud().display());
                (RESPONSE_OTHER, HashMap::new())
            }
            Some(Ok(out)) => match response(&String::from_utf8_lossy(&out.stdout), &choices) {
                (code, Some(choice)) => {
                    log::info!("{path}: chose {choice}");
                    (code, HashMap::from([("choice".to_string(), OwnedValue::from(zbus::zvariant::Str::from(choice)))]))
                }
                (code, None) => {
                    log::info!("{path}: dismissed");
                    (code, HashMap::new())
                }
            },
        }
    }

    /// The handler list changed while the popup is open (an app was
    /// installed). The open popup keeps the list it was given; the next
    /// request sees the new one.
    async fn update_choices(&self, handle: ObjectPath<'_>, choices: Vec<String>) {
        log::info!("UpdateChoices {handle}: {} choices (the open popup keeps its list)", choices.len());
    }
}

/// `org.freedesktop.impl.portal.Request` at the handle the frontend chose.
struct ChooserRequest {
    path: OwnedObjectPath,
    pending: Pending,
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl ChooserRequest {
    async fn close(&self, #[zbus(connection)] _conn: &Connection) {
        if let Some(cancel) = self.pending.lock().await.remove(&self.path) {
            let _ = cancel.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts() {
        assert_eq!(prompt(Some("/home/u/report.pdf"), None), "Open report.pdf with: ");
        assert_eq!(prompt(None, Some("https://example.org/a")), "Open https://example.org/a with: ");
        assert_eq!(prompt(None, None), "Open with: ");
        let long = format!("https://example.org/{}", "x".repeat(80));
        let p = prompt(None, Some(&long));
        assert!(p.ends_with("… with: ") && p.chars().count() < 70, "{p}");
    }

    #[test]
    fn only_an_offered_id_is_a_choice() {
        let choices = vec!["org.gnome.Evince".to_string(), "firefox".to_string()];
        assert_eq!(response("firefox\n", &choices), (RESPONSE_OK, Some("firefox".to_string())));
        assert_eq!(response("\n", &choices), (RESPONSE_CANCELLED, None), "Escape");
        assert_eq!(response("", &choices), (RESPONSE_CANCELLED, None), "killed");
        assert_eq!(response("something typed\n", &choices), (RESPONSE_CANCELLED, None));
    }
}
