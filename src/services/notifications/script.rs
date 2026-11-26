//! Custom script provider (#288): run a program when an event fires,
//! the way Sonarr's Custom Script connection does.
//!
//! ## What runs
//!
//! The saved path, directly: no shell, no arguments, stdin closed, the
//! script's own folder as the working directory. The event arrives as
//! environment variables, `ryokan_eventtype` plus one `ryokan_<field>`
//! per field of the event (`ryokan_dest_path`, `ryokan_series_title`,
//! ...). Lower case because Sonarr's own `Sonarr_*` names reach a
//! script lower-cased on Linux, and a script ported from there already
//! reads them that way.
//!
//! ## Why a queue
//!
//! The dispatcher gives a provider five seconds, and a script that
//! re-encodes a file takes minutes. `send` therefore only queues the
//! run; one worker runs the queue one script at a time, so a pack's
//! twenty imports do not start twenty encoders. The queue is
//! process-wide and not owned by a provider, because the provider
//! objects are rebuilt on every settings save and a queue inside one
//! would drop its waiting runs with it.
//!
//! ## Guards
//!
//! Running a program as Ryokan's user is the feature, and it is on by
//! default, as in Sonarr. What stands between a logged-in session and
//! an arbitrary command is narrower than a switch:
//!
//! * `RYOKAN_CUSTOM_SCRIPTS=0` turns the whole kind off ([`enabled`]).
//! * No arguments, so a system binary cannot be pointed at anything.
//! * The path must be absolute, a regular file, and executable for
//!   Ryokan's user.
//! * Not in a system folder (Sonarr's list, [`SYSTEM_FOLDERS`]).
//! * Not a file that came from a download, as far as Ryokan can tell:
//!   nothing under the media root, the recycle bin, a download client's
//!   configured download path, or the content of any grab on record.
//!
//! The path is checked on save and again before every run: the file can
//! change in between. A run also reads its provider's row again, so
//! deleting, disabling or re-pointing a provider takes effect on the
//! runs still waiting in the queue.
//!
//! None of this is a security boundary. The executable bit is not one
//! (libtorrent sets it for a file its torrent marks executable), and a
//! download folder Ryokan was never told about is not on the list. What
//! keeps a stranger from running a program is the login, as in Sonarr.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;

use super::{NotificationEvent, NotificationProvider};
use crate::models::log::LogCategory;
use crate::services::logger;

/// The environment variable that turns custom scripts off.
pub const SWITCH_VAR: &str = "RYOKAN_CUSTOM_SCRIPTS";

/// Time limit for a new provider, and for a stored config without one.
pub const DEFAULT_TIMEOUT_MINUTES: u64 = 30;
/// The longest limit the form accepts: a day.
pub const MAX_TIMEOUT_MINUTES: u64 = 24 * 60;

/// The Settings Test button's own limit. A test is `ryokan_eventtype=
/// Test` and nothing to work on, so a script that is still going after
/// this is waiting on something.
pub const TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs waiting behind the one in progress. An import of a long series
/// queues one per file; past this a run is dropped and logged.
const QUEUE_CAPACITY: usize = 1024;

/// How much of each output stream is kept: the end, where a failing
/// script says why.
const OUTPUT_TAIL_BYTES: usize = 4096;
/// How much of that reaches one log row.
const OUTPUT_LOG_CHARS: usize = 1000;

/// After the time limit: how long the script has to exit on SIGTERM
/// before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(5);
/// After SIGKILL: how long the group is given to be gone.
const KILL_GRACE: Duration = Duration::from_secs(1);
/// One environment value's size. Linux refuses to start a program with
/// a single value over 128 KiB, and a misgrab's file list can be long.
const ENV_VALUE_MAX_BYTES: usize = 32 * 1024;
/// After the script exits: how long its output pipes are read for. A
/// script that leaves a background job holding stdout would otherwise
/// keep the run open until that job ends.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Sonarr's `SystemFolders` for Linux and macOS (its Custom Script
/// settings refuse a path equal to or under one). A script that is
/// really `/sbin/reboot` needs no arguments to do harm.
pub const SYSTEM_FOLDERS: &[&str] = &[
    "/bin",
    "/boot",
    "/lib",
    "/sbin",
    "/proc",
    "/usr/bin",
    "/usr/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
    "/System",
];

// ── The switch ─────────────────────────────────────────────────────────────

/// `RYOKAN_CUSTOM_SCRIPTS` as (scripts allowed, startup warning).
///
/// Unset or blank is on, the default. A value that is not a yes or a no
/// turns scripts off, with a warning: someone who set the variable was
/// reaching for the switch, and off is the reading that cannot run
/// anything they did not mean to (`RYOKAN_HOST_CHECK` fails closed the
/// same way).
fn read_switch(raw: Option<&str>) -> (bool, Option<String>) {
    let value = raw.unwrap_or("").trim().to_ascii_lowercase();
    match value.as_str() {
        "" | "1" | "true" | "yes" | "on" => (true, None),
        "0" | "false" | "no" | "off" => (false, None),
        _ => (
            false,
            Some(format!(
                "{SWITCH_VAR}={value:?} isn't a yes or no value, so custom scripts are off. Set it to 1 or 0."
            )),
        ),
    }
}

static SWITCH: LazyLock<bool> = LazyLock::new(|| {
    let raw = std::env::var(SWITCH_VAR).ok();
    let (on, warning) = read_switch(raw.as_deref());
    if let Some(warning) = warning {
        tracing::warn!("{warning}");
    } else if !on {
        tracing::info!("Custom scripts are off ({SWITCH_VAR})");
    }
    on
});

/// Whether custom scripts may run in this process. Read from the
/// environment once, so nothing in the UI can turn it back on.
/// `main.rs` forces it at startup, which is when it logs.
pub fn enabled() -> bool {
    *SWITCH
}

/// What a save, a test or a run says when the switch is off.
pub fn disabled_message() -> String {
    format!(
        "Custom scripts are turned off by the {SWITCH_VAR} environment variable, so this one can't be saved or run."
    )
}

// ── Config and the path check ──────────────────────────────────────────────

/// `notification_providers.config_json` for `kind = 'script'`. Nothing
/// in it is a secret; the path is shown on the card and in the form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptConfig {
    pub path: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_MINUTES * 60
}

impl ScriptConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.clamp(1, MAX_TIMEOUT_MINUTES * 60))
    }

    /// The limit in whole minutes, for the form.
    pub fn timeout_minutes(&self) -> u64 {
        self.timeout().as_secs().div_ceil(60)
    }
}

