//! Update check for this personal build ("TS Word"): the About dialog and
//! Help ▸ Check for Updates ask GitHub for the newest release of the fork's repo
//! (`tonynehihi/tsw-by-tonynehihi`, public — no sign-in needed). On macOS the flow is:
//! download the release's source archive, then **build it inside the app** with
//! `packaging/macos/package.sh` while the UI shows live progress (the running version
//! stays untouched, and a failed build changes nothing). Only when the new app is
//! built does a small shell swapper (embedded below) take over: it waits for the app
//! to quit, moves the old app out of /Applications into a backup folder, installs
//! the built one and relaunches — a few seconds, during which the old version can
//! no longer be launched. The work runs on worker threads, which hold an
//! [`egui::Context`] and wake the UI with `request_repaint` when they post a
//! message; the UI only polls the channel, so the UI thread never waits. Failures
//! land in [`Phase::Failed`] as readable text — nothing here panics.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use crate::WordApp;
use crate::theme::Tokens;

/// GitHub API: the newest published release of the fork.
pub const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/tonynehihi/tsw-by-tonynehihi/releases/latest";

/// Largest release JSON accepted (release notes included).
const MAX_JSON_BYTES: u64 = 1 << 20;
/// Largest source archive accepted (the repo snapshot is ~14 MB today).
const MAX_ZIP_BYTES: u64 = 300 << 20;

/// This build's version (the ui crate carries the workspace version).
fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// One published release, as the dialog shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// Version without the tag's leading "v" (e.g. "1.1.0").
    pub version: String,
    pub name: String,
    pub notes: String,
    /// Release page (opened in the browser on platforms without auto-install).
    pub page_url: String,
    /// Source archive of the release tag (zipball).
    pub zip_url: String,
}

/// Where the update flow stands.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Idle,
    Checking,
    UpToDate,
    Available(ReleaseInfo),
    Downloading {
        info: ReleaseInfo,
        received: u64,
        total: Option<u64>,
    },
    Downloaded {
        info: ReleaseInfo,
        zip: PathBuf,
    },
    /// Building the downloaded source inside the app: `step` is the latest
    /// human-readable build step, `compiled` counts finished crate builds.
    Building {
        info: ReleaseInfo,
        zip: PathBuf,
        step: String,
        compiled: u32,
    },
    /// The new app is built and staged; only the seconds-long swap remains.
    ReadyToInstall {
        info: ReleaseInfo,
        app: PathBuf,
        work: PathBuf,
    },
    Failed(String),
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // build messages are only sent by the macOS build worker
enum JobMsg {
    Check(Result<Option<ReleaseInfo>, String>),
    Progress(u64, Option<u64>),
    Download(Result<PathBuf, String>),
    BuildStep {
        step: String,
        compiled: u32,
    },
    /// Build finished: (staged app bundle, build work folder).
    Built(Result<(PathBuf, PathBuf), String>),
    BuildCancelled,
}

/// Update state owned by the app; jobs report back over a channel drained by [`UpdateState::poll`].
pub struct UpdateState {
    pub phase: Phase,
    rx: Option<Receiver<JobMsg>>,
    cancel: Arc<AtomicBool>,
    /// The launch-time prompt is open (only ever set when a startup check found a
    /// newer release; see [`UpdateState::start_check_at_startup`]).
    pub prompt: bool,
    /// Version the user already waved away with "Later" this run: don't prompt again for it.
    dismissed: Option<String>,
    /// A check started at launch still owes a prompt decision when it lands.
    startup_armed: bool,
    /// When the in-app build started (drives the elapsed-time display).
    build_started: Option<std::time::Instant>,
}

impl Default for UpdateState {
    fn default() -> Self {
        UpdateState {
            phase: Phase::Idle,
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            prompt: false,
            dismissed: None,
            startup_armed: false,
            build_started: None,
        }
    }
}

impl UpdateState {
    fn busy(&self) -> bool {
        self.rx.is_some()
    }

    /// Ask GitHub for the newest release (no-op while another job runs).
    pub fn start_check(&mut self, ctx: &egui::Context) {
        self.begin_check(false, ctx);
    }

    /// The launch-time check: if it finds a newer release, the prompt opens
    /// (see [`prompt_window`]); up-to-date and failure stay silent.
    pub fn start_check_at_startup(&mut self, ctx: &egui::Context) {
        self.begin_check(true, ctx);
    }

