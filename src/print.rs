//! `org.freedesktop.impl.portal.Print`: the print dialog as a cce popup, and
//! the job sent to CUPS with `lp`.
//!
//! The portal splits printing in two. `PreparePrint` asks the user how to
//! print before the app has rendered anything (so the page count is not
//! known yet) and answers GtkPrintSettings plus a token; the app renders
//! with those settings and calls `Print` with the token and an fd of the
//! result (a PDF). A `Print` without a known token asks first.
//!
//! The dialog is a `cce-cloud --json` panel: page 0 lists the destinations —
//! "Save as PDF", then each CUPS queue (`lpstat -e`, default first) — and
//! each opens a page of its own options (copies, pages, two-sided where the
//! queue has a Duplex option) ending in its own Print / Save button. The
//! panel reports only the button that closed it plus every control's value,
//! so a per-destination page is what says which destination was chosen
//! (`print:<i>`), and its controls carry the same prefix (`<i>.copies`).
//!
//! Page ranges are rendered by the app (GtkPrintOperation renders only the
//! pages the settings name), so the job is sent without them; copies and
//! two-sided are the printer's, so `lp -n` and `-o sides=` carry them.
//! "Save as PDF" asks for a path with `cce-files --save` and copies the
//! rendered file there.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use zbus::interface;
use zbus::zvariant::{OwnedFd, OwnedValue, Str};

const RESPONSE_OK: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_OTHER: u32 = 2;

/// GtkPrintSettings' name for the print-to-file destination.
pub const TO_FILE: &str = "Print to File";

/// A place the dialog can send the job.
#[derive(Debug, Clone, PartialEq)]
pub struct Destination {
    /// The CUPS queue, or [`TO_FILE`].
    pub name: String,
    pub label: String,
    pub duplex: bool,
}

/// What the dialog decided, as the app gets it back and `Print` reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub printer: String,
    pub copies: u32,
    /// 1-based, inclusive; `None` for all pages.
    pub range: Option<(u32, u32)>,
    pub duplex: bool,
    /// Print to File: where to.
    pub output: Option<String>,
}

impl Choice {
    /// As GtkPrintSettings: string values throughout, page ranges 0-based.
    pub fn settings(&self) -> HashMap<String, OwnedValue> {
        let mut s: Vec<(&str, String)> = vec![
            ("printer", self.printer.clone()),
            ("n-copies", self.copies.to_string()),
            ("duplex", if self.duplex { "horizontal" } else { "simplex" }.to_string()),
        ];
        match self.range {
            Some((from, to)) => {
                s.push(("print-pages", "ranges".into()));
                s.push(("page-ranges", format!("{}-{}", from.saturating_sub(1), to.saturating_sub(1))));
            }
            None => s.push(("print-pages", "all".into())),
        }
        if let Some(path) = &self.output {
            s.push(("output-file-format", "pdf".into()));
            s.push(("output-uri", format!("file://{path}")));
        }
        s.into_iter().map(|(k, v)| (k.to_string(), OwnedValue::from(Str::from(v)))).collect()
    }

    /// The `lp` argv for a rendered file at `path`.
    pub fn lp_args(&self, title: &str, path: &str) -> Vec<String> {
        let mut a = vec!["-d".into(), self.printer.clone(), "-n".into(), self.copies.max(1).to_string()];
        if !title.is_empty() {
            a.extend(["-t".into(), title.to_string()]);
        }
        if self.duplex {
            a.extend(["-o".into(), "sides=two-sided-long-edge".into()]);
        }
        a.extend(["--".into(), path.to_string()]);
        a
    }
}

// ── destinations ───────────────────────────────────────────────────────────

/// `lpstat -e` (one queue per line) with `lpstat -d`'s default first.
pub fn order_queues(queues: &str, default_line: &str) -> Vec<String> {
    let default = default_line.rsplit(':').next().map(str::trim).filter(|d| !d.is_empty() && !d.contains(' '));
    let mut names: Vec<String> = queues.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
    if let Some(d) = default {
        if let Some(i) = names.iter().position(|n| n == d) {
            let dflt = names.remove(i);
            names.insert(0, dflt);
        }
    }
    names
}