/// The folders, beyond the system ones, no script may live under:
/// places whose files come from downloads.
#[derive(Debug, Clone, Default)]
pub struct PathPolicy {
    pub media_root: String,
    pub recycle_bin: String,
    /// Each download client's "Download path (as Ryokan sees it)",
    /// where one is set.
    pub download_folders: Vec<String>,
}

impl PathPolicy {
    /// `Err` when the settings cannot be read. Read as "no folders",
    /// a failed query would switch the folder rules off.
    pub async fn load(db: &SqlitePool) -> Result<Self, String> {
        let unreadable = |e: sqlx::Error| {
            format!("Ryokan couldn't read its settings to check the script path: {e}")
        };
        let cfg = crate::models::config::get_config(db)
            .await
            .map_err(unreadable)?
            .unwrap_or_default();
        let clients = crate::models::download_clients::list_all(db)
            .await
            .map_err(unreadable)?;
        Ok(Self {
            media_root: cfg.media_root,
            recycle_bin: cfg.recycle_bin_path,
            download_folders: clients
                .into_iter()
                .map(|client| client.download_path)
                .filter(|folder| !folder.trim().is_empty())
                .collect(),
        })
    }
}

/// `path` resolved, when it names a program this install will run. The
/// message is shown on the Notifications page and written to the log.
///
/// The folder rules are applied to the path as typed and as resolved,
/// so a link elsewhere into `/usr/bin` or the library is refused like
/// the file it points at.
pub fn validate_path(path: &str, policy: &PathPolicy) -> Result<PathBuf, String> {
    let typed = path.trim();
    if typed.is_empty() {
        return Err("Script path is required".into());
    }
    if typed.contains('\0') {
        return Err("Script path can't contain a NUL character".into());
    }
    let typed_path = Path::new(typed);
    if !typed_path.is_absolute() {
        return Err(format!(
            "Script path must be a full path starting with /, and {typed} is not"
        ));
    }
    // As typed first, so a refused folder is named whether or not the
    // file is there.
    refuse_folder(typed, typed_path, policy)?;
    let resolved = std::fs::canonicalize(typed_path).map_err(|e| {
        format!(
            "Ryokan can't open {typed} ({}). In Docker, use the path as it is inside the container.",
            io_reason(&e)
        )
    })?;
    let meta = std::fs::metadata(&resolved)
        .map_err(|e| format!("Ryokan can't read {typed} ({})", io_reason(&e)))?;
    if !meta.is_file() {
        return Err(format!("{typed} is not a file"));
    }
    refuse_folder(typed, &resolved, policy)?;

    nix::unistd::access(&resolved, nix::unistd::AccessFlags::X_OK).map_err(|_| {
        format!(
            "{typed} isn't executable for the user Ryokan runs as. Run chmod +x on it, and check the folders above it can be entered."
        )
    })?;
    Ok(resolved)
}

/// The folder rules for one spelling of the path.
fn refuse_folder(typed: &str, candidate: &Path, policy: &PathPolicy) -> Result<(), String> {
    if let Some(folder) = SYSTEM_FOLDERS.iter().find(|f| candidate.starts_with(f)) {
        return Err(format!(
            "{typed} is in the system folder {folder}. Keep the script somewhere of your own."
        ));
    }
    let downloads = policy
        .download_folders
        .iter()
        .map(|folder| (folder, "a download folder"));
    let roots = [
        (&policy.media_root, "the media library"),
        (&policy.recycle_bin, "the recycle bin"),
    ];
    for (root, what) in roots.into_iter().chain(downloads) {
        if is_under(candidate, root) {
            return Err(format!(
                "{typed} is inside {what}. Files there come from downloads, so nothing in it is run. Keep the script somewhere else."
            ));
        }
    }
    Ok(())
}

/// Whether `path` is `root` or inside it. `root` is compared as typed
/// and as resolved; a blank one holds nothing.
fn is_under(path: &Path, root: &str) -> bool {
    let root = root.trim();
    if root.is_empty() {
        return false;
    }
    let root = Path::new(root);
    path.starts_with(root) || std::fs::canonicalize(root).is_ok_and(|r| path.starts_with(r))
}

/// An I/O error without the "(os error N)" tail.
fn io_reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "no such file".to_string(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        _ => e.to_string(),
    }
}

/// Everything a save, a test and a run check about the path:
/// [`validate_path`] against the folders the settings name (off the
/// async workers: it stats the file, which can sit on a network mount),
/// then the grabs on record.
pub async fn check_path(db: &SqlitePool, path: &str) -> Result<PathBuf, String> {
    let policy = PathPolicy::load(db).await?;
    let typed = path.trim().to_string();
    let resolved = {
        let typed = typed.clone();
        tokio::task::spawn_blocking(move || validate_path(&typed, &policy))
            .await
            .map_err(|e| format!("Script path check failed: {e}"))??
    };
    for candidate in [typed.as_str(), &resolved.to_string_lossy()] {
        if is_grabbed_content(db, candidate).await? {
            return Err(format!(
                "{typed} is part of a download. Files from downloads are never run. Keep the script somewhere else."
            ));
        }
    }
    Ok(resolved)
}

/// Whether `path` is, or is inside, the content of a grab on record
/// (the path its client reported once it finished). This is what covers
/// a download folder the settings do not name: the library's hardlinked
/// twin of an imported file sits there, outside the media root.
async fn is_grabbed_content(db: &SqlitePool, path: &str) -> Result<bool, String> {
    sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM grabbed_torrents
         WHERE length(rtrim(COALESCE(client_content_path, ''), '/')) > 1
           AND (? = rtrim(client_content_path, '/')
                OR substr(?, 1, length(rtrim(client_content_path, '/')) + 1)
                   = rtrim(client_content_path, '/') || '/')
         LIMIT 1",
    )
    .bind(path)
    .bind(path)
    .fetch_optional(db)
    .await
    .map(|hit| hit.is_some())
    .map_err(|e| format!("Ryokan couldn't read its downloads to check the script path: {e}"))
}

// ── Environment ────────────────────────────────────────────────────────────

/// The variables a run for `event` gets: `ryokan_eventtype`, then one
/// `ryokan_<field>` per field of the event's data. A missing value is
/// an empty variable, never an unset one, so a script under `set -u`
/// can read every field of its event. A list is joined with `|`, as
/// Sonarr joins its own.
pub fn event_env(event: &NotificationEvent) -> Vec<(String, String)> {
    let mut env = vec![("ryokan_eventtype".to_string(), event.kind().to_string())];
    let data = serde_json::to_value(event)
        .ok()
        .and_then(|v| v.get("data").cloned());
    if let Some(serde_json::Value::Object(fields)) = data {
        for (name, value) in fields {
            env.push((format!("ryokan_{name}"), env_value(&value)));
        }
    }
    env
}