    fn begin_check(&mut self, startup: bool, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = startup;
            self.phase = Phase::Failed("Update checks are not available in the web build.".to_string());
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if startup {
                self.startup_armed = true;
            }
            self.cancel.store(false, Ordering::SeqCst);
            let (tx, rx) = std::sync::mpsc::channel();
            self.rx = Some(rx);
            self.phase = Phase::Checking;
            let cancel = self.cancel.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(JobMsg::Check(check_latest(&cancel)));
                ctx.request_repaint();
            });
        }
    }

    /// Close the launch prompt; `remember` also stores the version on offer so the
    /// prompt does not reopen for it during this run.
    pub fn dismiss_prompt(&mut self, remember: bool) {
        self.prompt = false;
        if !remember {
            return;
        }
        let version = match &self.phase {
            Phase::Available(info) => Some(info.version.clone()),
            Phase::Downloaded { info, .. } => Some(info.version.clone()),
            Phase::Building { info, .. } => Some(info.version.clone()),
            Phase::ReadyToInstall { info, .. } => Some(info.version.clone()),
            _ => None,
        };
        if let Some(v) = version {
            self.dismissed = Some(v);
        }
    }

    /// Download the source archive of the release in [`Phase::Available`] (macOS auto-install path).
    pub fn start_download(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = ctx;
            self.phase = Phase::Failed("Update downloads are not available in the web build.".to_string());
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Phase::Available(info) = self.phase.clone() else { return };
            let Some(dir) = update_dir() else {
                self.phase = Phase::Failed("Could not find your home folder to hold the download.".to_string());
                return;
            };
            if let Err(e) = std::fs::create_dir_all(&dir) {
                self.phase = Phase::Failed(format!("Could not create the update folder: {e}"));
                return;
            }
            let dest = dir.join(format!("tsw-by-tonynehihi-{}-source.zip", info.version));
            self.cancel.store(false, Ordering::SeqCst);
            let (tx, rx) = std::sync::mpsc::channel();
            self.rx = Some(rx);
            self.phase = Phase::Downloading { info: info.clone(), received: 0, total: None };
            let cancel = self.cancel.clone();
            let url = info.zip_url.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(JobMsg::Download(download_zip(&url, &dest, &cancel, &tx, &ctx).map(|()| dest)));
                ctx.request_repaint();
            });
        }
    }

    /// Build the downloaded source inside the app (macOS auto-install path): the
    /// worker extracts the archive and runs the packaging script, reporting its
    /// steps back over the channel. The running version is never touched — a
    /// failed or cancelled build changes nothing.
    pub fn start_build(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        let Phase::Downloaded { info, zip } = self.phase.clone() else { return };
        if !cfg!(target_os = "macos") {
            self.phase = Phase::Failed("Automatic install is only available on macOS.".to_string());
            return;
        }
        let Some(dir) = update_dir() else {
            self.phase = Phase::Failed("Could not find your home folder to hold the build.".to_string());
            return;
        };
        let work = dir.join(format!("build-{}", info.version));
        if work.exists() {
            let _ = std::fs::remove_dir_all(&work);
        }
        if let Err(e) = std::fs::create_dir_all(&work) {
            self.phase = Phase::Failed(format!("Could not create the build folder: {e}"));
            return;
        }
        self.cancel.store(false, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel();
        self.rx = Some(rx);
        self.build_started = Some(std::time::Instant::now());
        self.phase = Phase::Building { info: info.clone(), zip: zip.clone(), step: "Extracting the update...".to_string(), compiled: 0 };
        let cancel = self.cancel.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let msg = match run_build(&zip, &work, &cancel, &tx, &ctx) {
                BuildOutcome::Built(app) => JobMsg::Built(Ok((app, work.clone()))),
                BuildOutcome::Failed(e) => JobMsg::Built(Err(e)),
                BuildOutcome::Cancelled => JobMsg::BuildCancelled,
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Ask the running build to stop (the worker kills the packaging process).
    pub fn cancel_build(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// How long the in-app build has been running, while one is.
    pub fn build_elapsed(&self) -> Option<Duration> {
        self.build_started.map(|t| t.elapsed())
    }

    /// Drain job messages into the phase. Returns true while a job is still running.
    pub fn poll(&mut self) -> bool {
        let mut finished = false;
        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(JobMsg::Check(res)) => {
                        self.phase = match res {
                            Ok(Some(info)) => {
                                if prompt_wanted(self.startup_armed, self.dismissed.as_deref(), &info.version) {
                                    self.prompt = true;
                                }
                                self.startup_armed = false;
                                Phase::Available(info)
                            }
                            Ok(None) => {
                                self.startup_armed = false;
                                Phase::UpToDate
                            }
                            Err(e) => {
                                self.startup_armed = false;
                                Phase::Failed(e)
                            }
                        };
                        finished = true;
                    }
                    Ok(JobMsg::Progress(received, total)) => {
                        if let Phase::Downloading { received: r, total: t, .. } = &mut self.phase {
                            *r = received;
                            *t = total;
                        }
                    }
                    Ok(JobMsg::Download(res)) => {
                        let info = match &self.phase {
                            Phase::Downloading { info, .. } => Some(info.clone()),
                            _ => None,
                        };
                        self.phase = match (res, info) {
                            (Ok(zip), Some(info)) => Phase::Downloaded { info, zip },
                            (Err(e), _) => Phase::Failed(e),
                            (Ok(_), None) => Phase::Failed("The download finished in an unexpected state.".to_string()),
                        };
                        finished = true;
                    }
                    Ok(JobMsg::BuildStep { step, compiled }) => {
                        if let Phase::Building { step: s, compiled: c, .. } = &mut self.phase {
                            *s = step;
                            *c = compiled;
                        }
                    }
                    Ok(JobMsg::Built(res)) => {
                        let info = match &self.phase {
                            Phase::Building { info, .. } => Some(info.clone()),
                            _ => None,
                        };
                        self.build_started = None;
                        self.phase = match (res, info) {
                            (Ok((app, work)), Some(info)) => Phase::ReadyToInstall { info, app, work },
                            (Err(e), _) => Phase::Failed(e),
                            (Ok(_), None) => Phase::Failed("The build finished in an unexpected state.".to_string()),
                        };
                        finished = true;
                    }
                    Ok(JobMsg::BuildCancelled) => {
                        let back = match &self.phase {
                            Phase::Building { info, zip, .. } => Some(Phase::Downloaded { info: info.clone(), zip: zip.clone() }),
                            _ => None,
                        };
                        self.build_started = None;
                        if let Some(p) = back {
                            self.phase = p;
                        }
                        finished = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.phase = match &self.phase {
                            Phase::Checking => Phase::Failed("The update check stopped unexpectedly.".to_string()),
                            Phase::Downloading { .. } => Phase::Failed("The download stopped unexpectedly.".to_string()),
                            Phase::Building { .. } => {
                                Phase::Failed("The build stopped unexpectedly. Your current version was not changed.".to_string())
                            }
                            other => other.clone(),
                        };
                        self.build_started = None;
                        finished = true;
                        break;
                    }
                }
            }
        }
        if finished {
            self.rx = None;
        }
        self.busy()
    }

    /// Write the swap script next to the downloads and launch it detached; it waits
    /// for this app to exit, then swaps the built app in (seconds) and relaunches.
    /// macOS only.
    pub fn finish_install(&self) -> Result<(), String> {
        let Phase::ReadyToInstall { info, app, work } = &self.phase else {
            return Err("There is no built update to install.".to_string());
        };
        if !cfg!(target_os = "macos") {
            return Err("Automatic install is only available on macOS.".to_string());
        }
        let dir = update_dir().ok_or_else(|| "Could not find your home folder.".to_string())?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("Could not create the update folder: {e}"))?;
        let script = dir.join("install-update.sh");
        std::fs::write(&script, SWAP_SCRIPT).map_err(|e| format!("Could not write the installer script: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("Could not make the installer script runnable: {e}"))?;
        }
        spawn_swapper(&script, app, &info.version, work)
    }
}