async fn run(cmd: &str, args: &[&str]) -> String {
    match tokio::process::Command::new(cmd).args(args).output().await {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// Every place a job can go: Save as PDF, then the queues.
async fn destinations() -> Vec<Destination> {
    let mut out = vec![Destination { name: TO_FILE.into(), label: "Save as PDF".into(), duplex: false }];
    let queues = order_queues(&run("lpstat", &["-e"]).await, &run("lpstat", &["-d"]).await);
    for q in queues {
        // `lpoptions -l` lists the queue's PPD options; Duplex is the one
        // two-sided printing needs.
        let opts = run("lpoptions", &["-p", &q, "-l"]).await;
        let duplex = opts.lines().any(|l| l.starts_with("Duplex/") || l.starts_with("Duplex:"));
        out.push(Destination { label: q.clone(), name: q, duplex });
    }
    out
}

// ── the dialog ─────────────────────────────────────────────────────────────

/// The `cce-cloud --json` panel for `dests`: a destination list leading to
/// one options page each. With a single destination the list is skipped.
pub fn dialog_layout(title: &str, dests: &[Destination]) -> serde_json::Value {
    let heading = if title.is_empty() { "Print".to_string() } else { format!("Print “{title}”") };
    let single = dests.len() == 1;
    let mut pages = Vec::new();
    if !single {
        let mut widgets = vec![json!({"type": "label", "text": heading})];
        for (i, d) in dests.iter().enumerate() {
            widgets.push(json!({"type": "button", "text": d.label, "id": format!("go:{i}"), "target_page": i + 1}));
        }
        pages.push(json!({"title": "Print", "widgets": widgets}));
    }
    for (i, d) in dests.iter().enumerate() {
        let mut widgets = Vec::new();
        if !single {
            widgets.push(json!({"type": "button", "text": "Back", "id": format!("back:{i}"), "target_page": 0}));
        }
        widgets.push(json!({"type": "label", "text": if single { heading.clone() } else { d.label.clone() }}));
        let to_file = d.name == TO_FILE;
        if !to_file {
            widgets.push(json!({"type": "spinbox", "text": "Copies", "id": format!("{i}.copies"), "value": 1, "min": 1, "max": 99, "step": 1}));
        }
        widgets.push(json!({"type": "checkbox", "text": "All pages", "id": format!("{i}.all"), "checked": true}));
        widgets.push(json!({"type": "spinbox", "text": "From page", "id": format!("{i}.from"), "value": 1, "min": 1, "max": 9999, "step": 1}));
        widgets.push(json!({"type": "spinbox", "text": "To page", "id": format!("{i}.to"), "value": 1, "min": 1, "max": 9999, "step": 1}));
        if d.duplex {
            widgets.push(json!({"type": "checkbox", "text": "Two-sided", "id": format!("{i}.duplex"), "checked": false}));
        }
        let go = if to_file { "Save…" } else { "Print" };
        widgets.push(json!({"type": "button", "text": go, "id": format!("print:{i}")}));
        pages.push(json!({"title": d.label, "widgets": widgets}));
    }
    json!({"pages": pages})
}

/// The panel's answer as a [`Choice`] (without a save path yet), or `None`
/// when it was dismissed or closed by something other than a Print button.
pub fn parse_answer(stdout: &str, dests: &[Destination]) -> Option<Choice> {
    let v: serde_json::Value = serde_json::from_str(stdout.lines().next()?.trim()).ok()?;
    let i: usize = v.get("button")?.as_str()?.strip_prefix("print:")?.parse().ok()?;
    let dest = dests.get(i)?;
    let num = |k: &str| v.get("spinboxes").and_then(|s| s.get(format!("{i}.{k}"))).and_then(|n| n.as_f64()).map(|n| n.max(1.0) as u32);
    let flag = |k: &str| v.get("checkboxes").and_then(|s| s.get(format!("{i}.{k}"))).and_then(|b| b.as_bool());
    let range = match flag("all") {
        Some(false) => {
            let (a, b) = (num("from").unwrap_or(1), num("to").unwrap_or(1));
            Some((a.min(b), a.max(b)))
        }
        _ => None,
    };
    Some(Choice {
        printer: dest.name.clone(),
        copies: num("copies").unwrap_or(1),
        range,
        duplex: dest.duplex && flag("duplex").unwrap_or(false),
        output: None,
    })
}

fn tool(env: &str, name: &str) -> std::path::PathBuf {
    if let Some(bin) = std::env::var_os(env).filter(|b| !b.is_empty()) {
        return bin.into();
    }
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".local/bin").join(name))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| name.into())
}