/// The Settings Test button's environment, Sonarr's `EventType=Test`.
pub fn test_env() -> Vec<(String, String)> {
    vec![("ryokan_eventtype".to_string(), "Test".to_string())]
}

fn env_value(value: &serde_json::Value) -> String {
    use serde_json::Value;
    let text = match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(env_value).collect::<Vec<_>>().join("|"),
        other => other.to_string(),
    };
    // A NUL cannot go into an environment block, and neither can a
    // value past the kernel's size limit; either would fail the spawn.
    let mut text = text.replace('\0', "");
    if text.len() > ENV_VALUE_MAX_BYTES {
        let mut end = ENV_VALUE_MAX_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// The last `max_chars` of `text`, marked when cut. A failing script
/// says why at the end of what it wrote, so that is the part to keep.
pub fn tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().skip(count - max_chars).collect();
    format!("…{}", kept.trim_start_matches('…'))
}

// ── Running one script ─────────────────────────────────────────────────────

/// How one run ended.
#[derive(Debug, Clone)]
pub struct ScriptOutput {
    /// `None` when a signal ended it.
    pub exit_code: Option<i32>,
    /// It ran past its limit and was stopped.
    pub timed_out: bool,
    pub elapsed: Duration,
    /// The end of each stream.
    pub stdout: String,
    pub stderr: String,
}

impl ScriptOutput {
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }

    /// What the script said, for one line of a result: stderr when it
    /// wrote any, else stdout.
    pub fn said(&self) -> &str {
        if self.stderr.is_empty() {
            &self.stdout
        } else {
            &self.stderr
        }
    }
}

/// Run `path` with `env` added to Ryokan's own environment, and stop it
/// at `timeout`. `Err` is a run that never started.
///
/// The script leads its own process group, so stopping it at the limit
/// stops what it started too (an `ffmpeg` under a shell script would
/// otherwise run on, holding the output pipes open). The whole group
/// gets SIGTERM and [`TERM_GRACE`] to finish up, then SIGKILL.
///
/// Dropping this future stops only the script itself, not its group, so
/// callers run it in a task that is never dropped: the queue's worker,
/// and [`run_test`]'s own task.
pub async fn run(
    path: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> Result<ScriptOutput, String> {
    run_with_grace(path, env, timeout, TERM_GRACE).await
}

async fn run_with_grace(
    path: &Path,
    env: &[(String, String)],
    timeout: Duration,
    term_grace: Duration,
) -> Result<ScriptOutput, String> {
    let mut command = tokio::process::Command::new(path);
    command
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        // The script runs as Ryokan's user and could read the key file,
        // but there is no reason to hand the key over in its environment.
        .env_remove("RYOKAN_ENCRYPTION_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(folder) = path.parent() {
        command.current_dir(folder);
    }

    let started = Instant::now();
    let mut child = spawn_child(&mut command)
        .await
        .map_err(|e| format!("couldn't start {}: {}", path.display(), spawn_reason(&e)))?;
    let group = child.id();
    let stdout = Tail::capture(child.stdout.take());
    let stderr = Tail::capture(child.stderr.take());

    let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => (status, false),
        Err(_) => {
            signal_group(group, nix::sys::signal::Signal::SIGTERM);
            let deadline = Instant::now() + term_grace;
            let status = match tokio::time::timeout(term_grace, child.wait()).await {
                Ok(status) => status,
                Err(_) => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            // A shell dies on SIGTERM at once; the encoder it started
            // gets the rest of the grace to finish its file. Whatever
            // is left after that is killed.
            if !group_gone_by(group, deadline).await {
                signal_group(group, nix::sys::signal::Signal::SIGKILL);
                group_gone_by(group, Instant::now() + KILL_GRACE).await;
            }
            (status, true)
        }
    };
    let status = status.map_err(|e| format!("couldn't wait for {}: {e}", path.display()))?;

    Ok(ScriptOutput {
        exit_code: status.code(),
        timed_out,
        elapsed: started.elapsed(),
        stdout: stdout.finish().await,
        stderr: stderr.finish().await,
    })
}

/// Spawn, trying again on "text file busy": a script saved a moment ago
/// can still be open for writing in another process.
async fn spawn_child(
    command: &mut tokio::process::Command,
) -> std::io::Result<tokio::process::Child> {
    let mut tries = 0;
    loop {
        match command.spawn() {
            Err(e) if tries < 3 && e.raw_os_error() == Some(nix::errno::Errno::ETXTBSY as i32) => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => return other,
        }
    }
}

fn spawn_reason(e: &std::io::Error) -> String {
    if e.raw_os_error() == Some(nix::errno::Errno::ENOEXEC as i32) {
        return "it isn't a program the system can run. A script needs a first line such as #!/bin/sh".to_string();
    }
    io_reason(e)
}

/// Signal the script's process group. Best effort: by the time the
/// limit passes the group may be gone.
fn signal_group(group: Option<u32>, signal: nix::sys::signal::Signal) {
    let Some(pid) = group.and_then(|id| i32::try_from(id).ok()) else {
        return;
    };
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), signal);
}

/// Whether the script's process group has emptied by `deadline`. Only
/// for after the script itself has been waited for: this collects the
/// group's exited members, and must not collect the one tokio is
/// waiting on.
async fn group_gone_by(group: Option<u32>, deadline: Instant) -> bool {
    let Some(pgid) = group.and_then(|id| i32::try_from(id).ok()) else {
        return true;
    };
    loop {
        reap_orphans(pgid);
        // Signal 0 only asks whether anything is left to signal.
        let left = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None);
        if left == Err(nix::errno::Errno::ESRCH) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Collect the group's exited members that are Ryokan's own children.
/// They are only when Ryokan is the container's first process with no
/// init above it: what a script leaves behind is then handed to Ryokan,
/// and would stay in the process table as a zombie (and keep the group
/// looking alive) until something waited for it. Anywhere else there
/// is nothing to collect and this returns at once.
fn reap_orphans(pgid: i32) {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    let group = nix::unistd::Pid::from_raw(-pgid);
    while let Ok(status) = waitpid(group, Some(WaitPidFlag::WNOHANG)) {
        if status == WaitStatus::StillAlive {
            return;
        }
    }
}

/// The end of one output stream, read as the script writes it so a
/// chatty script never blocks on a full pipe.
struct Tail {
    kept: Arc<StdMutex<(Vec<u8>, bool)>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Tail {
    fn capture<R: AsyncRead + Unpin + Send + 'static>(stream: Option<R>) -> Self {
        let kept = Arc::new(StdMutex::new((Vec::new(), false)));
        let sink = kept.clone();
        let reader = tokio::spawn(async move {
            let Some(mut stream) = stream else { return };
            let mut chunk = [0u8; 4096];
            loop {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        let mut kept = sink.lock().unwrap();
                        kept.0.extend_from_slice(&chunk[..n]);
                        let over = kept.0.len().saturating_sub(OUTPUT_TAIL_BYTES);
                        if over > 0 {
                            kept.0.drain(..over);
                            kept.1 = true;
                        }
                    }
                }
            }
        });
        Self { kept, reader }
    }

    /// What was kept, once the stream closes or [`DRAIN_GRACE`] passes.
    async fn finish(mut self) -> String {
        if tokio::time::timeout(DRAIN_GRACE, &mut self.reader)
            .await
            .is_err()
        {
            self.reader.abort();
        }
        let kept = self.kept.lock().unwrap();
        let text = String::from_utf8_lossy(&kept.0);
        let text = text.trim();
        if kept.1 {
            format!("…{text}")
        } else {
            text.to_string()
        }
    }
}