/// Launch the swapper detached from this app (it must survive the app quitting).
#[cfg(unix)]
fn spawn_swapper(script: &std::path::Path, app: &std::path::Path, version: &str, work: &std::path::Path) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    std::process::Command::new("/bin/sh")
        .arg(script)
        .arg(app)
        .arg(version)
        .arg(work)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("Could not start the installer: {e}"))?;
    Ok(())
}

/// Non-Unix builds never reach here (install stops earlier); this only satisfies the compiler.
#[cfg(not(unix))]
fn spawn_swapper(_script: &std::path::Path, _app: &std::path::Path, _version: &str, _work: &std::path::Path) -> Result<(), String> {
    Err("Automatic install is only available on macOS.".to_string())
}

/// Folder holding downloaded update archives and the updater script:
/// `~/Library/Caches/TS Word/updates` on macOS, `~/.cache/...` elsewhere.
fn update_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    if cfg!(target_os = "macos") {
        p.push("Library");
        p.push("Caches");
    } else {
        p.push(".cache");
    }
    p.push("TS Word");
    p.push("updates");
    Some(p)
}

/// The launch prompt opens only for a startup check that found a version the
/// user has not already waved away with "Later" during this run.
fn prompt_wanted(armed: bool, dismissed: Option<&str>, version: &str) -> bool {
    armed && dismissed != Some(version)
}

/// Split a version into numeric parts ("1.10.2" → [1, 10, 2]); non-numeric tails count as 0.
pub fn version_parts(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| {
            let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().unwrap_or(0)
        })
        .collect()
}

/// True when `remote` is a strictly newer version than `current`.
pub fn is_newer(remote: &str, current: &str) -> bool {
    let (r, c) = (version_parts(remote), version_parts(current));
    for i in 0..r.len().max(c.len()) {
        let (a, b) = (r.get(i).copied().unwrap_or(0), c.get(i).copied().unwrap_or(0));
        if a != b {
            return a > b;
        }
    }
    false
}

/// Parse the GitHub "latest release" JSON into a [`ReleaseInfo`].
pub fn parse_release(json_text: &str) -> Result<ReleaseInfo, String> {
    let v: serde_json::Value = serde_json::from_str(json_text).map_err(|e| format!("GitHub sent release data that could not be read: {e}"))?;
    let field = |key: &str| v.get(key).and_then(serde_json::Value::as_str);
    let tag = field("tag_name").ok_or_else(|| "GitHub's release data has no version tag.".to_string())?;
    let zip_url = field("zipball_url").ok_or_else(|| "GitHub's release data has no source archive.".to_string())?;
    Ok(ReleaseInfo {
        version: tag.trim_start_matches('v').to_string(),
        name: field("name").unwrap_or(tag).to_string(),
        notes: field("body").unwrap_or_default().to_string(),
        page_url: field("html_url").unwrap_or_default().to_string(),
        zip_url: zip_url.to_string(),
    })
}

/// The certificate authorities the operating system trusts (same approach as the
/// sibling apps: rustls over the OS root store, no bundled roots).
#[cfg(not(target_arch = "wasm32"))]
fn os_roots() -> Result<ureq::tls::RootCerts, String> {
    let found = rustls_native_certs::load_native_certs();
    let certs: Vec<ureq::tls::Certificate<'static>> = found.certs.iter().map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned()).collect();
    if certs.is_empty() {
        return Err("No trusted certificates were found on this system.".to_string());
    }
    Ok(ureq::tls::RootCerts::new_with_certs(&certs))
}

#[cfg(not(target_arch = "wasm32"))]
fn agent(global_timeout: Option<Duration>) -> Result<ureq::Agent, String> {
    let mut cfg = ureq::Agent::config_builder().tls_config(ureq::tls::TlsConfig::builder().root_certs(os_roots()?).build());
    if let Some(t) = global_timeout {
        cfg = cfg.timeout_global(Some(t));
    } else {
        cfg = cfg.timeout_connect(Some(Duration::from_secs(15))).timeout_recv_body(Some(Duration::from_secs(30)));
    }
    Ok(cfg.build().new_agent())
}