/// Run `cmd` with `input` on stdin; its stdout, or `None` if it would not run.
async fn ask(cmd: std::path::PathBuf, args: &[&str], input: Option<String>) -> Option<String> {
    let mut c = tokio::process::Command::new(&cmd);
    c.args(args).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true);
    c.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = c.spawn().map_err(|e| log::error!("running {}: {e}", cmd.display())).ok()?;
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        let _ = stdin.write_all(text.as_bytes()).await;
    }
    let out = child.wait_with_output().await.ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A save path ends in `.pdf`.
pub fn pdf_path(path: &str) -> String {
    if path.to_ascii_lowercase().ends_with(".pdf") { path.to_string() } else { format!("{path}.pdf") }
}

/// Show the dialog; `None` when the user backed out anywhere.
async fn choose(title: &str) -> Option<Choice> {
    let dests = destinations().await;
    let layout = dialog_layout(title, &dests);
    let answer = ask(tool("CCE_CHOOSER_BIN", "cce-cloud"), &["--json"], Some(layout.to_string())).await?;
    let mut choice = parse_answer(&answer, &dests)?;
    if choice.printer == TO_FILE {
        let docs = std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Documents"));
        let start = docs.filter(|d| d.is_dir()).map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
        let mut args = vec!["--save"];
        if !start.is_empty() {
            args.push(&start);
        }
        let path = ask(tool("CCE_FILES_BIN", "cce-files"), &args, None).await?;
        let path = path.lines().next().unwrap_or("").trim().to_string();
        if path.is_empty() {
            return None;
        }
        choice.output = Some(pdf_path(&path));
    }
    Some(choice)
}

// ── the portal ─────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct PrintPortal {
    prepared: Arc<Mutex<(u32, HashMap<u32, Choice>)>>,
}

fn results(choice: &Choice, page_setup: HashMap<String, OwnedValue>, token: Option<u32>) -> HashMap<String, OwnedValue> {
    let mut r = HashMap::from([
        ("settings".to_string(), OwnedValue::try_from(zbus::zvariant::Value::from(choice.settings())).expect("no fds")),
        ("page-setup".to_string(), OwnedValue::try_from(zbus::zvariant::Value::from(page_setup)).expect("no fds")),
    ]);
    if let Some(t) = token {
        r.insert("token".to_string(), OwnedValue::from(t));
    }
    r
}

#[interface(name = "org.freedesktop.impl.portal.Print")]
impl PrintPortal {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_print(
        &self,
        _handle: zbus::zvariant::ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        title: String,
        _settings: HashMap<String, OwnedValue>,
        page_setup: HashMap<String, OwnedValue>,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        log::info!("PreparePrint {title:?} for {app_id:?}");
        let Some(choice) = choose(&title).await else {
            log::info!("PreparePrint {title:?}: dismissed");
            return (RESPONSE_CANCELLED, HashMap::new());
        };
        let token = {
            let mut p = self.prepared.lock().await;
            p.0 = p.0.wrapping_add(1).max(1);
            let t = p.0;
            p.1.insert(t, choice.clone());
            t
        };
        log::info!("PreparePrint {title:?}: {choice:?} as token {token}");
        (RESPONSE_OK, results(&choice, page_setup, Some(token)))
    }

    #[allow(clippy::too_many_arguments)]
    async fn print(
        &self,
        _handle: zbus::zvariant::ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        title: String,
        fd: OwnedFd,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let token = options.get("token").and_then(|v| u32::try_from(v).ok());
        let prepared = match token {
            Some(t) => self.prepared.lock().await.1.remove(&t),
            None => None,
        };
        let choice = match prepared {
            Some(c) => c,
            None => match choose(&title).await {
                Some(c) => c,
                None => return (RESPONSE_CANCELLED, HashMap::new()),
            },
        };
        log::info!("Print {title:?} for {app_id:?}: {choice:?}");
        match send(&choice, &title, fd).await {
            Ok(()) => (RESPONSE_OK, HashMap::new()),
            Err(e) => {
                log::error!("Print {title:?}: {e}");
                (RESPONSE_OTHER, HashMap::new())
            }
        }
    }
}