// ── The queue ──────────────────────────────────────────────────────────────

/// One queued run. It carries its own database handle so the worker
/// logs where the event came from, and the provider's id rather than
/// its settings: the row is read again when the run comes up.
pub struct Job {
    pub db: SqlitePool,
    pub provider_id: i64,
    /// For the log, should the row be gone by then.
    pub provider_name: String,
    pub event_kind: &'static str,
    pub env: Vec<(String, String)>,
}

/// The process-wide queue's sender. `None` until the first run is
/// queued; replaced if its worker is gone.
static QUEUE: StdMutex<Option<mpsc::Sender<Job>>> = StdMutex::new(None);

/// A handle on a serial script queue: the process-wide one, or (in
/// tests) one of its own.
#[derive(Clone)]
pub struct Runner(RunnerKind);

#[derive(Clone)]
enum RunnerKind {
    Global,
    Own(mpsc::Sender<Job>),
}

impl Runner {
    /// The process-wide queue every provider shares.
    pub fn global() -> Self {
        Self(RunnerKind::Global)
    }

    /// A queue of its own with room for `capacity` waiting runs.
    pub fn start(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        tokio::spawn(worker(rx));
        Self(RunnerKind::Own(tx))
    }

    /// Queue a run. `Err` when the queue is full: the run is dropped.
    pub fn submit(&self, job: Job) -> Result<(), String> {
        let sent = match &self.0 {
            RunnerKind::Own(tx) => tx.try_send(job),
            RunnerKind::Global => {
                let mut queue = QUEUE.lock().unwrap();
                if queue.as_ref().is_none_or(|tx| tx.is_closed()) {
                    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
                    tokio::spawn(worker(rx));
                    *queue = Some(tx);
                }
                queue
                    .as_ref()
                    .expect("the queue was just started")
                    .try_send(job)
            }
        };
        sent.map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => {
                "the script queue is full, so this run was dropped".to_string()
            }
            mpsc::error::TrySendError::Closed(_) => "the script runner has stopped".to_string(),
        })
    }
}

/// One run at a time, in the order queued. Each run is its own task so
/// a panic in one is not the end of the queue.
async fn worker(mut jobs: mpsc::Receiver<Job>) {
    while let Some(job) = jobs.recv().await {
        let _ = tokio::spawn(run_job(job)).await;
    }
}

/// Why a queued run did not start.
enum NotRun {
    /// The user took the provider away since the event fired.
    Dropped(&'static str),
    /// Something is wrong that the user should see.
    Refused(String),
}

/// The provider's settings as they are now. A pack queues a run per
/// file; deleting or disabling the provider, unticking the event or
/// saving another path has to reach the runs still waiting, or the
/// only way to stop a misbehaving script would be a restart.
async fn current_config(job: &Job) -> Result<ScriptConfig, NotRun> {
    let row = super::store::get_provider(&job.db, job.provider_id)
        .await
        .map_err(|e| NotRun::Refused(format!("couldn't read the provider: {e}")))?
        .ok_or(NotRun::Dropped("the provider was deleted"))?;
    if !row.enabled {
        return Err(NotRun::Dropped("the provider was disabled"));
    }
    if row.kind != "script" {
        return Err(NotRun::Dropped("the provider is no longer a script"));
    }
    let events = super::store::matrix_for_provider(&job.db, job.provider_id)
        .await
        .map_err(|e| NotRun::Refused(format!("couldn't read the provider's events: {e}")))?;
    if !events.get(job.event_kind).copied().unwrap_or(false) {
        return Err(NotRun::Dropped("the event was turned off for the provider"));
    }
    serde_json::from_str(&row.config_json)
        .map_err(|e| NotRun::Refused(format!("invalid script config_json: {e}")))
}

async fn run_job(job: Job) {
    let who = format!("provider={} event={}", job.provider_name, job.event_kind);
    let category = LogCategory::Notifications;
    // The switch and the path are checked here as well as on save: the
    // row may predate the switch, and the file can have changed.
    let checked = if !enabled() {
        Err(NotRun::Refused(disabled_message()))
    } else {
        match current_config(&job).await {
            Ok(config) => check_path(&job.db, &config.path)
                .await
                .map(|path| (path, config.timeout()))
                .map_err(NotRun::Refused),
            Err(not_run) => Err(not_run),
        }
    };
    let (path, timeout) = match checked {
        Ok(ready) => ready,
        Err(NotRun::Dropped(reason)) => {
            let detail = format!("{who} reason={reason}");
            logger::info(&job.db, category, "Script run dropped", &detail).await;
            return;
        }
        Err(NotRun::Refused(reason)) => {
            let detail = format!("{who} reason={reason}");
            logger::warn(&job.db, category, "Script not run", &detail).await;
            return;
        }
    };
    match run(&path, &job.env, timeout).await {
        Ok(output) => log_output(&job.db, &who, &output).await,
        Err(reason) => {
            let detail = format!("{who} reason={reason}");
            logger::warn(&job.db, category, "Script not run", &detail).await;
        }
    }
}

/// One row in System → Logs per run: the exit code, how long it took,
/// and the end of what it wrote (stdout too when it failed).
async fn log_output(db: &SqlitePool, who: &str, output: &ScriptOutput) {
    let took = format!("{:.1}s", output.elapsed.as_secs_f64());
    let mut detail = match (output.timed_out, output.exit_code) {
        (true, _) => format!("{who} stopped_after={took}"),
        (false, Some(code)) => format!("{who} exit={code} took={took}"),
        (false, None) => format!("{who} took={took}"),
    };
    if !output.stderr.is_empty() {
        detail.push_str(&format!(
            " stderr={}",
            tail(&output.stderr, OUTPUT_LOG_CHARS)
        ));
    }
    if !output.succeeded() && !output.stdout.is_empty() {
        detail.push_str(&format!(
            " stdout={}",
            tail(&output.stdout, OUTPUT_LOG_CHARS)
        ));
    }
    let category = LogCategory::Notifications;
    if output.succeeded() {
        logger::info(db, category, "Script finished", &detail).await;
    } else if output.timed_out {
        logger::warn(db, category, "Script timed out", &detail).await;
    } else if output.exit_code.is_some() {
        logger::warn(db, category, "Script failed", &detail).await;
    } else {
        logger::warn(db, category, "Script ended by a signal", &detail).await;
    }
}

// ── The provider ───────────────────────────────────────────────────────────

/// One configured script. It holds no settings: a run reads the row
/// when it comes up ([`current_config`]).
pub struct ScriptProvider {
    id: i64,
    name: String,
    db: SqlitePool,
    runner: Runner,
}

impl ScriptProvider {
    /// From a `notification_providers` row. Only the config's shape is
    /// checked here; the file itself is checked before each run, so a
    /// script that is missing while Ryokan starts is not dropped for
    /// good.
    pub fn from_row(
        id: i64,
        name: String,
        config_json: &str,
        db: SqlitePool,
    ) -> Result<Self, String> {
        let config: ScriptConfig = serde_json::from_str(config_json)
            .map_err(|e| format!("invalid script config_json: {e}"))?;
        if !Path::new(config.path.trim()).is_absolute() {
            return Err("script path is not a full path".into());
        }
        Ok(Self {
            id,
            name,
            db,
            runner: Runner::global(),
        })
    }