/// Fetch and parse the newest release; `Ok(None)` when it is not newer than this build.
#[cfg(not(target_arch = "wasm32"))]
fn check_latest(_cancel: &AtomicBool) -> Result<Option<ReleaseInfo>, String> {
    let agent = agent(Some(Duration::from_secs(15)))?;
    let mut resp = agent
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", concat!("TsW-by-Tonynehihi/", env!("CARGO_PKG_VERSION")))
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|e| format!("Could not reach GitHub: {e}"))?;
    let status = resp.status().as_u16();
    if status == 404 {
        return Err("No published release was found on GitHub yet.".to_string());
    }
    if status != 200 {
        return Err(format!("GitHub replied with status {status}."));
    }
    let body = resp.body_mut().with_config().limit(MAX_JSON_BYTES).read_to_string().map_err(|e| format!("Could not read GitHub's reply: {e}"))?;
    let info = parse_release(&body)?;
    Ok(if is_newer(&info.version, current_version()) { Some(info) } else { None })
}

#[cfg(target_arch = "wasm32")]
fn check_latest(_cancel: &AtomicBool) -> Result<Option<ReleaseInfo>, String> {
    Err("Update checks are not available in the web build.".to_string())
}

/// Download `url` (a GitHub zipball: redirects to the file host) into `dest`,
/// reporting progress on `tx`. Written to a `.part` file and renamed when complete.
#[cfg(not(target_arch = "wasm32"))]
fn download_zip(url_str: &str, dest: &std::path::Path, cancel: &AtomicBool, tx: &Sender<JobMsg>, ctx: &egui::Context) -> Result<(), String> {
    use std::io::{Read, Write};
    let agent = agent(None)?;
    let mut resp = agent
        .get(url_str)
        .header("User-Agent", concat!("TsW-by-Tonynehihi/", env!("CARGO_PKG_VERSION")))
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|e| format!("Could not reach the download host: {e}"))?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(format!("The download host replied with status {status}."));
    }
    let total: Option<u64> = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse().ok());
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let mut file = std::fs::File::create(&part).map_err(|e| format!("Could not create the download file: {e}"))?;
    let mut reader = resp.body_mut().as_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut received: u64 = 0;
    let mut last_sent: u64 = 0;
    let outcome = loop {
        if cancel.load(Ordering::SeqCst) {
            break Err("The download was cancelled.".to_string());
        }
        match reader.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                if let Err(e) = file.write_all(buf.get(..n).unwrap_or_default()) {
                    break Err(format!("Could not write the download: {e}"));
                }
                received = received.saturating_add(n as u64);
                if received > MAX_ZIP_BYTES {
                    break Err("The update archive was larger than expected.".to_string());
                }
                if received - last_sent >= 256 * 1024 {
                    last_sent = received;
                    let _ = tx.send(JobMsg::Progress(received, total));
                    ctx.request_repaint();
                }
            }
            Err(e) => break Err(format!("The download was interrupted: {e}")),
        }
    };
    if let Err(e) = outcome {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    let _ = tx.send(JobMsg::Progress(received, total));
    std::fs::rename(&part, dest).map_err(|e| format!("Could not finish the download file: {e}"))
}

#[cfg(target_arch = "wasm32")]
fn download_zip(_url: &str, _dest: &std::path::Path, _cancel: &AtomicBool, _tx: &Sender<JobMsg>, _ctx: &egui::Context) -> Result<(), String> {
    Err("Update downloads are not available in the web build.".to_string())
}

/// How an in-app build ended.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // only the macOS worker produces every outcome
enum BuildOutcome {
    /// The staged app bundle is ready to swap in.
    Built(PathBuf),
    Failed(String),
    Cancelled,
}

/// Map one packaging-script output line to a display step, if it starts a step.
/// The script marks phases with "==> "; the raw lines carry long paths, so the
/// known ones are translated to short labels.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the macOS build worker (and tests)
fn step_label(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("==> ")?;
    let lower = rest.to_lowercase();
    let label = if lower.starts_with("ts word") {
        tl!("Preparing the build...")
    } else if lower.contains("generating") && lower.contains("icns") {
        tl!("Generating the app icon...")
    } else if lower.contains("assembling") {
        tl!("Assembling the app...")
    } else if lower.contains("notarizing") {
        tl!("Notarizing (this can take a few minutes)...")
    } else if lower.contains("building") && lower.contains(".dmg") {
        tl!("Building the disk image...")
    } else if lower.contains("building") && lower.contains("cli") {
        tl!("Packaging the command-line tool...")
    } else if lower.starts_with("done") {
        tl!("Finishing up...")
    } else {
        return Some(rest.chars().take(90).collect());
    };
    Some(label.to_string())
}

/// True for cargo's per-crate progress lines ("   Compiling egui v0.36.0").
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the macOS build worker (and tests)
fn is_compiling_line(line: &str) -> bool {
    line.trim_start().starts_with("Compiling ")
}