/// Copy the rendered document out of `fd` and deliver it.
async fn send(choice: &Choice, title: &str, fd: OwnedFd) -> Result<(), String> {
    use std::io::Read;
    let mut data = Vec::new();
    std::fs::File::from(std::os::fd::OwnedFd::from(fd))
        .read_to_end(&mut data)
        .map_err(|e| format!("reading the document: {e}"))?;
    if let Some(path) = &choice.output {
        std::fs::write(path, &data).map_err(|e| format!("writing {path}: {e}"))?;
        log::info!("saved {} bytes to {path}", data.len());
        return Ok(());
    }
    let dir = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("no XDG_RUNTIME_DIR")?).join("cce-desktop-portal");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ext = if data.starts_with(b"%!PS") { "ps" } else { "pdf" };
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let spool = dir.join(format!("print-{}-{n}.{ext}", std::process::id()));
    std::fs::write(&spool, &data).map_err(|e| e.to_string())?;
    let args = choice.lp_args(title, &spool.to_string_lossy());
    if std::env::var_os("CCE_PRINT_DRY_RUN").is_some() {
        log::info!("dry run: lp {}", args.join(" "));
        let _ = std::fs::remove_file(&spool);
        return Ok(());
    }
    let out = tokio::process::Command::new("lp").args(&args).output().await.map_err(|e| format!("lp: {e}"))?;
    // lp has the job once it answers; the spool copy is ours to drop.
    let _ = std::fs::remove_file(&spool);
    if out.status.success() {
        log::info!("{}", String::from_utf8_lossy(&out.stdout).trim());
        Ok(())
    } else {
        Err(format!("lp: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dests() -> Vec<Destination> {
        vec![
            Destination { name: TO_FILE.into(), label: "Save as PDF".into(), duplex: false },
            Destination { name: "laser".into(), label: "laser".into(), duplex: true },
        ]
    }

    #[test]
    fn default_queue_first() {
        assert_eq!(order_queues("inkjet\nlaser\n", "system default destination: laser"), ["laser", "inkjet"]);
        assert_eq!(order_queues("a\nb\n", "no system default destination"), ["a", "b"]);
        assert!(order_queues("", "").is_empty());
    }

    #[test]
    fn the_print_button_names_its_destination_and_controls() {
        let out = r#"{"button":"print:1","checkboxes":{"0.all":true,"1.all":false,"1.duplex":true},"spinboxes":{"1.copies":2,"1.from":5,"1.to":3},"colors":{},"sliders":{}}"#;
        let c = parse_answer(out, &dests()).unwrap();
        assert_eq!(c, Choice { printer: "laser".into(), copies: 2, range: Some((3, 5)), duplex: true, output: None });
        assert_eq!(c.lp_args("Doc", "/run/x.pdf"), ["-d", "laser", "-n", "2", "-t", "Doc", "-o", "sides=two-sided-long-edge", "--", "/run/x.pdf"]);
        let s = c.settings();
        let get = |k: &str| match s.get(k).map(|v| &**v) {
            Some(zbus::zvariant::Value::Str(v)) => v.to_string(),
            _ => panic!("{k} missing"),
        };
        assert_eq!(get("page-ranges"), "2-4", "GtkPrintSettings ranges are 0-based");
        assert_eq!(get("print-pages"), "ranges");
        assert_eq!(get("n-copies"), "2");
    }

    #[test]
    fn dismissal_and_page_turns_are_not_answers() {
        assert_eq!(parse_answer("\n", &dests()), None);
        assert_eq!(parse_answer(r#"{"button":"go:1"}"#, &dests()), None);
        assert_eq!(parse_answer(r#"{"button":"print:9"}"#, &dests()), None);
    }

    #[test]
    fn layout_skips_the_list_for_one_destination() {
        let one = dialog_layout("Doc", &dests()[..1]);
        assert_eq!(one["pages"].as_array().unwrap().len(), 1);
        let two = dialog_layout("Doc", &dests());
        let pages = two["pages"].as_array().unwrap();
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0]["widgets"][2]["target_page"], 2);
        let ids: Vec<&str> = pages[2]["widgets"].as_array().unwrap().iter().filter_map(|w| w["id"].as_str()).collect();
        assert!(ids.contains(&"1.duplex") && ids.contains(&"print:1"));
        let pdf_ids: Vec<&str> = pages[1]["widgets"].as_array().unwrap().iter().filter_map(|w| w["id"].as_str()).collect();
        assert!(!pdf_ids.contains(&"0.copies"), "a file has no copies");
    }

    #[test]
    fn save_paths_end_in_pdf() {
        assert_eq!(pdf_path("/home/u/a"), "/home/u/a.pdf");
        assert_eq!(pdf_path("/home/u/a.PDF"), "/home/u/a.PDF");
    }
}