    #[cfg(test)]
    fn with_runner(mut self, runner: Runner) -> Self {
        self.runner = runner;
        self
    }
}

#[async_trait]
impl NotificationProvider for ScriptProvider {
    fn id(&self) -> i64 {
        self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "script"
    }

    /// Queues the run and returns; the worker logs how it ended.
    async fn send(&self, event: &NotificationEvent) -> Result<(), String> {
        self.runner.submit(Job {
            db: self.db.clone(),
            provider_id: self.id,
            provider_name: self.name.clone(),
            event_kind: event.kind(),
            env: event_env(event),
        })
    }
}

/// The Settings Test button: check the path, run the script once with
/// `ryokan_eventtype=Test`, and wait for it (up to [`TEST_TIMEOUT`]).
/// It does not go through the queue, so the answer is not held up
/// behind a real run.
pub async fn run_test(db: &SqlitePool, config: &ScriptConfig) -> Result<ScriptOutput, String> {
    if !enabled() {
        return Err(disabled_message());
    }
    let path = check_path(db, &config.path).await?;
    // In a task of its own: a closed tab drops the request, and the run
    // has to go on to its end or its limit either way, so the script is
    // stopped the way `run` stops it and never left running.
    tokio::spawn(async move { run(&path, &test_env(), TEST_TIMEOUT).await })
        .await
        .map_err(|e| format!("the test run stopped unexpectedly: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_memory_pool;
    use std::os::unix::fs::PermissionsExt;

    /// A shell script in `dir`, executable unless `mode` says otherwise.
    fn write_script(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn imported() -> NotificationEvent {
        NotificationEvent::Imported {
            series_id: 7,
            series_title: "Sousou no Frieren".into(),
            episode_number: 3,
            source_path: "/downloads/[G] Frieren - 03.mkv".into(),
            dest_path: "/media/Frieren/Season 01/Frieren - S01E03.mkv".into(),
            quality_tag: "WEB-DL 1080p".into(),
        }
    }

    /// The file's text once a whole line is in it (a script creates
    /// the file a moment before it writes to it).
    async fn wait_for_line(path: &Path) -> String {
        for _ in 0..500 {
            match std::fs::read_to_string(path) {
                Ok(text) if text.ends_with('\n') => return text,
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        panic!("{} was never written", path.display());
    }

    async fn wait_until_exists(path: &Path) {
        for _ in 0..500 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{} never appeared", path.display());
    }

    async fn wait_for_log_rows(db: &SqlitePool, count: usize) -> Vec<(String, String, String)> {
        for _ in 0..500 {
            let rows = log_rows(db).await;
            if rows.len() >= count {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("fewer than {count} log rows were written");
    }

    async fn log_rows(db: &SqlitePool) -> Vec<(String, String, String)> {
        sqlx::query_as("SELECT level, message, COALESCE(detail, '') FROM logs ORDER BY id")
            .fetch_all(db)
            .await
            .unwrap()
    }

    // ── the switch ──

    #[test]
    fn scripts_are_on_unless_the_variable_says_no() {
        for on in [
            None,
            Some(""),
            Some("  "),
            Some("1"),
            Some("TRUE"),
            Some("yes"),
            Some("on"),
        ] {
            assert_eq!(read_switch(on), (true, None), "{on:?}");
        }
        for off in ["0", "false", "No", " off "] {
            assert_eq!(read_switch(Some(off)), (false, None), "{off:?}");
        }
    }

    #[test]
    fn a_value_that_is_not_a_yes_or_no_turns_scripts_off_with_a_warning() {
        let (on, warning) = read_switch(Some("disabled"));
        assert!(!on, "fails closed");
        let warning = warning.expect("says why");
        assert!(
            warning.contains("RYOKAN_CUSTOM_SCRIPTS=\"disabled\""),
            "{warning}"
        );
        assert!(warning.contains("custom scripts are off"), "{warning}");
    }

    // ── the path check ──

    #[test]
    fn an_executable_file_of_the_users_own_passes() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "post.sh", "exit 0", 0o755);
        let resolved = validate_path(script.to_str().unwrap(), &PathPolicy::default()).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&script).unwrap());
    }

    #[test]
    fn a_path_has_to_be_absolute_present_a_file_and_executable() {
        let dir = tempfile::tempdir().unwrap();
        let policy = PathPolicy::default();
        let err = |path: &str| validate_path(path, &policy).unwrap_err();

        assert_eq!(err("  "), "Script path is required");
        assert!(err("scripts/post.sh").contains("must be a full path"));
        let missing = dir.path().join("nope.sh");
        assert!(err(missing.to_str().unwrap()).contains("no such file"));
        assert!(err(dir.path().to_str().unwrap()).contains("is not a file"));
        let plain = write_script(dir.path(), "plain.sh", "exit 0", 0o644);
        assert!(err(plain.to_str().unwrap()).contains("isn't executable"));
    }

    #[test]
    fn a_system_binary_is_refused_directly_and_through_a_link() {
        let policy = PathPolicy::default();
        let direct = validate_path("/bin/sh", &policy).unwrap_err();
        assert!(direct.contains("system folder"), "{direct}");

        // A link in the user's own folder is the file it points at.
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("totally-mine");
        std::os::unix::fs::symlink("/bin/sh", &link).unwrap();
        let linked = validate_path(link.to_str().unwrap(), &policy).unwrap_err();
        assert!(linked.contains("system folder"), "{linked}");
    }

    #[test]
    fn nothing_under_the_library_or_the_recycle_bin_is_run() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("media");
        let bin = dir.path().join("recycle");
        std::fs::create_dir_all(library.join("Show")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let policy = PathPolicy {
            media_root: library.to_str().unwrap().into(),
            recycle_bin: bin.to_str().unwrap().into(),
            ..Default::default()
        };

        let in_library = write_script(&library.join("Show"), "x.sh", "exit 0", 0o755);
        let err = validate_path(in_library.to_str().unwrap(), &policy).unwrap_err();
        assert!(err.contains("inside the media library"), "{err}");

        let in_bin = write_script(&bin, "x.sh", "exit 0", 0o755);
        let err = validate_path(in_bin.to_str().unwrap(), &policy).unwrap_err();
        assert!(err.contains("inside the recycle bin"), "{err}");

        // A sibling folder whose name merely starts the same is fine.
        let beside = dir.path().join("media-scripts");
        std::fs::create_dir_all(&beside).unwrap();
        let ok = write_script(&beside, "x.sh", "exit 0", 0o755);
        assert!(validate_path(ok.to_str().unwrap(), &policy).is_ok());
    }

    // ── the environment ──

    #[test]
    fn an_event_becomes_lower_case_ryokan_variables() {
        let env: std::collections::HashMap<String, String> =
            event_env(&imported()).into_iter().collect();
        assert_eq!(env["ryokan_eventtype"], "Imported");
        assert_eq!(env["ryokan_series_id"], "7");
        assert_eq!(env["ryokan_series_title"], "Sousou no Frieren");
        assert_eq!(env["ryokan_episode_number"], "3");
        assert_eq!(env["ryokan_source_path"], "/downloads/[G] Frieren - 03.mkv");
        assert_eq!(
            env["ryokan_dest_path"],
            "/media/Frieren/Season 01/Frieren - S01E03.mkv"
        );
        assert_eq!(env["ryokan_quality_tag"], "WEB-DL 1080p");
        assert_eq!(env.len(), 7);
    }

    #[test]
    fn a_missing_value_is_an_empty_variable_and_a_list_is_joined() {
        let failed: std::collections::HashMap<String, String> =
            event_env(&NotificationEvent::ImportFailed {
                series_id: 1,
                series_title: "S".into(),
                episode_number: None,
                source_path: "/d/x.mkv".into(),
                reason: "not a video".into(),
            })
            .into_iter()
            .collect();
        assert_eq!(failed["ryokan_episode_number"], "", "set, and empty");

        let misgrab: std::collections::HashMap<String, String> =
            event_env(&NotificationEvent::Misgrabbed {
                series_id: 1,
                series_title: "S".into(),
                release_title: "R".into(),
                hash: "h".into(),
                files: vec!["a.mkv".into(), "b.mkv".into()],
                action: "removed".into(),
            })
            .into_iter()
            .collect();
        assert_eq!(misgrab["ryokan_files"], "a.mkv|b.mkv");
    }

    // ── running ──

    #[tokio::test]
    async fn a_run_gets_the_event_no_arguments_and_its_own_folder() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "post.sh",
            r#"echo "args=$# type=$ryokan_eventtype dest=$ryokan_dest_path key=${RYOKAN_ENCRYPTION_KEY:-none}"
pwd
echo "to stderr" >&2"#,
            0o755,
        );
        // Ryokan's own key never reaches the script, even if something
        // put it in the run's variables.
        let mut env = event_env(&imported());
        env.push(("RYOKAN_ENCRYPTION_KEY".into(), "c2VjcmV0".into()));
        let output = run(&script, &env, Duration::from_secs(20)).await.unwrap();

        assert!(output.succeeded(), "{output:?}");
        let mut lines = output.stdout.lines();
        assert_eq!(
            lines.next().unwrap(),
            "args=0 type=Imported dest=/media/Frieren/Season 01/Frieren - S01E03.mkv key=none"
        );
        assert_eq!(
            Path::new(lines.next().unwrap()),
            std::fs::canonicalize(dir.path()).unwrap()
        );
        assert_eq!(output.stderr, "to stderr");
        assert_eq!(output.said(), "to stderr");
    }

    #[tokio::test]
    async fn a_failing_script_reports_its_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "bad.sh", "echo nope >&2\nexit 3", 0o755);
        let output = run(&script, &test_env(), Duration::from_secs(20))
            .await
            .unwrap();
        assert_eq!(output.exit_code, Some(3));
        assert!(!output.succeeded());
        assert_eq!(output.stderr, "nope");
    }