/// Extract the downloaded archive into `work` and build it with the packaging
/// script, streaming step progress to `tx`. Runs on a worker thread; the running
/// app and the installed copy are never touched.
#[cfg(target_os = "macos")]
fn run_build(zip: &std::path::Path, work: &std::path::Path, cancel: &AtomicBool, tx: &Sender<JobMsg>, ctx: &egui::Context) -> BuildOutcome {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let src_dir = work.join("src");
    if let Err(e) = std::fs::create_dir_all(&src_dir) {
        return BuildOutcome::Failed(format!("Could not create the build folder: {e}"));
    }
    match Command::new("/usr/bin/ditto").arg("-x").arg("-k").arg(zip).arg(&src_dir).output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => return BuildOutcome::Failed(format!("Could not extract the update archive (the extractor stopped with {}).", out.status)),
        Err(e) => return BuildOutcome::Failed(format!("Could not start the extractor: {e}")),
    }
    if cancel.load(Ordering::SeqCst) {
        return BuildOutcome::Cancelled;
    }
    // The archive holds one top folder containing Cargo.toml.
    let mut source: Option<PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(&src_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && p.join("Cargo.toml").is_file() {
                source = Some(p);
                break;
            }
        }
    }
    let Some(source) = source else {
        return BuildOutcome::Failed("The update archive did not contain the app source.".to_string());
    };
    let _ = tx.send(JobMsg::BuildStep { step: tl!("Building the new version...").to_string(), compiled: 0 });
    ctx.request_repaint();

    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut path = home.join(".cargo").join("bin").to_string_lossy().into_owned();
    path.push_str(":/opt/homebrew/bin:/usr/local/bin:");
    path.push_str(&std::env::var("PATH").unwrap_or_default());
    let script = source.join("packaging").join("macos").join("package.sh");
    let mut child = match Command::new("/bin/bash")
        .arg(&script)
        .arg("--arch")
        .arg("aarch64")
        .current_dir(&source)
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return BuildOutcome::Failed(format!("Could not start the build: {e}")),
    };
    // Forward both output streams into one line channel (the handles are dropped
    // on purpose: the reader threads end when the process closes its pipes).
    let (ltx, lrx) = std::sync::mpsc::channel::<String>();
    if let Some(out) = child.stdout.take() {
        let t = ltx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                match line {
                    Ok(l) => {
                        if t.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let t = ltx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines() {
                match line {
                    Ok(l) => {
                        if t.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    drop(ltx);
    let mut log = std::fs::File::create(work.join("build.log")).ok();
    let mut compiled: u32 = 0;
    let status = loop {
        match lrx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                if let Some(f) = log.as_mut() {
                    let _ = writeln!(f, "{line}");
                }
                if is_compiling_line(&line) {
                    compiled = compiled.saturating_add(1);
                    if compiled % 10 == 0 {
                        let _ = tx.send(JobMsg::BuildStep { step: tl!("Building the new version...").to_string(), compiled });
                        ctx.request_repaint();
                    }
                } else if let Some(step) = step_label(&line) {
                    let _ = tx.send(JobMsg::BuildStep { step, compiled });
                    ctx.request_repaint();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::SeqCst) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return BuildOutcome::Cancelled;
                }
                match child.try_wait() {
                    Ok(Some(s)) => break s,
                    Ok(None) => {}
                    Err(e) => return BuildOutcome::Failed(format!("The build process could not be watched: {e}")),
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => match child.wait() {
                Ok(s) => break s,
                Err(e) => return BuildOutcome::Failed(format!("The build process could not be watched: {e}")),
            },
        }
    };
    if cancel.load(Ordering::SeqCst) {
        return BuildOutcome::Cancelled;
    }
    if !status.success() {
        return BuildOutcome::Failed(format!(
            "The new version could not be built (the build stopped with {status}). Your current version was not changed. The build log is at {}",
            work.join("build.log").display()
        ));
    }
    // The packaged bundle in the output folder (found by extension, not by name,
    // so an app rename can never make the updater look for a stale bundle name).
    let pkg_dir = source.join("target").join("macos-package");
    let mut built: Option<PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(&pkg_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && p.extension().is_some_and(|e| e == "app") {
                built = Some(p);
                break;
            }
        }
    }
    match built {
        Some(app) => BuildOutcome::Built(app),
        None => BuildOutcome::Failed("The build finished but the packaged app was not produced. Your current version was not changed.".to_string()),
    }
}

/// Non-macOS builds never start a build (the UI stops earlier); this only
/// satisfies the compiler.
#[cfg(not(target_os = "macos"))]
fn run_build(_zip: &std::path::Path, _work: &std::path::Path, _cancel: &AtomicBool, _tx: &Sender<JobMsg>, _ctx: &egui::Context) -> BuildOutcome {
    BuildOutcome::Failed("Automatic install is only available on macOS.".to_string())
}

/// Shell swapper launched detached by [`UpdateState::finish_install`]: the build
/// already happened inside the app, so this only waits for the app to quit,
/// moves the old app out of /Applications into a backup folder (so it cannot be
/// launched mid-swap), installs the built app and relaunches. Every step logs to
/// ~/Library/Logs/TSWord-update.log; a failed install restores the
/// old app from the backup and reopens it.
const SWAP_SCRIPT: &str = r#"#!/bin/sh
BUILT="$1"
VER="${2:-unknown}"
WORK="${3:-}"
DEST="/Applications/TsW.app"
CACHE_DIR="$HOME/Library/Caches/TS Word/updates"
BACKUP="$CACHE_DIR/backup/TsW.app"
LOG_DIR="$HOME/Library/Logs"
LOG="$LOG_DIR/TSWord-update.log"
mkdir -p "$LOG_DIR" "$CACHE_DIR/backup"
exec >>"$LOG" 2>&1
echo ""
echo "=== Installing TS Word $VER - $(date) ==="
INSTALLED=0
fail() {
  echo "FAILED: $1"
  if [ "$INSTALLED" != 1 ] && [ -d "$BACKUP" ]; then
    rm -rf "$DEST"
    mv "$BACKUP" "$DEST" || true
  fi
  open "$DEST" >/dev/null 2>&1 || true
  exit 1
}
i=0
while pgrep -f "/TsW.app/Contents/MacOS/TsW" >/dev/null 2>&1; do
  i=$((i+1))
  if [ "$i" -gt 120 ]; then
    echo "The app was still running after 120s - install postponed, nothing changed."
    exit 0
  fi
  sleep 1
done
[ -d "$BUILT" ] || fail "the built app is missing"
rm -rf "$BACKUP"
mv "$DEST" "$BACKUP" || fail "could not move the old app aside"
ditto "$BUILT" "$DEST" || fail "install the new app"
INSTALLED=1
echo "=== Update to $VER installed (previous version kept at $BACKUP) ==="
[ -n "$WORK" ] && rm -rf "$WORK"
open "$DEST" || fail "relaunch the app"
"#;

/// The Updates section at the bottom of the About dialog.
pub fn about_section(app: &mut WordApp, ui: &mut egui::Ui) {
    // Workers wake the UI themselves when they post a message; this just drains.
    app.updates.poll();
    let t = Tokens::get(ui.ctx());
    ui.label(egui::RichText::new(tl!("Updates")).font(crate::theme::semibold(15.0)).color(t.text));
    ui.add_space(2.0);
    let phase = app.updates.phase.clone();
    let mut open_page: Option<String> = None;
    match &phase {
        Phase::Idle => {
            if ui.button(tl!("Check for Updates")).clicked() {
                app.updates.start_check(ui.ctx());
            }
        }
        Phase::Checking => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(tl!("Checking for updates…"));
            });
        }
        Phase::UpToDate => {
            ui.label(egui::RichText::new(tl!("You have the latest version.")).color(t.text_dim));
            if ui.button(tl!("Check Again")).clicked() {
                app.updates.start_check(ui.ctx());
            }
        }
        Phase::Available(info) => {
            ui.label(crate::i18n::fmt(
                tl!("Version {version} is available - you have {current}."),
                &[("version", &info.version), ("current", current_version())],
            ));
            if !info.notes.trim().is_empty() {
                egui::ScrollArea::vertical().id_salt("update-notes").max_height(90.0).show(ui, |ui| {
                    notes_ui(ui, info.notes.trim(), &t);
                });
            }
            ui.horizontal(|ui| {
                if cfg!(target_os = "macos") {
                    if ui.button(tl!("Download Update")).clicked() {
                        app.updates.start_download(ui.ctx());
                    }
                } else if ui.button(tl!("Open Release Page")).clicked() && !info.page_url.is_empty() {
                    open_page = Some(info.page_url.clone());
                }
                if ui.button(tl!("Check Again")).clicked() {
                    app.updates.start_check(ui.ctx());
                }
            });
        }
        Phase::Downloading { received, total, .. } => {
            ui.horizontal(|ui| {
                ui.spinner();
                let mb = format!("{:.1}", *received as f64 / 1_000_000.0);
                ui.label(crate::i18n::fmt(tl!("Downloading update... {mb} MB"), &[("mb", &mb)]));
            });
            if let Some(total) = total
                && *total > 0
            {
                let frac = (*received as f64 / *total as f64).clamp(0.0, 1.0) as f32;
                ui.add(egui::ProgressBar::new(frac).desired_width(220.0));
            }
        }
        Phase::Downloaded { info, .. } => {
            ui.label(crate::i18n::fmt(tl!("Update {version} is downloaded."), &[("version", &info.version)]));
            ui.label(egui::RichText::new(tl!("Installing builds the new version while the app stays open, with progress shown here. The app closes only for a few seconds at the end to swap in the new version, then reopens by itself.")).color(t.text_dim));
            ui.horizontal(|ui| {
                if ui.button(tl!("Install Update")).clicked() {
                    app.updates.start_build(ui.ctx());
                }
                if ui.button(tl!("Later")).clicked() {
                    app.updates.phase = Phase::Available(info.clone());
                }
            });
        }
        Phase::Building { info, step, compiled, .. } => {
            building_ui(app, ui, info, step, *compiled);
        }
        Phase::ReadyToInstall { info, .. } => {
            ui.label(crate::i18n::fmt(tl!("Version {version} is built and ready to install."), &[("version", &info.version)]));
            ui.label(
                egui::RichText::new(tl!("The app will close for a few seconds, swap in the new version and reopen automatically.")).color(t.text_dim),
            );
            if ui.button(tl!("Finish Install")).clicked() {
                match app.updates.finish_install() {
                    Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                    Err(e) => app.updates.phase = Phase::Failed(e),
                }
            }
        }
        Phase::Failed(msg) => {
            ui.label(egui::RichText::new(msg.as_str()).color(t.text_dim));
            if ui.button(tl!("Try Again")).clicked() {
                app.updates.start_check(ui.ctx());
            }
        }
    }
    if let Some(url) = open_page {
        app.canvas.open_url = Some(url);
    }
}

fn push_span(spans: &mut Vec<(String, bool)>, text: &str, bold: bool) {
    if text.is_empty() {
        return;
    }
    if let Some((last, b)) = spans.last_mut()
        && *b == bold
    {
        last.push_str(text);
        return;
    }
    spans.push((text.to_string(), bold));
}

/// Split release notes into (text, bold) spans the way GitHub renders them:
/// `**bold**` toggles bold, a leading `#`/`##` makes the whole line a bold
/// heading, backticks are dropped (their text stays, as plain text). A marker
/// left open runs to the end of its line. Adjacent spans of one kind are merged.
fn notes_spans(notes: &str) -> Vec<(String, bool)> {
    let mut spans: Vec<(String, bool)> = Vec::new();
    for (i, line) in notes.lines().enumerate() {
        if i > 0 {
            push_span(&mut spans, "\n", false);
        }
        let (text, mut bold) = match line.strip_prefix("## ").or_else(|| line.strip_prefix("# ")) {
            Some(rest) => (rest, true),
            None => (line, false),
        };
        let mut cur = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '*' && chars.peek() == Some(&'*') {
                chars.next();
                push_span(&mut spans, &cur, bold);
                cur.clear();
                bold = !bold;
            } else if c != '`' {
                cur.push(c);
            }
        }
        push_span(&mut spans, &cur, bold);
    }
    spans
}