    #[tokio::test]
    async fn a_script_past_its_limit_is_stopped_with_what_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        // The child would write the marker long after the limit.
        let script = write_script(
            dir.path(),
            "slow.sh",
            &format!(
                "(sleep 1; echo late > '{}') &\necho started\nsleep 30",
                marker.display()
            ),
            0o755,
        );
        let started = Instant::now();
        let output = run(&script, &test_env(), Duration::from_millis(300))
            .await
            .unwrap();
        assert!(output.timed_out);
        assert!(!output.succeeded());
        assert_eq!(output.stdout, "started");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "stopped at the limit, not at the script's own end"
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!marker.exists(), "the background child went with it");
    }

    #[tokio::test]
    async fn only_the_end_of_a_long_output_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "chatty.sh",
            "i=0\nwhile [ $i -lt 2000 ]; do echo \"line $i of many\"; i=$((i+1)); done\necho last",
            0o755,
        );
        let output = run(&script, &test_env(), Duration::from_secs(20))
            .await
            .unwrap();
        assert!(output.succeeded());
        assert!(output.stdout.starts_with('…'), "marked as cut");
        assert!(output.stdout.ends_with("last"));
        assert!(output.stdout.len() <= OUTPUT_TAIL_BYTES + 8);
    }

    #[tokio::test]
    async fn a_file_that_is_not_a_program_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "just some text\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        match run(&path, &test_env(), Duration::from_secs(5)).await {
            Err(e) => assert!(e.contains("#!/bin/sh"), "{e}"),
            // Some C libraries hand such a file to /bin/sh, which then
            // fails on its first word.
            Ok(output) => assert!(!output.succeeded(), "{output:?}"),
        }
    }

    // ── the queue and the provider ──

    /// A saved, enabled script provider with Imported ticked, as the
    /// Add form saves one.
    async fn save_provider(db: &SqlitePool, id: i64, script: &Path) {
        sqlx::query(
            "INSERT INTO notification_providers (id, name, kind, enabled, config_json)
             VALUES (?, 'post', 'script', 1, ?)",
        )
        .bind(id)
        .bind(serde_json::json!({"path": script, "timeout_secs": 60}).to_string())
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO notification_settings (provider_id, event_kind, enabled)
             VALUES (?, 'Imported', 1)",
        )
        .bind(id)
        .execute(db)
        .await
        .unwrap();
    }

    fn job(db: &SqlitePool, provider_id: i64, env: Vec<(String, String)>) -> Job {
        Job {
            db: db.clone(),
            provider_id,
            provider_name: "post".into(),
            event_kind: "Imported",
            env,
        }
    }

    #[tokio::test]
    async fn runs_are_serial_and_in_the_order_queued() {
        let db = in_memory_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let trace = dir.path().join("trace");
        // Each run brackets a pause; overlapping runs would interleave.
        let script = write_script(
            dir.path(),
            "post.sh",
            &format!(
                "echo \"start $n\" >> '{0}'\nsleep 0.3\necho \"end $n\" >> '{0}'\n[ \"$n\" = 3 ] && touch '{0}.done'\nexit 0",
                trace.display()
            ),
            0o755,
        );
        save_provider(&db, 1, &script).await;
        let runner = Runner::start(8);
        for n in 1..=3 {
            runner
                .submit(job(&db, 1, vec![("n".into(), n.to_string())]))
                .unwrap();
        }
        wait_until_exists(&dir.path().join("trace.done")).await;
        assert_eq!(
            std::fs::read_to_string(&trace).unwrap(),
            "start 1\nend 1\nstart 2\nend 2\nstart 3\nend 3\n"
        );
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_run_and_says_so() {
        let db = in_memory_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "slow.sh", "sleep 1", 0o755);
        save_provider(&db, 1, &script).await;
        let runner = Runner::start(1);
        // One running (or about to), one waiting; the rest have no room.
        let mut refused = None;
        for _ in 0..4 {
            if let Err(e) = runner.submit(job(&db, 1, vec![])) {
                refused = Some(e);
            }
        }
        assert!(refused.expect("no room").contains("queue is full"));
    }

    #[tokio::test]
    async fn every_run_leaves_a_log_row_with_its_exit_code_and_stderr() {
        let db = in_memory_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let done = dir.path().join("done");
        let ok = write_script(dir.path(), "ok.sh", "exit 0", 0o755);
        let bad = write_script(
            dir.path(),
            "bad.sh",
            &format!(
                "echo out\necho 'no such codec' >&2\ntouch '{}'\nexit 2",
                done.display()
            ),
            0o755,
        );
        let gone = dir.path().join("gone.sh");
        save_provider(&db, 1, &ok).await;
        save_provider(&db, 2, &gone).await;
        save_provider(&db, 3, &bad).await;
        let runner = Runner::start(8);
        for id in 1..=3 {
            runner.submit(job(&db, id, vec![])).unwrap();
        }
        wait_until_exists(&done).await;
        // The last row is written after its script exits.
        let rows = wait_for_log_rows(&db, 3).await;

        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(
            (rows[0].0.as_str(), rows[0].1.as_str()),
            ("info", "Script finished")
        );
        assert!(
            rows[0]
                .2
                .starts_with("provider=post event=Imported exit=0 took="),
            "{}",
            rows[0].2
        );

        assert_eq!(
            (rows[1].0.as_str(), rows[1].1.as_str()),
            ("warn", "Script not run")
        );
        assert!(rows[1].2.contains("no such file"), "{}", rows[1].2);

        assert_eq!(
            (rows[2].0.as_str(), rows[2].1.as_str()),
            ("warn", "Script failed")
        );
        assert!(rows[2].2.contains("exit=2"), "{}", rows[2].2);
        assert!(rows[2].2.contains("stderr=no such codec"), "{}", rows[2].2);
        assert!(rows[2].2.contains("stdout=out"), "{}", rows[2].2);
    }

    #[tokio::test]
    async fn the_path_is_checked_again_before_each_run() {
        // Saved while it was fine, then replaced by a link to a system
        // binary: the run is refused, not started.
        let db = in_memory_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("post.sh");
        std::os::unix::fs::symlink("/bin/sh", &script).unwrap();
        save_provider(&db, 1, &script).await;
        let runner = Runner::start(8);
        runner.submit(job(&db, 1, vec![])).unwrap();
        let rows = wait_for_log_rows(&db, 1).await;
        assert_eq!(rows[0].1, "Script not run");
        assert!(rows[0].2.contains("system folder"), "{}", rows[0].2);
    }

    #[tokio::test]
    async fn send_returns_at_once_and_the_run_outlives_the_provider() {
        // The provider objects are rebuilt on every settings save. A
        // run queued through one that is then dropped still happens.
        let db = in_memory_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let script = write_script(
            dir.path(),
            "post.sh",
            &format!(
                "sleep 0.5\necho \"$ryokan_eventtype $ryokan_episode_number\" > '{}'",
                out.display()
            ),
            0o755,
        );
        save_provider(&db, 1, &script).await;
        let config = serde_json::json!({"path": script.to_str().unwrap()}).to_string();
        let provider = ScriptProvider::from_row(1, "post".into(), &config, db.clone())
            .unwrap()
            .with_runner(Runner::start(8));
        assert_eq!(provider.kind(), "script");

        let started = Instant::now();
        provider.send(&imported()).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "send queues; it does not wait for the script"
        );
        drop(provider);
        assert_eq!(wait_for_line(&out).await, "Imported 3\n");
    }

    #[tokio::test]
    async fn deleting_the_provider_stops_the_runs_still_waiting() {
        // A pack queued three runs. The first is under way when the
        // user deletes the provider; the other two must not start.
        let db = in_memory_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let trace = dir.path().join("trace");
        let script = write_script(
            dir.path(),
            "post.sh",
            &format!("echo run >> '{}'\nsleep 0.4", trace.display()),
            0o755,
        );
        save_provider(&db, 1, &script).await;
        let runner = Runner::start(8);
        for _ in 0..3 {
            runner.submit(job(&db, 1, vec![])).unwrap();
        }
        wait_for_line(&trace).await;
        sqlx::query("DELETE FROM notification_providers WHERE id = 1")
            .execute(&db)
            .await
            .unwrap();

        let rows = wait_for_log_rows(&db, 3).await;
        let messages: Vec<&str> = rows.iter().map(|r| r.1.as_str()).collect();
        assert_eq!(
            messages,
            vec![
                "Script finished",
                "Script run dropped",
                "Script run dropped"
            ]
        );
        assert!(
            rows[1].2.contains("the provider was deleted"),
            "{}",
            rows[1].2
        );
        assert_eq!(std::fs::read_to_string(&trace).unwrap(), "run\n");
    }

    #[tokio::test]
    async fn a_waiting_run_follows_the_provider_as_it_is_now() {
        let db = in_memory_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let old = write_script(dir.path(), "old.sh", "exit 9", 0o755);
        let new = write_script(
            dir.path(),
            "new.sh",
            &format!("echo new > '{}'", out.display()),
            0o755,
        );
        save_provider(&db, 1, &old).await;
        // Re-pointed after the event fired: the run uses the new path.
        sqlx::query("UPDATE notification_providers SET config_json = ? WHERE id = 1")
            .bind(serde_json::json!({"path": new}).to_string())
            .execute(&db)
            .await
            .unwrap();
        let runner = Runner::start(8);
        runner.submit(job(&db, 1, vec![])).unwrap();
        assert_eq!(wait_for_line(&out).await, "new\n");

        // Disabled, or the event unticked: dropped, with the reason.
        for (change, reason) in [
            (
                "UPDATE notification_settings SET enabled = 0 WHERE provider_id = 1",
                "the event was turned off",
            ),
            (
                "UPDATE notification_providers SET enabled = 0 WHERE id = 1",
                "the provider was disabled",
            ),
        ] {
            sqlx::query(change).execute(&db).await.unwrap();
            let before = log_rows(&db).await.len();
            runner.submit(job(&db, 1, vec![])).unwrap();
            let rows = wait_for_log_rows(&db, before + 1).await;
            let last = rows.last().unwrap();
            assert_eq!(last.1, "Script run dropped");
            assert!(last.2.contains(reason), "{}", last.2);
        }
    }

    #[tokio::test]
    async fn what_the_script_started_gets_the_grace_to_finish_up() {
        // The shell dies on SIGTERM at once. The job it started traps
        // the signal and needs a moment to write its file; it is given
        // the grace, not killed with the shell.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("cleaned");
        let script = write_script(
            dir.path(),
            "wrapper.sh",
            &format!(
                "(trap 'sleep 0.3; echo done > \"{}\"; exit 0' TERM; while :; do sleep 0.05; done) &\necho started\nsleep 30",
                marker.display()
            ),
            0o755,
        );
        let started = Instant::now();
        let output = run_with_grace(
            &script,
            &test_env(),
            Duration::from_millis(300),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(output.timed_out);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "done\n");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the wait ends when the group is gone, not at the full grace"
        );
    }

    #[tokio::test]
    async fn a_job_that_ignores_the_grace_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let script = write_script(
            dir.path(),
            "stubborn.sh",
            &format!(
                "(trap '' TERM; sleep 1; echo late > '{}') &\nsleep 30",
                marker.display()
            ),
            0o755,
        );
        let output = run_with_grace(
            &script,
            &test_env(),
            Duration::from_millis(200),
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        assert!(output.timed_out);
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(!marker.exists(), "killed once the grace ran out");
    }

    #[tokio::test]
    async fn a_file_ryokan_downloaded_is_never_run() {
        // The download folder is not in the settings, and the torrent
        // marked its file executable. The grab's recorded content path
        // is what gives it away, for the file and for anything inside a
        // downloaded folder.
        let db = in_memory_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("downloads");
        let pack = downloads.join("Some.Pack");
        std::fs::create_dir_all(&pack).unwrap();
        let single = write_script(&downloads, "release.mkv", "exit 0", 0o755);
        let inside = write_script(&pack, "extras.sh", "exit 0", 0o755);
        let mine = write_script(dir.path(), "mine.sh", "exit 0", 0o755);
        let series = crate::test_support::seed_series(&db, 1, "Show").await;
        for (hash, content) in [("a", &single), ("b", &pack)] {
            let grab =
                crate::test_support::seed_grabbed_torrent(&db, series, hash, "x", &[1]).await;
            crate::models::grabbed_torrents::stamp_client_content_path(
                &db,
                grab,
                content.to_str().unwrap(),
            )
            .await
            .unwrap();
        }

        for refused in [&single, &inside] {
            let err = check_path(&db, refused.to_str().unwrap())
                .await
                .unwrap_err();
            assert!(err.contains("part of a download"), "{err}");
        }
        assert!(check_path(&db, mine.to_str().unwrap()).await.is_ok());

        // A download client's configured folder is refused outright.
        let policy = PathPolicy {
            download_folders: vec![downloads.to_str().unwrap().into()],
            ..Default::default()
        };
        let other = write_script(&downloads, "not-a-grab.sh", "exit 0", 0o755);
        let err = validate_path(other.to_str().unwrap(), &policy).unwrap_err();
        assert!(err.contains("inside a download folder"), "{err}");
    }

    #[test]
    fn a_long_output_keeps_its_end_and_a_long_value_is_cut() {
        assert_eq!(tail("short", 10), "short");
        assert_eq!(
            tail("banner banner Conversion failed!", 18),
            "…Conversion failed!"
        );
        // Already marked as cut by the byte cap: one mark, not two.
        assert_eq!(tail("…abcdef", 3), "…def");

        let files: Vec<serde_json::Value> = (0..5000)
            .map(|i| serde_json::Value::String(format!("Season 01/Episode {i:04} 日本語.mkv")))
            .collect();
        let value = env_value(&serde_json::Value::Array(files));
        assert!(value.len() <= ENV_VALUE_MAX_BYTES);
        assert!(value.starts_with("Season 01/Episode 0000"));
    }

    #[tokio::test]
    async fn a_row_with_a_relative_path_or_a_broken_config_is_not_built() {
        let db = in_memory_pool().await;
        let relative = ScriptProvider::from_row(1, "p".into(), r#"{"path":"x.sh"}"#, db.clone());
        assert!(relative.is_err());
        assert!(ScriptProvider::from_row(1, "p".into(), "{}", db).is_err());
    }

    #[test]
    fn the_time_limit_is_clamped_to_a_day() {
        let config = |secs| ScriptConfig {
            path: "/x".into(),
            timeout_secs: secs,
        };
        assert_eq!(config(90).timeout_minutes(), 2);
        assert_eq!(config(0).timeout(), Duration::from_secs(1));
        assert_eq!(config(u64::MAX).timeout_minutes(), MAX_TIMEOUT_MINUTES);
    }
}