/// Show release notes the way GitHub does (see [`notes_spans`]): a plain label
/// would print the `**` markers literally. Bold spans use the semibold face in
/// the brighter label color, like the app's other headings.
fn notes_ui(ui: &mut egui::Ui, notes: &str, t: &Tokens) {
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = ui.available_width();
    for (text, bold) in notes_spans(notes) {
        let format = egui::text::TextFormat {
            font_id: if bold { crate::theme::semibold(13.0) } else { egui::FontId::proportional(13.0) },
            color: if bold { t.text } else { t.text_dim },
            ..Default::default()
        };
        job.append(&text, 0.0, format);
    }
    ui.label(job);
}

/// Shared progress display for the in-app build (About section + launch prompt):
/// the current step, how many crates are done, and the elapsed time — the build
/// is only "done" when it flips to [`Phase::ReadyToInstall`].
fn building_ui(app: &mut WordApp, ui: &mut egui::Ui, info: &ReleaseInfo, step: &str, compiled: u32) {
    let t = Tokens::get(ui.ctx());
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(crate::i18n::fmt(tl!("Building version {version}..."), &[("version", &info.version)]));
    });
    ui.label(egui::RichText::new(step).color(t.text_dim));
    let mut detail = String::new();
    if compiled > 0 {
        detail.push_str(&crate::i18n::fmt(tl!("{compiled} crates compiled"), &[("compiled", &compiled.to_string())]));
    }
    if let Some(el) = app.updates.build_elapsed() {
        let secs = el.as_secs();
        if !detail.is_empty() {
            detail.push_str("  -  ");
        }
        let clock = format!("{}:{:02}", secs / 60, secs % 60);
        detail.push_str(&crate::i18n::fmt(tl!("{elapsed} elapsed"), &[("elapsed", &clock)]));
    }
    if !detail.is_empty() {
        ui.label(egui::RichText::new(detail).color(t.text_dim));
    }
    ui.label(egui::RichText::new(tl!("Your current version keeps working and is not changed until the new one is fully built.")).color(t.text_dim));
    if ui.button(tl!("Cancel Build")).clicked() {
        app.updates.cancel_build();
    }
}

/// The launch-time update prompt: [`UpdateState::poll`] opens it when the startup
/// check found a newer release, and only then — a launch with nothing new (or no
/// connection) shows nothing. It shows the release notes (the changelog) so the
/// choice is informed, then runs the same download/install flow as About.
pub fn prompt_window(app: &mut WordApp, ctx: &egui::Context) {
    if !app.updates.prompt {
        return;
    }
    let t = Tokens::get(ctx);
    let phase = app.updates.phase.clone();
    // a phase with nothing to ask about closes the prompt again
    if matches!(phase, Phase::Idle | Phase::Checking | Phase::UpToDate) {
        app.updates.prompt = false;
        return;
    }
    let mut close_remember = false;
    let mut close_plain = false;
    let mut open_page: Option<String> = None;
    egui::Window::new(tl!("A new version is available"))
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.set_width(430.0);
            match &phase {
                Phase::Available(info) => {
                    ui.label(crate::i18n::fmt(tl!("Version {version} is available - you have {current}."), &[("version", &info.version), ("current", current_version())]));
                    if !info.notes.trim().is_empty() {
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(tl!("What's new")).font(crate::theme::semibold(13.0)).color(t.text));
                        egui::ScrollArea::vertical().id_salt("prompt-update-notes").max_height(180.0).show(ui, |ui| {
                            notes_ui(ui, info.notes.trim(), &t);
                        });
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if cfg!(target_os = "macos") {
                            if ui.button(tl!("Download Update")).clicked() {
                                app.updates.start_download(ui.ctx());
                            }
                        } else if ui.button(tl!("Open Release Page")).clicked() && !info.page_url.is_empty() {
                            open_page = Some(info.page_url.clone());
                        }
                        if ui.button(tl!("Later")).clicked() {
                            close_remember = true;
                        }
                    });
                }
                Phase::Downloading { received, total, .. } => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        let mb = format!("{:.1}", *received as f64 / 1_000_000.0);
                        ui.label(crate::i18n::fmt(tl!("Downloading update... {mb} MB"), &[("mb", &mb)]));
                    });
                    if let Some(total) = total && *total > 0 {
                        let frac = (*received as f64 / *total as f64).clamp(0.0, 1.0) as f32;
                        ui.add(egui::ProgressBar::new(frac).desired_width(300.0));
                    }
                    ui.add_space(4.0);
                    if ui.button(tl!("Hide")).clicked() {
                        close_plain = true;
                    }
                }
                Phase::Downloaded { info, .. } => {
                    ui.label(crate::i18n::fmt(tl!("Update {version} is downloaded."), &[("version", &info.version)]));
                    ui.label(egui::RichText::new(tl!("Installing builds the new version while the app stays open, with progress shown here. The app closes only for a few seconds at the end to swap in the new version, then reopens by itself.")).color(t.text_dim));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button(tl!("Install Update")).clicked() {
                            app.updates.start_build(ui.ctx());
                        }
                        if ui.button(tl!("Later")).clicked() {
                            app.updates.phase = Phase::Available(info.clone());
                            close_remember = true;
                        }
                    });
                }
                Phase::Building { info, step, compiled, .. } => {
                    building_ui(app, ui, info, step, *compiled);
                    ui.add_space(4.0);
                    if ui.button(tl!("Hide")).clicked() {
                        close_plain = true;
                    }
                }
                Phase::ReadyToInstall { info, .. } => {
                    ui.label(crate::i18n::fmt(tl!("Version {version} is built and ready to install."), &[("version", &info.version)]));
                    ui.label(egui::RichText::new(tl!("The app will close for a few seconds, swap in the new version and reopen automatically.")).color(t.text_dim));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button(tl!("Finish Install")).clicked() {
                            match app.updates.finish_install() {
                                Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                                Err(e) => app.updates.phase = Phase::Failed(e),
                            }
                        }
                        if ui.button(tl!("Later")).clicked() {
                            close_remember = true;
                        }
                    });
                }
                Phase::Failed(msg) => {
                    ui.label(egui::RichText::new(msg.as_str()).color(t.text_dim));
                    ui.add_space(4.0);
                    if ui.button(tl!("Close")).clicked() {
                        close_plain = true;
                    }
                }
                _ => {}
            }
        });
    if close_remember {
        app.updates.dismiss_prompt(true);
    }
    if close_plain {
        app.updates.dismiss_prompt(false);
    }
    if let Some(url) = open_page {
        app.canvas.open_url = Some(url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_compare_by_numbers() {
        assert!(is_newer("1.1.6", "1.1.5"));
        assert!(is_newer("1.2.0", "1.1.9"));
        assert!(is_newer("2.0", "1.9.9"));
        assert!(is_newer("1.1.5.1", "1.1.5"));
        assert!(!is_newer("1.1.5", "1.1.5"));
        assert!(!is_newer("1.1.4", "1.1.5"));
        assert!(!is_newer("1.1.5", "1.1.5.1"));
        assert!(!is_newer("1.9.9", "2.0"));
    }

    #[test]
    fn release_json_parses_and_strips_the_v() {
        let json = r#"{
            "tag_name": "v1.2.3",
            "name": "TS Word 1.2.3",
            "body": "What changed.",
            "html_url": "https://github.com/tonynehihi/tsw-by-tonynehihi/releases/tag/v1.2.3",
            "zipball_url": "https://api.github.com/repos/tonynehihi/tsw-by-tonynehihi/zipball/v1.2.3"
        }"#;
        let info = parse_release(json).expect("sample release parses");
        assert_eq!(info.version, "1.2.3");
        assert_eq!(info.name, "TS Word 1.2.3");
        assert_eq!(info.notes, "What changed.");
        assert!(info.zip_url.ends_with("/zipball/v1.2.3"));
    }

    #[test]
    fn launch_prompt_only_for_a_fresh_startup_find() {
        assert!(prompt_wanted(true, None, "1.1.6"));
        assert!(!prompt_wanted(false, None, "1.1.6"), "manual checks never prompt");
        assert!(!prompt_wanted(true, Some("1.1.6"), "1.1.6"), "already waved away with Later");
        assert!(prompt_wanted(true, Some("1.1.5"), "1.1.6"), "a newer find prompts again");
    }

    #[test]
    fn release_json_without_a_source_archive_is_an_error() {
        assert!(parse_release(r#"{"tag_name": "v1.0.0"}"#).is_err());
        assert!(parse_release("not json").is_err());
    }

    #[test]
    fn swap_script_only_swaps_and_restores_on_failure() {
        // The build happens in the app now; the script must not build anything.
        assert!(!SWAP_SCRIPT.contains("package.sh"), "swap script must not build");
        for needle in ["pgrep -f", "/Applications/TsW.app", "mv \"$DEST\" \"$BACKUP\"", "ditto", "open \"$DEST\""] {
            assert!(SWAP_SCRIPT.contains(needle), "swap script must contain: {needle}");
        }
    }

    #[test]
    fn notes_spans_render_bold_headers_and_code() {
        let spans = notes_spans("## TS Word 1.1.0\n- **NEW THING**: does `stuff` now\nplain **bold** tail");
        assert_eq!(
            spans,
            vec![
                ("TS Word 1.1.0".to_string(), true),
                ("\n- ".to_string(), false),
                ("NEW THING".to_string(), true),
                (": does stuff now\nplain ".to_string(), false),
                ("bold".to_string(), true),
                (" tail".to_string(), false),
            ]
        );
        // a single star is literal text; an unclosed marker bolds to the line's end
        assert_eq!(notes_spans("a * b"), vec![("a * b".to_string(), false)]);
        assert_eq!(notes_spans("x **y"), vec![("x ".to_string(), false), ("y".to_string(), true)]);
        assert_eq!(notes_spans("just text"), vec![("just text".to_string(), false)]);
    }

    #[test]
    fn build_output_lines_map_to_steps_and_counts() {
        assert_eq!(step_label("==> TS Word 1.1.0 for macOS (aarch64), identity: -").as_deref(), Some("Preparing the build..."));
        assert_eq!(step_label("==> assembling /long/path/TsW.app").as_deref(), Some("Assembling the app..."));
        assert_eq!(step_label("==> building /long/path/tsw-by-tonynehihi-1.1.0-macos-aarch64.dmg").as_deref(), Some("Building the disk image..."));
        assert_eq!(
            step_label("==> building /long/path/wordcraft-cli-1.1.0-macos-aarch64.zip").as_deref(),
            Some("Packaging the command-line tool...")
        );
        assert_eq!(step_label("==> done").as_deref(), Some("Finishing up..."));
        assert_eq!(step_label("   Compiling egui v0.36.0"), None);
        assert_eq!(step_label("some other line"), None);
        assert!(is_compiling_line("   Compiling egui v0.36.0"));
        assert!(is_compiling_line("Compiling proc-macro2 v1.0.0"));
        assert!(!is_compiling_line("    Finished `release` profile in 1m 30s"));
        assert!(!is_compiling_line("==> assembling"));
    }
}
