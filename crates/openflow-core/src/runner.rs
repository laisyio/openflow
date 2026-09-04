//! The local transcription runner: a supervised MLX sidecar on loopback.
//!
//! The engine choice is the benchmark's
//! (`docs/native-port/local-runner-benchmark.md`): MLX Qwen3-ASR, 0.40 s for an
//! 8.7 s clip against 1.8 s for whisper.cpp and 1.7 s for Groq. MLX needs a
//! Python runtime and about 600 MB of packages, which the app does *not* bundle
//! -- tripling the download for everyone to serve the users who want local
//! transcription is the wrong trade. So this module finds a Python the user
//! already has, builds a virtualenv beside the database on demand, downloads the
//! weights on demand, and supervises `runner/runner.py` as a child process.
//!
//! What it owns:
//!
//! - **Interpreter discovery.** Homebrew and python.org locations plus whatever
//!   `python3` resolves to, filtered to 3.10 or newer. Never bundled, and when
//!   none is found the state says exactly what to install.
//! - **Install** (`python -m venv` + `pip install mlx-audio`), about 600 MB, on
//!   demand, with the installer's own output as progress.
//! - **Download** (`huggingface_hub.snapshot_download` inside the venv, default
//!   cache), on demand, with progress. Weights already in the standard cache are
//!   found rather than fetched again.
//! - **Spawn** on a free loopback port, readiness by polling `/health`, restart
//!   on crash with backoff, `failed` after three restarts inside a minute, and a
//!   kill on drop and on quit.
//! - **`prewarm`**, which the engine calls when recording starts so the ~3 s
//!   model load overlaps the user speaking.
//!
//! Every state change is pushed to the host as [`EngineEvent::RunnerState`].
//! Nothing here asks the UI to poll, and the UI is not allowed to.
//!
//! ## Why the child is supervised by threads and signals
//!
//! `Child::wait` needs `&mut Child`, so holding the child in a mutex would mean
//! either holding that mutex for the whole life of the process (and blocking
//! every kill) or polling `try_wait` (a timer, on the idle path, which this port
//! exists to remove). Instead the monitor thread owns the `Child` and blocks in
//! `wait`, and the supervisor keeps only the pid and stops the process by
//! signalling it. A generation counter, bumped on every intentional stop, is how
//! the monitor tells "it crashed" from "we killed it".

use crate::engine::{EngineEvent, EngineEvents};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// The `mlx-audio` release the runner is written against and the benchmark was
/// measured on. Pinned: a transcription backend that changes under the user
/// between launches is not one they can trust.
pub const MLX_AUDIO_VERSION: &str = "0.5.1";

/// The oldest Python `mlx-audio` supports.
pub const MINIMUM_PYTHON: (u32, u32) = (3, 10);

/// How long a spawned sidecar has to answer `/health`.
///
/// Generous because the first answer costs importing mlx and mlx-audio, which
/// is seconds on a cold file cache. The model load is *not* inside this window:
/// `/health` answers `loading` while it happens.
pub const READY_TIMEOUT: Duration = Duration::from_secs(45);

/// A crash inside this window counts towards [`MAX_RESTARTS`].
pub const RESTART_WINDOW: Duration = Duration::from_secs(60);

/// Restarts allowed inside [`RESTART_WINDOW`] before the runner reports failed.
///
/// Three, because the failures worth retrying (a port that was taken between
/// picking it and binding it, an OOM kill under momentary memory pressure) do
/// not repeat four times in a minute, and the ones that are not worth retrying
/// (a broken venv, a corrupt download) repeat forever.
pub const MAX_RESTARTS: usize = 3;

/// Backoff before each restart, longest last.
const BACKOFF: [Duration; MAX_RESTARTS] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_millis(1_000),
];

/// Lines of the sidecar's stderr kept for diagnosing a failure.
const LOG_LINES: usize = 40;

/// What an install or download reports when the user stopped it.
///
/// Not routed through [`RunnerPhase::Failed`]: a stop is the outcome the user
/// asked for, and `stop` has already written the line they read.
const SETUP_STOPPED: &str = "Setup was stopped.";

// ── Models ────────────────────────────────────────────────

/// One offered model, with the cost the Settings screen shows next to it. The
/// numbers are measured, from the benchmark, not estimated.
pub struct LocalModel {
    /// What the `local_model` setting stores.
    pub key: &'static str,
    /// The Hugging Face repo the sidecar loads.
    pub repo: &'static str,
    /// The menu title.
    pub label: &'static str,
    /// The cost, short enough to sit in the menu item beside the label.
    pub short_cost: &'static str,
    /// The sentence under the picker.
    pub cost: &'static str,
}

/// Accurate first: it is the default, because it keeps the proper nouns 0.6B
/// loses, and a dictation tool that mangles product names is not faster in any
/// sense the user cares about.
pub const LOCAL_MODELS: &[LocalModel] = &[
    LocalModel {
        key: "accurate",
        repo: "mlx-community/Qwen3-ASR-1.7B-8bit",
        label: "Accurate (1.7B)",
        short_cost: "2.5 GB, 1.0 s",
        cost:
            "About 2.5 GB of memory while loaded, 1.0 s for a 10 s dictation. Keeps product names.",
    },
    LocalModel {
        key: "fast",
        repo: "mlx-community/Qwen3-ASR-0.6B-8bit",
        label: "Fast (0.6B)",
        short_cost: "1.0 GB, 0.4 s",
        cost: "About 1.0 GB of memory while loaded, 0.4 s for a 10 s dictation. Weaker on names.",
    },
];

/// The model for a stored key, falling back to the default rather than leaving
/// the runner with nothing to load.
pub fn model_for(key: &str) -> &'static LocalModel {
    LOCAL_MODELS
        .iter()
        .find(|model| model.key == key)
        .unwrap_or(&LOCAL_MODELS[0])
}

// ── State ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerPhase {
    /// Not running, and nothing is wrong.
    Stopped,
    /// No Python 3.10+ on this Mac.
    MissingPython,
    /// Building the virtualenv and installing packages.
    Installing,
    /// Fetching model weights.
    Downloading,
    /// Spawned, waiting for `/health`.
    Starting,
    /// Answering on `port`.
    Ready,
    /// Gave up. `detail` says why.
    Failed,
}

impl RunnerPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::MissingPython => "missing_python",
            Self::Installing => "installing",
            Self::Downloading => "downloading",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    /// Whether something is in flight, so the UI can disable the buttons that
    /// would start a second one.
    pub fn is_busy(self) -> bool {
        matches!(self, Self::Installing | Self::Downloading | Self::Starting)
    }
}

/// Everything the UI shows about the runner, in one value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RunnerStatus {
    pub phase: RunnerPhase,
    /// One line for the user: progress while busy, the reason while failed.
    pub detail: String,
    pub port: Option<u16>,
    /// The repo id the sidecar was started with.
    pub model: String,
    /// What the sidecar reports it is holding, from `/health`: its resident set
    /// plus the unified memory MLX has allocated.
    ///
    /// Both halves are needed and neither is enough. Metal buffers do not show
    /// up in the process's RSS -- a loaded 0.6B measures 142 MB resident while
    /// MLX is holding 1.01 GB of weights -- so RSS alone would tell the user
    /// the model costs a seventh of what it costs, and MLX's number alone would
    /// miss the interpreter.
    pub resident_bytes: Option<u64>,
}

impl RunnerStatus {
    fn new(model: &str) -> Self {
        Self {
            phase: RunnerPhase::Stopped,
            detail: String::new(),
            port: None,
            model: model.to_string(),
            resident_bytes: None,
        }
    }
}

/// What to run, for a test that wants a fake sidecar in place of the real one.
#[derive(Clone, Debug)]
pub struct RunnerProgram {
    pub python: PathBuf,
    pub script: PathBuf,
    /// Extra arguments appended after the standard three.
    pub extra_args: Vec<String>,
    /// Whether the venv and the weights have to exist before a spawn.
    pub needs_install: bool,
}

#[derive(Deserialize)]
struct Health {
    state: String,
    #[serde(default)]
    resident_bytes: Option<u64>,
    #[serde(default)]
    active_memory_bytes: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

impl Health {
    /// Everything this sidecar is holding: see [`RunnerStatus::resident_bytes`].
    fn memory(&self) -> Option<u64> {
        match (self.resident_bytes, self.active_memory_bytes) {
            (None, None) => None,
            (resident, active) => Some(resident.unwrap_or(0).saturating_add(active.unwrap_or(0))),
        }
    }
}

struct Inner {
    status: RunnerStatus,
    /// The running sidecar, if any. Only the pid: the `Child` belongs to the
    /// monitor thread (see the module docs).
    pid: Option<u32>,
    /// Bumped on every intentional stop or reconfiguration, so a monitor thread
    /// for a process we killed knows not to restart it.
    generation: u64,
    /// Bumped on every spawn, restarts included. A readiness wait captures it
    /// and gives up the moment a newer launch exists: without it, the wait
    /// belonging to a process that has already crashed and been replaced
    /// eventually times out and writes its own failure over the supervisor's.
    launch_id: u64,
    /// When each unintentional exit happened, inside [`RESTART_WINDOW`].
    restarts: VecDeque<Instant>,
    model_key: String,
    idle_minutes: u64,
    log: VecDeque<String>,
    /// A start, install or download is already running on some thread.
    working: bool,
    /// A spawn is in flight and has not reached ready or failed yet.
    ///
    /// This is what makes `start` single-flight. Without it, `prewarm` on
    /// key-down and `ensure_ready` on key-up both saw "not ready" and both
    /// spawned: the second overwrote the first's pid, the first's monitor saw a
    /// generation mismatch and did nothing, and that sidecar went on to load a
    /// model and hold 1 to 2.5 GB until the app quit, reachable by nobody.
    starting: bool,
    /// Set once the venv has been proven importable, so the check is not a
    /// Python process on every key-up. Cleared by `install`.
    installed_verified: bool,
    /// The repo whose weights have been proven present, for the same reason.
    /// Cleared by `download` and by a model change in `configure`.
    model_verified: Option<String>,
    /// The pid of the install or download child that is running right now.
    ///
    /// Separate from `pid` because these are not the sidecar: `python -m venv`
    /// and a 600 MB `pip install` are spawned by `run_with_progress`, which
    /// owns the `Child` while it blocks reading its output. Same reason as the
    /// sidecar, then, for holding only the pid and stopping it by signal. Held
    /// here because `stop` is on a different thread from the install and had
    /// nothing to signal without it: pressing Stop during a download left pip
    /// running to completion.
    setup_pid: Option<u32>,
    /// Bumped on every stop, so a setup step knows the user asked for it to
    /// end. One install is several children in a row, and this is what makes a
    /// stop stick across the rest of them rather than killing one and letting
    /// the next start.
    setup_generation: u64,
}

pub struct LocalRunner {
    app_dir: PathBuf,
    events: Arc<dyn EngineEvents>,
    program: Option<RunnerProgram>,
    inner: Mutex<Inner>,
}

impl LocalRunner {
    pub fn new(app_dir: PathBuf, events: Arc<dyn EngineEvents>, model_key: &str) -> Arc<Self> {
        Self::with_program(app_dir, events, model_key, None)
    }

    pub fn with_program(
        app_dir: PathBuf,
        events: Arc<dyn EngineEvents>,
        model_key: &str,
        program: Option<RunnerProgram>,
    ) -> Arc<Self> {
        let repo = model_for(model_key).repo;
        Arc::new(Self {
            app_dir,
            events,
            program,
            inner: Mutex::new(Inner {
                status: RunnerStatus::new(repo),
                pid: None,
                generation: 0,
                launch_id: 0,
                restarts: VecDeque::new(),
                model_key: model_key.to_string(),
                idle_minutes: 10,
                log: VecDeque::new(),
                working: false,
                starting: false,
                installed_verified: false,
                model_verified: None,
                setup_pid: None,
                setup_generation: 0,
            }),
        })
    }

    // ── Paths ─────────────────────────────────────────────

    /// `<app dir>/runner/venv`, beside the database. Not in the bundle: a
    /// virtualenv hard-codes its own absolute path, so one inside `OpenFlow.app`
    /// would break the first time the app moved, and it would be discarded on
    /// every update.
    pub fn venv_dir(&self) -> PathBuf {
        self.app_dir.join("runner").join("venv")
    }

    pub fn venv_python(&self) -> PathBuf {
        self.venv_dir().join("bin").join("python3")
    }

    /// Where `runner.py` is, at run time.
    ///
    /// Inside the bundle it sits in `Contents/Resources/runner/`, which is
    /// `../Resources/runner` from the executable. In a `cargo run` or a test
    /// there is no bundle, so the source tree is the fallback -- found relative
    /// to this file rather than to the working directory, which a test does not
    /// control.
    pub fn script_path() -> Result<PathBuf, String> {
        if let Some(program) = std::env::var_os("OPENFLOW_RUNNER_SCRIPT") {
            let path = PathBuf::from(program);
            if path.is_file() {
                return Ok(path);
            }
        }
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(executable) = std::env::current_exe() {
            if let Some(directory) = executable.parent() {
                candidates.push(directory.join("../Resources/runner/runner.py"));
                candidates.push(directory.join("runner/runner.py"));
            }
        }
        // `crates/openflow-core/src` -> `crates/openflow-native/runner`.
        candidates.push(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../openflow-native/runner/runner.py"),
        );
        for candidate in candidates {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        Err("Could not find runner.py. Reinstall OpenFlow.".to_string())
    }

    // ── Interpreter discovery ─────────────────────────────

    /// A Python 3.10+ on this Mac, or nothing.
    ///
    /// `OPENFLOW_PYTHON` wins, then the Homebrew and python.org install
    /// locations newest first, then whatever `python3` means on the PATH. The
    /// macOS system `python3` is 3.9 on current releases and fails the version
    /// check rather than being special-cased.
    pub fn find_python() -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(explicit) = std::env::var_os("OPENFLOW_PYTHON") {
            candidates.push(PathBuf::from(explicit));
        }
        for version in ["3.14", "3.13", "3.12", "3.11", "3.10"] {
            candidates.push(PathBuf::from(format!("/opt/homebrew/bin/python{version}")));
            candidates.push(PathBuf::from(format!("/usr/local/bin/python{version}")));
            candidates.push(PathBuf::from(format!(
                "/Library/Frameworks/Python.framework/Versions/{version}/bin/python3"
            )));
        }
        candidates.push(PathBuf::from("python3"));
        candidates.into_iter().find(|candidate| {
            python_version(candidate)
                .map(|version| version >= MINIMUM_PYTHON)
                .unwrap_or(false)
        })
    }

    /// What to tell a user with no usable Python.
    pub fn missing_python_message() -> String {
        format!(
            "On-device transcription needs Python {}.{} or newer, which this Mac does not have. Install it with `brew install python@3.12`, or from python.org, then press Install again.",
            MINIMUM_PYTHON.0, MINIMUM_PYTHON.1
        )
    }

    // ── Status ────────────────────────────────────────────

    pub fn status(&self) -> RunnerStatus {
        self.lock().status.clone()
    }

    /// The port a *ready* sidecar is answering on. `None` means "not now", and
    /// never blocks: the preview loop uses this so a reading is skipped rather
    /// than queued behind a model load.
    pub fn ready_port(&self) -> Option<u16> {
        let inner = self.lock();
        if inner.status.phase == RunnerPhase::Ready {
            inner.status.port
        } else {
            None
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.lock().pid
    }

    pub fn model_repo(&self) -> String {
        model_for(&self.lock().model_key).repo.to_string()
    }

    /// The last lines the sidecar wrote to stderr, for a failure report.
    pub fn log(&self) -> Vec<String> {
        self.lock().log.iter().cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Mutate the status and tell the host, once, and only when it changed.
    /// The event is emitted with the lock released: `emit` may hop threads and
    /// must never be given a reason to reach back in here.
    fn update(&self, mutate: impl FnOnce(&mut RunnerStatus)) {
        let published = {
            let mut inner = self.lock();
            let before = inner.status.clone();
            mutate(&mut inner.status);
            (inner.status != before).then(|| inner.status.clone())
        };
        if let Some(status) = published {
            let _ = self.events.emit(EngineEvent::RunnerState(status));
        }
    }

    /// A spawn is no longer in flight, if the one that finished is the one
    /// that claimed the flag. A stale readiness wait -- one whose launch has
    /// already been replaced -- must not release a claim it does not hold.
    fn clear_starting_for(&self, launch_id: u64) {
        let mut inner = self.lock();
        if inner.launch_id == launch_id {
            inner.starting = false;
        }
    }

    fn set_phase(&self, phase: RunnerPhase, detail: &str) {
        self.update(|status| {
            status.phase = phase;
            status.detail = detail.to_string();
        });
    }

    fn note(&self, line: &str) {
        let mut inner = self.lock();
        if inner.log.len() == LOG_LINES {
            inner.log.pop_front();
        }
        inner.log.push_back(line.to_string());
    }

    // ── Configuration ─────────────────────────────────────

    /// Point the runner at a model and an idle window. A change to either stops
    /// a running sidecar; the next dictation (or `prewarm`) starts the new one,
    /// so switching models does not pay a load the user did not ask for.
    pub fn configure(self: &Arc<Self>, model_key: &str, idle_minutes: u64) {
        let changed = {
            let mut inner = self.lock();
            let changed = inner.model_key != model_key || inner.idle_minutes != idle_minutes;
            if inner.model_key != model_key {
                // Different weights to prove present.
                inner.model_verified = None;
            }
            inner.model_key = model_key.to_string();
            inner.idle_minutes = idle_minutes;
            changed
        };
        if changed {
            self.stop();
            let repo = model_for(model_key).repo.to_string();
            self.update(|status| {
                status.model = repo;
                status.resident_bytes = None;
            });
        }
    }

    // ── Install ───────────────────────────────────────────

    /// [`LocalRunner::is_installed`], answered from the cache once it has been
    /// true.
    ///
    /// The uncached check runs a Python process, and `start` runs it on the
    /// key-up path of every dictation whose sidecar is not already up: two
    /// interpreter launches (this and the model check) in front of the take the
    /// user is waiting for. A venv that imports does not stop importing on its
    /// own, so the answer is remembered until `install` runs again. Only a
    /// *true* answer is cached: a missing venv becomes present the moment the
    /// user presses Install, and that must be noticed.
    fn installed_now(&self) -> bool {
        if self.lock().installed_verified {
            return true;
        }
        let verified = self.is_installed();
        if verified {
            self.lock().installed_verified = true;
        }
        verified
    }

    /// [`LocalRunner::is_model_present`], cached per repo for the same reason.
    fn model_present_now(&self) -> bool {
        let repo = self.model_repo();
        if self.lock().model_verified.as_deref() == Some(repo.as_str()) {
            return true;
        }
        let verified = self.is_model_present();
        if verified {
            self.lock().model_verified = Some(repo);
        }
        verified
    }

    pub fn is_installed(&self) -> bool {
        let python = self.venv_python();
        python.is_file()
            && Command::new(&python)
                .args(["-c", "import mlx_audio, huggingface_hub"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
    }

    /// Build the virtualenv and install `mlx-audio` into it. Blocking; the UI
    /// calls [`LocalRunner::install_async`].
    ///
    /// Idempotent: an install over a working venv checks, reports and returns
    /// without downloading anything, so pressing Install twice is free and a
    /// half-built venv is repaired by pressing it again.
    pub fn install(&self) -> Result<(), String> {
        // Captured once, before the first child, and checked by every one of
        // them: a stop that lands between the venv and the pip install has to
        // stop the install, not just the child that happened to be running.
        let generation = self.setup_generation();
        // Re-prove it: the point of pressing Install on a broken venv is that
        // the cached answer is the thing being doubted.
        self.lock().installed_verified = false;
        if self.installed_now() {
            self.set_phase(
                RunnerPhase::Stopped,
                "On-device transcription is installed.",
            );
            return Ok(());
        }
        let Some(python) = Self::find_python() else {
            self.set_phase(RunnerPhase::MissingPython, &Self::missing_python_message());
            return Err(Self::missing_python_message());
        };
        self.set_phase(
            RunnerPhase::Installing,
            "Creating the Python environment...",
        );
        let venv = self.venv_dir();
        if let Some(parent) = venv.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("Could not create {}: {}", parent.display(), error))?;
        }
        if !self.venv_python().is_file() {
            self.run_with_progress(
                Command::new(&python).arg("-m").arg("venv").arg(&venv),
                RunnerPhase::Installing,
                generation,
            )?;
        }
        self.set_phase(
            RunnerPhase::Installing,
            "Installing mlx-audio, about 600 MB...",
        );
        self.run_with_progress(
            Command::new(self.venv_python())
                .arg("-m")
                .arg("pip")
                .arg("install")
                .arg("--disable-pip-version-check")
                .arg(format!("mlx-audio=={MLX_AUDIO_VERSION}"))
                .arg("huggingface_hub"),
            RunnerPhase::Installing,
            generation,
        )?;
        if !self.installed_now() {
            let message = "The Python environment installed but mlx-audio will not import. Remove the runner folder in the app's data directory and try again.".to_string();
            self.set_phase(RunnerPhase::Failed, &message);
            return Err(message);
        }
        self.set_phase(
            RunnerPhase::Stopped,
            "On-device transcription is installed.",
        );
        Ok(())
    }

    // ── Download ──────────────────────────────────────────

    /// Whether the weights are already in the Hugging Face cache. Uses the
    /// standard cache and `local_files_only`, so a model fetched by anything
    /// else on this Mac counts and is never downloaded twice.
    pub fn is_model_present(&self) -> bool {
        let repo = self.model_repo();
        self.venv_python().is_file()
            && Command::new(self.venv_python())
                .args([
                    "-c",
                    "import sys; from huggingface_hub import snapshot_download; snapshot_download(sys.argv[1], local_files_only=True)",
                    &repo,
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
    }

    /// Fetch the weights for the configured model. Blocking.
    pub fn download(&self) -> Result<(), String> {
        let generation = self.setup_generation();
        if !self.installed_now() {
            return Err(
                "Install the Python environment first, then download the model.".to_string(),
            );
        }
        self.lock().model_verified = None;
        let repo = self.model_repo();
        if self.model_present_now() {
            self.set_phase(RunnerPhase::Stopped, &format!("{repo} is downloaded."));
            return Ok(());
        }
        self.set_phase(RunnerPhase::Downloading, &format!("Downloading {repo}..."));
        self.run_with_progress(
            Command::new(self.venv_python()).args([
                "-c",
                "import sys; from huggingface_hub import snapshot_download; print(snapshot_download(sys.argv[1]))",
                &repo,
            ]),
            RunnerPhase::Downloading,
            generation,
        )?;
        self.lock().model_verified = Some(repo.clone());
        self.set_phase(RunnerPhase::Stopped, &format!("{repo} is downloaded."));
        Ok(())
    }

    /// The stop counter the setup children are measured against.
    fn setup_generation(&self) -> u64 {
        self.lock().setup_generation
    }

    /// Run a child to completion, publishing its output as progress.
    ///
    /// `generation` is the value [`LocalRunner::setup_generation`] had when the
    /// step began. A stop bumps it, and this refuses to spawn once it is stale
    /// -- which is how a Stop pressed between two children of one install stops
    /// the install rather than only the child that was running.
    fn run_with_progress(
        &self,
        command: &mut Command,
        phase: RunnerPhase,
        generation: u64,
    ) -> Result<(), String> {
        // The spawn and the pid are recorded under one lock, and the generation
        // is re-checked inside it, for the same reason `launch` does it: `stop`
        // takes the same lock, so it either runs entirely before this (and the
        // check refuses to spawn) or entirely after (and it sees the pid).
        // Outside the lock there is a window in which `stop` finds no pid to
        // kill because there is not one yet, and pip goes on downloading
        // 600 MB after the user pressed Stop.
        let mut child = {
            let mut inner = self.lock();
            if inner.setup_generation != generation {
                return Err(SETUP_STOPPED.to_string());
            }
            let child = command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .env("PYTHONUNBUFFERED", "1")
                .spawn()
                // No `set_phase` here: the lock is held, and taking it twice is
                // a deadlock. The caller reports the failure.
                .map_err(|error| format!("Could not start Python: {}", error))?;
            inner.setup_pid = Some(child.id());
            child
        };

        // stderr on its own thread: pip and the hub both write progress there,
        // and reading one pipe to the end while the other fills its buffer is
        // how a build deadlocks.
        let errors = child.stderr.take().map(|stream| {
            std::thread::spawn(move || {
                let mut collected = Vec::new();
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    collected.push(line);
                    if collected.len() > LOG_LINES {
                        collected.remove(0);
                    }
                }
                collected
            })
        });
        if let Some(stream) = child.stdout.take() {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                self.note(&line);
                // Keep draining the pipe, but stop publishing: a straggler line
                // that arrives after a stop would otherwise write "Installing
                // mlx-audio..." over the "Stopped." the user just asked for.
                if self.setup_generation() != generation {
                    continue;
                }
                self.update(|status| {
                    status.phase = phase;
                    status.detail = progress_line(&line);
                });
            }
        }
        let wait = child.wait();
        let stopped = {
            let mut inner = self.lock();
            // Only if it is still ours: a stop took it before signalling it.
            if inner.setup_pid == Some(child.id()) {
                inner.setup_pid = None;
            }
            inner.setup_generation != generation
        };
        let status = wait.map_err(|error| format!("Python did not finish: {}", error))?;
        let errors = errors
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        for line in &errors {
            self.note(line);
        }
        // Before the exit status, because a stopped child exits on a signal and
        // "Setting up on-device transcription failed: exit code None" is not
        // what pressing Stop means.
        if stopped {
            return Err(SETUP_STOPPED.to_string());
        }
        if status.success() {
            return Ok(());
        }
        let reason = errors
            .iter()
            .rev()
            .find(|line| !line.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| format!("exit code {:?}", status.code()));
        let message = format!("Setting up on-device transcription failed: {reason}");
        self.set_phase(RunnerPhase::Failed, &message);
        Err(message)
    }

    // ── Spawning ──────────────────────────────────────────

    /// Make sure a sidecar is answering, starting one if necessary, and give
    /// back its port. Blocking, so callers on an async runtime hand it to
    /// `spawn_blocking`.
    pub fn ensure_ready(self: &Arc<Self>, timeout: Duration) -> Result<u16, String> {
        if let Some(port) = self.ready_port() {
            return Ok(port);
        }
        self.start(timeout)
    }

    /// Spawn the sidecar and wait for `/health`, or join a start already in
    /// flight.
    ///
    /// Single-flight, and that is the whole of it: `prewarm` on key-down and
    /// `ensure_ready` on key-up are two callers asking the same question a few
    /// seconds apart, and the first has not finished answering when the second
    /// arrives. Spawning twice left the first sidecar loading a model nobody
    /// could reach and nobody would kill until quit.
    pub fn start(self: &Arc<Self>, timeout: Duration) -> Result<u16, String> {
        {
            let mut inner = self.lock();
            if inner.starting {
                drop(inner);
                return self.wait_for_start(timeout);
            }
            // Claimed here rather than in `launch`, because everything between
            // here and the spawn -- the install and model checks -- is time a
            // second caller could walk straight through.
            inner.starting = true;
        }
        let outcome = self.start_claimed(timeout);
        if outcome.is_err() {
            // `await_ready` clears the claim for its own launch; this covers
            // the paths that never got that far.
            self.lock().starting = false;
        }
        outcome
    }

    /// Wait out the start someone else is already running.
    fn wait_for_start(&self, timeout: Duration) -> Result<u16, String> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let inner = self.lock();
                if inner.status.phase == RunnerPhase::Ready {
                    if let Some(port) = inner.status.port {
                        return Ok(port);
                    }
                }
                if inner.status.phase == RunnerPhase::Failed {
                    return Err(inner.status.detail.clone());
                }
                if !inner.starting {
                    // It finished, and not with a ready sidecar.
                    let detail = inner.status.detail.clone();
                    return Err(if detail.is_empty() {
                        "The local runner did not start".to_string()
                    } else {
                        detail
                    });
                }
            }
            if Instant::now() >= deadline {
                // A claim with no child behind it, a whole window later, is a
                // claim nobody is going to release. Left alone it would send
                // every later start into this same wait; the owner cannot still
                // be between claiming and spawning after all this time.
                let mut inner = self.lock();
                if inner.pid.is_none() {
                    inner.starting = false;
                }
                return Err("Timed out waiting for the local runner to start".to_string());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The body of a start whose claim this caller holds.
    fn start_claimed(self: &Arc<Self>, timeout: Duration) -> Result<u16, String> {
        let needs_install = self
            .program
            .as_ref()
            .map(|program| program.needs_install)
            .unwrap_or(true);
        if needs_install {
            if !self.installed_now() {
                let message = if Self::find_python().is_some() {
                    "On-device transcription is not installed yet. Open Settings and press Install."
                        .to_string()
                } else {
                    Self::missing_python_message()
                };
                self.set_phase(
                    if Self::find_python().is_some() {
                        RunnerPhase::Stopped
                    } else {
                        RunnerPhase::MissingPython
                    },
                    &message,
                );
                return Err(message);
            }
            if !self.model_present_now() {
                let message = format!(
                    "{} is not downloaded yet. Open Settings and press Download.",
                    self.model_repo()
                );
                self.set_phase(RunnerPhase::Stopped, &message);
                return Err(message);
            }
        }
        let generation = {
            let mut inner = self.lock();
            inner.generation += 1;
            inner.generation
        };
        if let Err(error) = self.launch(generation) {
            self.set_phase(RunnerPhase::Failed, &error);
            return Err(error);
        }
        self.await_ready(timeout)
    }

    /// One spawn. Shared by the first start and every restart, so a restarted
    /// sidecar is configured exactly like the original.
    fn launch(self: &Arc<Self>, generation: u64) -> Result<(), String> {
        // Whatever this launch is replacing dies first. On the ordinary restart
        // path there is nothing here, because `child_exited` already cleared
        // the pid of the process that exited; this catches the case where a
        // live sidecar would otherwise be overwritten and left running with no
        // one holding its pid.
        // This launch's identity is claimed *before* anything is killed, and
        // the order is the whole point: the monitor of the child about to be
        // replaced compares the id it was given against the current one, and if
        // the kill came first it would still see its own id as current and read
        // being replaced as having crashed -- clearing the live sidecar's pid,
        // counting a restart and spawning a third process.
        let launch_id = {
            let mut inner = self.lock();
            inner.launch_id += 1;
            inner.launch_id
        };
        let replaced = {
            let mut inner = self.lock();
            inner.pid.take()
        };
        if let Some(pid) = replaced {
            terminate(pid);
        }

        let (model_key, idle_minutes) = {
            let inner = self.lock();
            (inner.model_key.clone(), inner.idle_minutes)
        };
        let repo = model_for(&model_key).repo.to_string();

        let (python, script, extra) = match &self.program {
            Some(program) => (
                program.python.clone(),
                program.script.clone(),
                program.extra_args.clone(),
            ),
            None => (self.venv_python(), Self::script_path()?, Vec::new()),
        };

        self.update(|status| {
            status.phase = RunnerPhase::Starting;
            status.detail = format!("Starting {repo}...");
            status.model = repo.clone();
            // Unknown until the child says so; see the `--port 0` note below.
            status.port = None;
            status.resident_bytes = None;
        });

        // The spawn and the pid are recorded under one lock, and the
        // generation is re-checked inside it. `stop` takes the same lock, so it
        // either runs entirely before this (and the check below refuses to
        // spawn) or entirely after (and it sees the pid). There is no window in
        // between, which is what orphaned a sidecar on a test machine when the
        // check was outside: `stop` found no pid to kill because there was not
        // one yet, and the spawn stored it a moment later.
        let mut inner = self.lock();
        if inner.generation != generation || inner.launch_id != launch_id {
            // Stopped, or overtaken by a launch that started after this one.
            return Err("The local runner was stopped while starting".to_string());
        }
        let mut child = Command::new(&python)
            .arg(&script)
            // The *child* picks the port. A parent that picks one has to close
            // its listener before the child binds, and two sidecars starting at
            // once can be handed the same number in that gap -- the loser then
            // exits on a bind error that reads as a crash. The child asks the
            // kernel for a port it is about to hold and prints it.
            .arg("--port")
            .arg("0")
            .arg("--model")
            .arg(&repo)
            .arg("--idle-minutes")
            .arg(idle_minutes.to_string())
            .args(&extra)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .env("PYTHONUNBUFFERED", "1")
            // The sidecar must never reach the network. The weights are
            // downloaded by the explicit step above; an offline hub client
            // turns a missing file into an error the user sees instead of a
            // multi-gigabyte download starting mid-dictation.
            .env("HF_HUB_OFFLINE", "1")
            .spawn()
            // No `set_phase` here: the lock is held, and taking it twice is a
            // deadlock. The caller reports the failure.
            .map_err(|error| format!("Could not start the local runner: {}", error))?;

        let pid = child.id();
        inner.pid = Some(pid);
        inner.starting = true;
        drop(inner);
        // Both of the supervisor's own threads hold a *weak* reference. A
        // monitor blocked in `wait` for the life of the sidecar would otherwise
        // be holding the supervisor alive, `Drop` would never run, and the
        // sidecar would outlive the app -- which is the one thing this file
        // exists to prevent.
        if let Some(stream) = child.stderr.take() {
            let runner = Arc::downgrade(self);
            std::thread::Builder::new()
                .name("openflow-runner-log".into())
                .spawn(move || {
                    let mut port_published = false;
                    for line in BufReader::new(stream).lines().map_while(Result::ok) {
                        let Some(runner) = runner.upgrade() else {
                            return;
                        };
                        if !port_published {
                            if let Some(port) = parse_listening_port(&line) {
                                port_published = true;
                                runner.publish_port(port, launch_id);
                            }
                        }
                        runner.note(&line);
                    }
                })
                .ok();
        }
        let runner: Weak<Self> = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("openflow-runner-monitor".into())
            .spawn(move || {
                let outcome = child.wait();
                // Gone means the app dropped the supervisor, which killed this
                // child on the way out. Nothing to restart.
                let Some(runner) = runner.upgrade() else {
                    return;
                };
                runner.child_exited(
                    generation,
                    launch_id,
                    outcome.ok().and_then(|status| status.code()),
                );
            })
            .map_err(|error| format!("Could not supervise the local runner: {}", error))?;
        Ok(())
    }

    /// The sidecar exited. Either we asked it to, or it crashed, or it was
    /// replaced -- and only the middle one is a reason to restart anything.
    ///
    /// Two identities are checked, because they mean different things. The
    /// *generation* changes when the user stops or reconfigures the runner, and
    /// a monitor for a process from an older generation has nothing to do. The
    /// *launch id* changes on every spawn, so a mismatch means this child was
    /// replaced by a newer one that is running right now: `launch` kills the
    /// child it is about to overwrite, and without this check that kill came
    /// back here as a crash. It would then clear `inner.pid` -- the pid of the
    /// *live* sidecar, which the supervisor would no longer be holding -- count
    /// a restart, wait out the backoff and spawn a third process. One kill,
    /// three sidecars, and no way to stop two of them.
    fn child_exited(self: &Arc<Self>, generation: u64, launch_id: u64, code: Option<i32>) {
        {
            let mut inner = self.lock();
            if inner.generation != generation {
                // We stopped it, or reconfigured past it. Nothing to do.
                return;
            }
            if inner.launch_id != launch_id {
                // Already replaced. The newer launch owns the pid slot and its
                // own monitor; this exit is the replacement happening, not a
                // failure.
                return;
            }
            inner.pid = None;
            let now = Instant::now();
            while let Some(oldest) = inner.restarts.front() {
                if now.duration_since(*oldest) > RESTART_WINDOW {
                    inner.restarts.pop_front();
                } else {
                    break;
                }
            }
            inner.restarts.push_back(now);
            if inner.restarts.len() > MAX_RESTARTS {
                inner.starting = false;
                drop(inner);
                let tail = self
                    .log()
                    .into_iter()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or_else(|| format!("exit code {:?}", code));
                self.update(|status| {
                    status.phase = RunnerPhase::Failed;
                    status.port = None;
                    status.resident_bytes = None;
                    status.detail = format!(
                        "The local runner stopped {} times in a minute and will not be restarted. Last message: {}",
                        MAX_RESTARTS + 1,
                        tail
                    );
                });
                return;
            }
            let attempt = inner.restarts.len().saturating_sub(1);
            drop(inner);
            self.update(|status| {
                status.phase = RunnerPhase::Starting;
                status.port = None;
                status.resident_bytes = None;
                status.detail = "The local runner stopped. Restarting...".to_string();
            });
            std::thread::sleep(BACKOFF[attempt.min(BACKOFF.len() - 1)]);
        }
        if self.lock().generation != generation {
            return;
        }
        if let Err(error) = self.launch(generation) {
            self.set_phase(RunnerPhase::Failed, &error);
            return;
        }
        let runner = Arc::clone(self);
        std::thread::Builder::new()
            .name("openflow-runner-ready".into())
            .spawn(move || {
                let _ = runner.await_ready(READY_TIMEOUT);
            })
            .ok();
    }

    /// The port the child bound, once it has told us, and only for the launch
    /// that is still current.
    fn publish_port(&self, port: u16, launch_id: u64) {
        if self.lock().launch_id != launch_id {
            return;
        }
        self.update(|status| status.port = Some(port));
    }

    /// Poll `/health` until the sidecar answers, or the window closes.
    ///
    /// A bounded wait on a process that has just been spawned, not a timer on
    /// the idle path: it ends the moment the first answer arrives, and there is
    /// nothing running between starts. The first thing it waits for is the port
    /// itself, which the child prints once it has bound one.
    fn await_ready(self: &Arc<Self>, timeout: Duration) -> Result<u16, String> {
        let deadline = Instant::now() + timeout;
        let (generation, launch_id) = {
            let inner = self.lock();
            (inner.generation, inner.launch_id)
        };
        loop {
            let port = {
                let inner = self.lock();
                if inner.generation != generation || inner.launch_id != launch_id {
                    // Stopped, reconfigured, or already relaunched. Whatever
                    // this wait learns is about a process nobody is waiting for,
                    // and the claim it would release belongs to that newer
                    // launch, so it is left alone.
                    return Err("The local runner was replaced while starting".to_string());
                }
                if inner.status.phase == RunnerPhase::Failed {
                    let detail = inner.status.detail.clone();
                    drop(inner);
                    self.clear_starting_for(launch_id);
                    return Err(detail);
                }
                inner.status.port
            };
            let Some(port) = port else {
                // The child has not printed its port yet.
                if Instant::now() >= deadline {
                    let message =
                        "The local runner never reported a port. Check that Python can run."
                            .to_string();
                    self.set_phase(RunnerPhase::Failed, &message);
                    self.clear_starting_for(launch_id);
                    return Err(message);
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            match health(port, Duration::from_secs(2)) {
                Ok(health) => {
                    self.update(|status| {
                        status.phase = RunnerPhase::Ready;
                        status.port = Some(port);
                        status.resident_bytes = health.memory();
                        status.detail = match health.state.as_str() {
                            "ready" => "Model loaded and ready.".to_string(),
                            "loading" => "Loading the model...".to_string(),
                            _ => "Ready. The model loads on the first dictation.".to_string(),
                        };
                    });
                    if let Some(error) = health.error {
                        self.set_phase(
                            RunnerPhase::Failed,
                            &format!("The local runner could not load the model: {error}"),
                        );
                        self.clear_starting_for(launch_id);
                        return Err(error);
                    }
                    self.clear_starting_for(launch_id);
                    return Ok(port);
                }
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(120));
                }
                Err(error) => {
                    let tail = self
                        .log()
                        .into_iter()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or(error);
                    let message = format!("The local runner did not start: {tail}");
                    self.set_phase(RunnerPhase::Failed, &message);
                    self.clear_starting_for(launch_id);
                    return Err(message);
                }
            }
        }
    }

    /// Ask the sidecar to load its weights now, and answer immediately.
    ///
    /// Called when recording starts, so the ~3 s load overlaps the user
    /// speaking. Never blocks the caller and never reports an error: a prewarm
    /// that fails costs the dictation nothing that the dictation would not have
    /// paid anyway.
    pub fn prewarm(self: &Arc<Self>) {
        {
            let mut inner = self.lock();
            if inner.working || inner.status.phase == RunnerPhase::Failed {
                return;
            }
            inner.working = true;
        }
        let runner = Arc::clone(self);
        std::thread::Builder::new()
            .name("openflow-runner-prewarm".into())
            .spawn(move || {
                if let Ok(port) = runner.ensure_ready(READY_TIMEOUT) {
                    if let Ok(health) = post(port, "/prewarm", Duration::from_secs(5)) {
                        runner.update(|status| {
                            status.resident_bytes = health.memory();
                            if health.state == "loading" {
                                status.detail = "Loading the model...".to_string();
                            }
                        });
                    }
                }
                runner.lock().working = false;
            })
            .ok();
    }

    /// Refresh the reported memory from `/health`, without starting anything.
    pub fn refresh_health(&self) {
        let Some(port) = self.ready_port() else {
            return;
        };
        if let Ok(health) = health(port, Duration::from_secs(2)) {
            self.update(|status| {
                status.resident_bytes = health.memory();
                if health.state == "unloaded" {
                    status.detail = "Idle. The model is unloaded.".to_string();
                }
            });
        }
    }

    /// Stop the sidecar, and any install or download in flight, and do not
    /// restart anything. Safe to call when nothing is running, and safe to call
    /// twice.
    ///
    /// The setup children are killed too because the Stop button is live during
    /// an install (`local_stop` is enabled whenever the phase is busy), and a
    /// Stop that left a 600 MB `pip install` running would also re-enable
    /// Install -- a second pip in the same half-built venv.
    pub fn stop(&self) {
        let (pid, setup) = {
            let mut inner = self.lock();
            inner.generation += 1;
            // Bumped under the same lock the spawn takes, so a setup step is
            // either already spawned (and its pid is here) or has not spawned
            // yet (and will refuse to, seeing a generation it does not own).
            inner.setup_generation += 1;
            inner.restarts.clear();
            // Nothing is in flight after a stop, whoever claimed it.
            inner.starting = false;
            (inner.pid.take(), inner.setup_pid.take())
        };
        if let Some(pid) = setup {
            terminate(pid);
        }
        if let Some(pid) = pid {
            terminate(pid);
        }
        self.update(|status| {
            if status.phase != RunnerPhase::Failed {
                status.phase = RunnerPhase::Stopped;
                status.detail = "Stopped.".to_string();
            }
            status.port = None;
            status.resident_bytes = None;
        });
    }
}

impl Drop for LocalRunner {
    /// A sidecar holding 2.5 GB of weights must not outlive the app, and
    /// neither must an install. The sidecar also watches for its parent
    /// disappearing, which covers the app being SIGKILLed, but this is the
    /// ordinary path.
    fn drop(&mut self) {
        let (pid, setup) = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.generation += 1;
            inner.setup_generation += 1;
            (inner.pid.take(), inner.setup_pid.take())
        };
        // A pip download is the same kind of orphan as a loaded sidecar: it
        // holds the venv the next launch will try to build into.
        if let Some(pid) = setup {
            terminate(pid);
        }
        if let Some(pid) = pid {
            terminate(pid);
        }
    }
}

// ── Process control ───────────────────────────────────────

/// SIGTERM, then SIGKILL if it is still there. The sidecar's HTTP server
/// shuts down on SIGTERM; the second signal is for one wedged in a decode.
#[cfg(unix)]
fn terminate(pid: u32) {
    // SAFETY: `kill` with a pid this process spawned and a valid signal number.
    // A pid that has already exited returns ESRCH, which is ignored.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    for _ in 0..20 {
        // SAFETY: signal 0 tests for the process's existence and sends nothing.
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    // SAFETY: as above.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn terminate(_pid: u32) {}

/// The port out of the sidecar's own "runner listening" line.
///
/// The child binds `127.0.0.1:0` and prints what the kernel gave it, so the
/// port is never chosen by a parent that then has to let go of it -- which is
/// what let two sidecars starting at once be handed the same number, with the
/// loser exiting on a bind error that read as a crash.
///
/// Deliberately strict about the host: a line naming any other address is not
/// this contract, and following it would point the supervisor's health checks
/// somewhere off the machine.
fn parse_listening_port(line: &str) -> Option<u16> {
    let rest = line.split("runner listening on http://127.0.0.1:").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let port: u16 = digits.parse().ok()?;
    (port != 0).then_some(port)
}

// ── The loopback client ───────────────────────────────────
//
// Small on purpose. `/health` and `/prewarm` are two request shapes against a
// server on this machine, and going through `reqwest` would mean either the
// blocking feature (a second runtime inside a supervisor thread) or making
// every supervisor path async. Transcription itself is not here: it goes
// through the normal `transcribe` path, as any other OpenAI-compatible
// endpoint would.

fn health(port: u16, timeout: Duration) -> Result<Health, String> {
    let body = loopback(port, "GET", "/health", timeout)?;
    serde_json::from_str(&body).map_err(|error| format!("Unreadable health response: {}", error))
}

fn post(port: u16, path: &str, timeout: Duration) -> Result<Health, String> {
    let body = loopback(port, "POST", path, timeout)?;
    serde_json::from_str(&body).map_err(|error| format!("Unreadable response: {}", error))
}

fn loopback(port: u16, method: &str, path: &str, timeout: Duration) -> Result<String, String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|error| format!("Could not reach the local runner: {}", error))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("Could not ask the local runner: {}", error))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|error| format!("The local runner did not answer: {}", error))?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "The local runner sent a malformed answer".to_string())?;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| "The local runner sent no status".to_string())?;
    if !(200..300).contains(&status) {
        return Err(format!("The local runner answered {status}"));
    }
    Ok(body.to_string())
}

/// One line of installer output, trimmed to something a label can hold.
fn progress_line(line: &str) -> String {
    let line = line.trim();
    let cut = line
        .char_indices()
        .nth(120)
        .map(|(index, _)| index)
        .unwrap_or(line.len());
    line[..cut].to_string()
}

/// The `(major, minor)` an interpreter reports, or nothing when it will not run.
fn python_version(python: &Path) -> Option<(u32, u32)> {
    let output = Command::new(python)
        .args([
            "-c",
            "import sys; print('%d.%d' % sys.version_info[:2], end='')",
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let (major, minor) = text.trim().split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A sink that counts what it was told, so a test can prove the UI is
    /// pushed to rather than expected to poll.
    #[derive(Default)]
    struct Recorder {
        states: Mutex<Vec<RunnerStatus>>,
        others: AtomicUsize,
    }

    impl EngineEvents for Recorder {
        fn emit(&self, event: EngineEvent) -> Result<(), String> {
            match event {
                EngineEvent::RunnerState(status) => {
                    self.states
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(status);
                }
                _ => {
                    self.others.fetch_add(1, Ordering::SeqCst);
                }
            }
            Ok(())
        }
    }

    impl Recorder {
        fn phases(&self) -> Vec<RunnerPhase> {
            self.states
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .map(|status| status.phase)
                .collect()
        }
    }

    /// The stdlib-only stand-in for `runner.py`: same argv, same two endpoints,
    /// canned answers, and switches for the failures the supervisor has to
    /// survive. Written to a temp directory by each test that wants one.
    const FAKE_RUNNER: &str = r#"
import argparse, json, os, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

parser = argparse.ArgumentParser()
parser.add_argument("--port", type=int, required=True)
parser.add_argument("--model", required=True)
parser.add_argument("--idle-minutes", type=float, default=10.0)
parser.add_argument("--crash-after", type=float, default=0.0)
parser.add_argument("--never-listen", action="store_true")
arguments = parser.parse_args()

if arguments.crash_after > 0:
    def die():
        time.sleep(arguments.crash_after)
        os._exit(9)
    threading.Thread(target=die, daemon=True).start()

if arguments.never_listen:
    time.sleep(3600)

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *args):
        pass
    def _reply(self):
        body = json.dumps({
            "state": "ready",
            "model": arguments.model,
            "resident_bytes": 1234567,
            "pid": os.getpid(),
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        self._reply()
    def do_POST(self):
        self._reply()

server = ThreadingHTTPServer(("127.0.0.1", arguments.port), Handler)
sys.stderr.write(
    "runner listening on http://127.0.0.1:%d model=%s idle=%.1fm\n"
    % (server.server_address[1], arguments.model, arguments.idle_minutes)
)
sys.stderr.flush()
server.serve_forever()
"#;

    struct Fixture {
        directory: PathBuf,
        script: PathBuf,
        python: PathBuf,
    }

    /// `None` when this machine has no Python at all, which is the one thing
    /// that makes these tests unrunnable rather than failing.
    fn fixture() -> Option<Fixture> {
        let python = LocalRunner::find_python().or_else(|| {
            let system = PathBuf::from("/usr/bin/python3");
            python_version(&system).map(|_| system)
        })?;
        let directory =
            std::env::temp_dir().join(format!("openflow-runner-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).ok()?;
        let script = directory.join("fake_runner.py");
        std::fs::write(&script, FAKE_RUNNER).ok()?;
        Some(Fixture {
            directory,
            script,
            python,
        })
    }

    fn build(fixture: &Fixture, extra: &[&str]) -> (Arc<LocalRunner>, Arc<Recorder>) {
        build_checked(fixture, extra, false)
    }

    fn build_checked(
        fixture: &Fixture,
        extra: &[&str],
        needs_install: bool,
    ) -> (Arc<LocalRunner>, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        let events: Arc<dyn EngineEvents> = recorder.clone();
        let runner = LocalRunner::with_program(
            fixture.directory.clone(),
            events,
            "fast",
            Some(RunnerProgram {
                python: fixture.python.clone(),
                script: fixture.script.clone(),
                extra_args: extra.iter().map(|argument| argument.to_string()).collect(),
                needs_install,
            }),
        );
        (runner, recorder)
    }

    /// How many sidecars from this fixture are running right now. Each fixture
    /// writes its script into its own uuid directory, so the path is unique to
    /// this test.
    fn live_sidecars(script: &Path) -> usize {
        let needle = script.display().to_string();
        let Ok(output) = Command::new("/bin/ps").args(["-A", "-o", "args="]).output() else {
            return 0;
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.contains(&needle))
            .count()
    }

    fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        condition()
    }

    fn alive(pid: u32) -> bool {
        #[cfg(unix)]
        // SAFETY: signal 0 tests for existence and sends nothing.
        unsafe {
            libc::kill(pid as libc::pid_t, 0) == 0
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            false
        }
    }

    /// A spawned sidecar becomes ready, on a loopback port, and the host is
    /// told about it rather than having to ask.
    #[test]
    fn a_spawned_runner_reports_ready_on_a_loopback_port() {
        let Some(fixture) = fixture() else {
            eprintln!("skipped: no python3 on this machine");
            return;
        };
        let (runner, recorder) = build(&fixture, &[]);
        let port = runner
            .start(Duration::from_secs(20))
            .expect("the fake runner should come up");
        assert_eq!(runner.status().phase, RunnerPhase::Ready);
        assert_eq!(runner.ready_port(), Some(port));
        assert_eq!(
            runner.status().resident_bytes,
            Some(1_234_567),
            "the memory the sidecar reports has to reach the status the UI reads"
        );

        // The port is on loopback and nowhere else.
        assert!(crate::transcribe::is_loopback_url(&format!(
            "http://127.0.0.1:{port}/v1"
        )));

        let phases = recorder.phases();
        assert!(
            phases.contains(&RunnerPhase::Starting) && phases.contains(&RunnerPhase::Ready),
            "the host must be pushed both states, got {phases:?}"
        );
        runner.stop();
        assert_eq!(runner.status().phase, RunnerPhase::Stopped);
        let _ = std::fs::remove_dir_all(&fixture.directory);
    }

    /// A sidecar that dies on its own comes back, with a new process.
    #[test]
    fn a_crashed_runner_is_restarted() {
        let Some(fixture) = fixture() else {
            eprintln!("skipped: no python3 on this machine");
            return;
        };
        let (runner, _recorder) = build(&fixture, &[]);
        runner.start(Duration::from_secs(20)).expect("first start");
        let first = runner.pid().expect("a running sidecar has a pid");

        terminate(first);
        assert!(
            wait_for(Duration::from_secs(20), || {
                runner.pid().map(|pid| pid != first).unwrap_or(false)
                    && runner.status().phase == RunnerPhase::Ready
            }),
            "the supervisor must replace a dead sidecar, status was {:?}",
            runner.status()
        );
        let second = runner.pid().expect("a replacement pid");
        assert_ne!(first, second);
        assert!(!alive(first), "the dead one must be gone");

        runner.stop();
        assert!(wait_for(Duration::from_secs(5), || !alive(second)));
        let _ = std::fs::remove_dir_all(&fixture.directory);
    }

    /// A sidecar that cannot stay up is given three restarts inside a minute
    /// and then reported failed, rather than respawned forever.
    #[test]
    fn a_runner_that_keeps_dying_ends_in_failed() {
        let Some(fixture) = fixture() else {
            eprintln!("skipped: no python3 on this machine");
            return;
        };
        let (runner, recorder) = build(&fixture, &["--crash-after", "0.15"]);
        // The first start may or may not see a ready window before the crash;
        // either way the crash path is what is under test.
        let began = Instant::now();
        let _ = runner.start(Duration::from_secs(20));

        assert!(
            wait_for(Duration::from_secs(40), || {
                runner.status().phase == RunnerPhase::Failed
            }),
            "a runner crashing every 150 ms must end in failed, status was {:?}",
            runner.status()
        );
        // Backoff, proven rather than assumed: giving up cannot have happened
        // faster than waiting out every backoff between the restarts.
        let floor: Duration = BACKOFF.iter().sum();
        assert!(
            began.elapsed() >= floor,
            "restarts must back off: gave up after {:?}, faster than {floor:?}",
            began.elapsed()
        );
        let status = runner.status();
        assert!(
            status.detail.contains("will not be restarted"),
            "the failure must say it gave up: {}",
            status.detail
        );
        assert_eq!(status.port, None, "a failed runner offers no port");
        assert_eq!(runner.ready_port(), None);

        // It restarted, and it stopped restarting: exactly MAX_RESTARTS + 1
        // deaths, and the backoff means this cannot have been instant.
        let starts = recorder
            .phases()
            .iter()
            .filter(|phase| **phase == RunnerPhase::Starting)
            .count();
        assert!(
            starts >= MAX_RESTARTS,
            "it must have tried to restart before giving up, saw {starts} starting states"
        );
        assert_eq!(
            recorder.phases().last(),
            Some(&RunnerPhase::Failed),
            "failed has to be the last thing the host is told"
        );

        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(
            runner.status().phase,
            RunnerPhase::Failed,
            "nothing may restart it after it failed"
        );
        runner.stop();
        let _ = std::fs::remove_dir_all(&fixture.directory);
    }

    /// A sidecar that never answers is failed within the window, not waited on
    /// forever.
    #[test]
    fn a_runner_that_never_answers_times_out() {
        let Some(fixture) = fixture() else {
            eprintln!("skipped: no python3 on this machine");
            return;
        };
        let (runner, _recorder) = build(&fixture, &["--never-listen"]);
        let started = Instant::now();
        let outcome = runner.start(Duration::from_secs(2));
        assert!(outcome.is_err(), "an unresponsive runner must not be ready");
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(runner.status().phase, RunnerPhase::Failed);
        runner.stop();
        let _ = std::fs::remove_dir_all(&fixture.directory);
    }

    /// Dropping the supervisor kills the sidecar. Nothing else does it on the
    /// quit path, and 2.5 GB of weights must not survive the app.
    #[test]
    fn dropping_the_supervisor_kills_the_sidecar() {
        let Some(fixture) = fixture() else {
            eprintln!("skipped: no python3 on this machine");
            return;
        };
        let (runner, _recorder) = build(&fixture, &[]);
        runner.start(Duration::from_secs(20)).expect("start");
        let pid = runner.pid().expect("a pid");
        assert!(alive(pid));

        drop(runner);
        assert!(
            wait_for(Duration::from_secs(10), || !alive(pid)),
            "the sidecar must not outlive its supervisor"
        );
        let _ = std::fs::remove_dir_all(&fixture.directory);
    }

    /// A stop that lands while a spawn is in flight must not leave the child
    /// behind.
    ///
    /// This is a bug that happened rather than one imagined: `stop` bumps the
    /// generation and kills whatever pid is recorded, and the launch used to
    /// check the generation just *before* spawning, so a stop in the window
    /// between that check and the pid being recorded killed nothing and the
    /// child was orphaned. A test run left a sidecar running on this machine.
    ///
    /// The fix is structural -- `launch` now spawns and records the pid under
    /// the same lock `stop` takes, so the window does not exist -- which is
    /// what this test can and cannot say. It is a stress check, not a proof:
    /// it can only observe that repeated stops during a spawn leave nothing
    /// running, and it would catch a regression over runs rather than
    /// certainly on the next one.
    #[test]
    fn a_stop_during_a_spawn_leaves_no_orphan() {
        let Some(fixture) = fixture() else {
            eprintln!("skipped: no python3 on this machine");
            return;
        };
        for _ in 0..6 {
            let (runner, _recorder) = build(&fixture, &[]);
            let starting = Arc::clone(&runner);
            let attempt = std::thread::spawn(move || {
                let _ = starting.start(Duration::from_secs(10));
            });
            // Stop as soon as the launch is under way, so the generation was
            // certainly bumped by `start` before `stop` bumps it again: any
            // process spawned from here belongs to a generation nobody wants.
            wait_for(Duration::from_secs(5), || {
                runner.status().phase != RunnerPhase::Stopped
            });
            runner.stop();
            let _ = attempt.join();

            // Deliberately no second `stop` before the assertion: that would
            // kill the orphan and hide exactly what this test is looking for.
            if let Some(pid) = runner.pid() {
                assert!(
                    wait_for(Duration::from_secs(5), || !alive(pid)),
                    "stop left {pid} running"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&fixture.directory);
    }

    /// Two callers asking for a runner at once get one sidecar.
    ///
    /// This is the shape of a real leak, not a hypothetical: `prewarm` fires on
    /// key-down and `ensure_ready` on key-up, a second or two later, and the
    /// first has not finished starting. Before `start` was single-flight the
    /// second spawned a second sidecar, the first's pid was overwritten, its
    /// monitor saw a generation mismatch and did nothing, and that process went
    /// on to load a model and hold 1 to 2.5 GB until the app quit.
    #[test]
    fn a_prewarm_and_a_start_together_spawn_one_sidecar() {
        let Some(fixture) = fixture() else {
            eprintln!("skipped: no python3 on this machine");
            return;
        };
        let (runner, recorder) = build(&fixture, &[]);

        runner.prewarm();
        let port = runner
            .start(Duration::from_secs(20))
            .expect("the second caller joins the start already running");
        assert_eq!(runner.ready_port(), Some(port));

        // One spawn: every port the host was ever told about is this one.
        let ports: Vec<u16> = recorder
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter_map(|status| status.port)
            .collect();
        let mut distinct = ports.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            distinct,
            vec![port],
            "two spawns would show two ports, and one of them would be unreachable"
        );

        // ...and one process, which is the thing the port count is standing in
        // for. A second sidecar answers nobody and is killed by nobody.
        assert!(
            wait_for(Duration::from_secs(5), || live_sidecars(&fixture.script)
                == 1),
            "expected exactly one sidecar, found {}",
            live_sidecars(&fixture.script)
        );

        runner.stop();
        assert!(
            wait_for(Duration::from_secs(5), || live_sidecars(&fixture.script)
                == 0),
            "stop must leave nothing running"
        );
        let _ = std::fs::remove_dir_all(&fixture.directory);
    }

    /// A launch that replaces a live sidecar kills it first.
    ///
    /// The single-flight claim in `start` is what stops two launches racing, so
    /// this path should now be unreachable from outside. It is kept and tested
    /// because it is the last thing standing between a supervisor bug and a
    /// 2.5 GB process nobody holds the pid of, and `launch` is driven directly
    /// here rather than through the guard that is meant to prevent it.
    #[test]
    fn a_launch_that_replaces_a_live_sidecar_kills_it() {
        let Some(fixture) = fixture() else {
            eprintln!("skipped: no python3 on this machine");
            return;
        };
        let (runner, recorder) = build(&fixture, &[]);
        let generation = {
            let mut inner = runner.lock();
            inner.generation += 1;
            inner.generation
        };

        runner.launch(generation).expect("the first spawn");
        let first = runner.pid().expect("a first pid");
        assert!(alive(first));

        runner
            .launch(generation)
            .expect("a second spawn, same generation");
        let second = runner.pid().expect("a second pid");
        assert_ne!(first, second, "the second launch has its own process");
        assert!(
            wait_for(Duration::from_secs(5), || !alive(first)),
            "the sidecar being replaced must be killed, not abandoned"
        );

        // The kill above lands in the first child's monitor thread. It must
        // read there as "replaced", not as "crashed": a crash would clear the
        // pid of the sidecar that is still running, count a restart, wait out
        // the backoff and spawn a third. Long enough here to cover the longest
        // backoff, so a restart would have happened by now if one were coming.
        std::thread::sleep(BACKOFF[BACKOFF.len() - 1] + Duration::from_millis(400));
        assert_eq!(
            runner.pid(),
            Some(second),
            "the supervisor must still be holding the live sidecar's pid"
        );
        assert!(alive(second), "and that sidecar must still be running");

        // Two spawns, two Starting states. A third would mean the replacement
        // was mistaken for a crash. Ports are not the proxy: the first child is
        // killed before its port line can be accepted (its launch id is stale by
        // then, which is the guard doing its job), so only one port is ever
        // published.
        let starts = recorder
            .phases()
            .iter()
            .filter(|phase| **phase == RunnerPhase::Starting)
            .count();
        assert_eq!(
            starts, 2,
            "expected exactly two spawns, saw {starts} starting states"
        );
        assert_eq!(live_sidecars(&fixture.script), 1, "one live sidecar");

        runner.stop();
        assert!(wait_for(Duration::from_secs(5), || !alive(second)));
        assert!(
            wait_for(Duration::from_secs(5), || live_sidecars(&fixture.script)
                == 0),
            "nothing may be left running"
        );
        let _ = std::fs::remove_dir_all(&fixture.directory);
    }

    /// The venv and weight checks are two Python launches, and `start` runs on
    /// the key-up path of any dictation whose sidecar is not already up. They
    /// are proved once and remembered, and forgotten again when the thing they
    /// proved could have changed.
    #[cfg(unix)]
    #[test]
    fn the_install_and_model_checks_are_not_re_run_on_every_start() {
        use std::os::unix::fs::PermissionsExt;
        let Some(fixture) = fixture() else {
            eprintln!("skipped: no python3 on this machine");
            return;
        };
        // A stand-in for the venv interpreter that records every invocation.
        let venv_bin = fixture.directory.join("runner").join("venv").join("bin");
        std::fs::create_dir_all(&venv_bin).expect("venv bin");
        let calls = fixture.directory.join("python-calls");
        let python = venv_bin.join("python3");
        std::fs::write(
            &python,
            format!("#!/bin/sh\necho call >> \"{}\"\nexit 0\n", calls.display()),
        )
        .expect("write the stand-in");
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
        let count = || {
            std::fs::read_to_string(&calls)
                .map(|text| text.lines().count())
                .unwrap_or(0)
        };

        let (runner, _recorder) = build_checked(&fixture, &[], true);
        runner.start(Duration::from_secs(20)).expect("first start");
        assert_eq!(count(), 2, "one check for the venv, one for the weights");

        runner.stop();
        runner.start(Duration::from_secs(20)).expect("second start");
        assert_eq!(
            count(),
            2,
            "a second start must not pay for two interpreter launches again"
        );

        // A different model is different weights, so that half is proved again
        // -- but the venv is still the venv.
        runner.stop();
        runner.configure("accurate", 10);
        runner.start(Duration::from_secs(20)).expect("third start");
        assert_eq!(
            count(),
            3,
            "changing the model re-checks the weights and nothing else"
        );

        runner.stop();
        let _ = std::fs::remove_dir_all(&fixture.directory);
    }

    /// The port comes off the child's own line. A parent that picks one has to
    /// let go of it before the child binds, and two sidecars starting at once
    /// could be handed the same number in that gap -- the loser exiting on a
    /// bind error that reads as a crash.
    #[test]
    fn the_port_is_read_from_the_child_rather_than_chosen_for_it() {
        assert_eq!(
            parse_listening_port(
                "runner listening on http://127.0.0.1:51610 model=mlx/x idle=10.0m"
            ),
            Some(51610)
        );
        // Nothing else is this line.
        assert_eq!(
            parse_listening_port("runner listening on http://10.0.0.5:8080 m=x"),
            None
        );
        assert_eq!(
            parse_listening_port("runner listening on http://127.0.0.1:abc"),
            None
        );
        assert_eq!(
            parse_listening_port("runner listening on http://127.0.0.1:0 m=x"),
            None
        );
        assert_eq!(
            parse_listening_port("Traceback (most recent call last):"),
            None
        );
        assert_eq!(parse_listening_port(""), None);

        // The sidecar has to be printing the line this parses, on loopback.
        let source =
            std::fs::read_to_string(LocalRunner::script_path().expect("runner.py")).expect("read");
        assert!(
            source.contains("runner listening on http://%s:%d"),
            "the sidecar must publish its bound port"
        );
        assert!(
            source.contains("BIND_HOST = \"127.0.0.1\""),
            "and it has to be the host the parser accepts"
        );
        assert!(
            source.contains("server.server_address[1]"),
            "the port printed has to be the bound one, not the requested one"
        );
    }

    /// The model table is what Settings offers and what `local_model` stores,
    /// so the default and the keys are pinned.
    #[test]
    fn the_model_table_defaults_to_the_accurate_one() {
        assert_eq!(model_for("accurate").key, "accurate");
        assert_eq!(model_for("fast").repo, "mlx-community/Qwen3-ASR-0.6B-8bit");
        assert_eq!(
            model_for("nonesuch").repo,
            "mlx-community/Qwen3-ASR-1.7B-8bit",
            "an unknown key falls back to the shipped default"
        );
        for model in LOCAL_MODELS {
            assert!(model.repo.starts_with("mlx-community/"));
            assert!(!model.cost.is_empty(), "{} needs its cost", model.key);
            assert!(
                !model.short_cost.is_empty(),
                "{} needs a cost short enough for the menu",
                model.key
            );
        }
    }

    /// Discovery must never accept the 3.9 that ships with macOS, and must
    /// accept whatever it does return.
    #[test]
    fn discovery_only_accepts_a_supported_python() {
        assert!(python_version(Path::new("/definitely/not/python")).is_none());
        if let Some(found) = LocalRunner::find_python() {
            let version = python_version(&found).expect("a discovered interpreter answers");
            assert!(
                version >= MINIMUM_PYTHON,
                "{found:?} is {version:?}, older than the minimum"
            );
        }
        if let Some(version) = python_version(Path::new("/usr/bin/python3")) {
            if version < MINIMUM_PYTHON {
                assert_ne!(
                    LocalRunner::find_python().as_deref(),
                    Some(Path::new("/usr/bin/python3")),
                    "the system python is too old and must not be chosen"
                );
            }
        }
        assert!(LocalRunner::missing_python_message().contains("3.10"));
    }

    /// Stop during an install kills the install.
    ///
    /// The install's children -- `python -m venv`, then a 600 MB `pip install`
    /// -- are spawned by `run_with_progress`, never by `launch`, so they were
    /// never in `inner.pid` and `stop` had nothing to signal. Measured before
    /// the fix with this same stand-in: the child was still on the process
    /// table ten seconds after `stop` returned, while the window said
    /// "Stopped." and the Install button was live again.
    ///
    /// A shell that publishes its own pid and then becomes a long sleep stands
    /// in for pip. What is being measured is whether the process this
    /// supervisor started is still there, not what it was doing, and the pid
    /// comes from the child rather than from the supervisor because before the
    /// fix the supervisor did not know it -- which was the bug.
    #[cfg(unix)]
    #[test]
    fn stopping_a_setup_step_kills_the_child_it_started() {
        let directory =
            std::env::temp_dir().join(format!("openflow-runner-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("a temp directory");
        let pid_file = directory.join("child.pid");
        let recorder = Arc::new(Recorder::default());
        let events: Arc<dyn EngineEvents> = recorder.clone();
        let runner = LocalRunner::new(directory.clone(), events, "fast");
        let generation = runner.setup_generation();

        // `exec` so the pid published is the pid of the process that lives.
        let script = format!("echo $$ > '{}'; exec sleep 600", pid_file.display());
        let worker = {
            let runner = Arc::clone(&runner);
            std::thread::spawn(move || {
                runner.run_with_progress(
                    Command::new("/bin/sh").arg("-c").arg(&script),
                    RunnerPhase::Installing,
                    generation,
                )
            })
        };
        assert!(
            wait_for(Duration::from_secs(20), || published_pid(&pid_file)
                .is_some()),
            "the stand-in setup child never published its pid"
        );
        let pid = published_pid(&pid_file).expect("the stand-in's pid");

        runner.stop();
        let gone = wait_for(Duration::from_secs(10), || !alive(pid));
        if !gone {
            // Do not leave the orphan behind for the rest of the suite.
            // SAFETY: a pid this test's own supervisor spawned.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
        let outcome = worker.join().expect("the setup thread");
        let phase = runner.status().phase;
        let _ = std::fs::remove_dir_all(&directory);

        assert!(gone, "stop() left the setup child {pid} running");
        assert_eq!(
            outcome,
            Err(SETUP_STOPPED.to_string()),
            "a stopped step reports the stop, not a Python failure"
        );
        assert_eq!(
            phase,
            RunnerPhase::Stopped,
            "a stop the user asked for must not land in the status as a failure"
        );
    }

    /// The pid the stand-in wrote, once the write has finished.
    #[cfg(unix)]
    fn published_pid(path: &Path) -> Option<u32> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    /// And the stop sticks for the rest of the install.
    ///
    /// One install is `python -m venv` and then `pip install`. Killing the
    /// first child is only half of it: without this check the second one would
    /// spawn a moment later and download 600 MB into a venv the user has
    /// already stopped.
    ///
    /// The stand-in exits by itself rather than sleeping, deliberately: a
    /// regression here has to fail this test, not hang it. It leaves a file
    /// behind, so "did it run" is answered by the filesystem afterwards
    /// instead of by catching it while it lives.
    #[cfg(unix)]
    #[test]
    fn a_setup_step_that_starts_after_a_stop_never_spawns() {
        let directory =
            std::env::temp_dir().join(format!("openflow-runner-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("a temp directory");
        let evidence = directory.join("the-child-ran");
        let recorder = Arc::new(Recorder::default());
        let events: Arc<dyn EngineEvents> = recorder.clone();
        let runner = LocalRunner::new(directory.clone(), events, "fast");

        // The generation an install captures before its first child.
        let generation = runner.setup_generation();
        runner.stop();
        let script = format!("> '{}'", evidence.display());
        let outcome = runner.run_with_progress(
            Command::new("/bin/sh").arg("-c").arg(&script),
            RunnerPhase::Installing,
            generation,
        );
        let ran = evidence.exists();
        let _ = std::fs::remove_dir_all(&directory);

        assert!(!ran, "the step spawned a child after the stop");
        assert_eq!(outcome, Err(SETUP_STOPPED.to_string()));
    }

    /// The script has to be findable from a test binary, or the E2E path below
    /// cannot run and neither can a `cargo run` build.
    #[test]
    fn the_sidecar_script_is_found_from_the_source_tree() {
        let script = LocalRunner::script_path().expect("runner.py must be findable");
        assert!(script.ends_with("runner/runner.py"), "{script:?}");
        let source = std::fs::read_to_string(&script).expect("readable");
        assert!(
            source.contains("/v1/audio/transcriptions") && source.contains("127.0.0.1"),
            "the script found must be the sidecar"
        );
    }

    /// The end-to-end proof, off by default because it installs about 600 MB
    /// and loads a model.
    ///
    /// ```text
    /// OPENFLOW_RUNNER_E2E=1 cargo test -p openflow-core e2e -- --nocapture --ignored
    /// ```
    ///
    /// `OPENFLOW_RUNNER_E2E_DIR` reuses an install between runs (the install
    /// step is idempotent), `OPENFLOW_RUNNER_E2E_MODEL` picks `fast` or
    /// `accurate`, and `OPENFLOW_RUNNER_E2E_CLIP` is the wav to transcribe.
    #[test]
    #[ignore = "installs a Python environment and loads a model"]
    fn e2e_local_runner_transcribes_the_reference_clip() {
        if std::env::var("OPENFLOW_RUNNER_E2E").as_deref() != Ok("1") {
            eprintln!("skipped: set OPENFLOW_RUNNER_E2E=1 to run this");
            return;
        }
        let directory = std::env::var_os("OPENFLOW_RUNNER_E2E_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("openflow-runner-e2e"));
        let model = std::env::var("OPENFLOW_RUNNER_E2E_MODEL").unwrap_or_else(|_| "fast".into());
        let clip = PathBuf::from(
            std::env::var("OPENFLOW_RUNNER_E2E_CLIP").expect("OPENFLOW_RUNNER_E2E_CLIP"),
        );
        let wav = std::fs::read(&clip).expect("the reference clip");

        let recorder = Arc::new(Recorder::default());
        let events: Arc<dyn EngineEvents> = recorder.clone();
        let runner = LocalRunner::new(directory, events, &model);
        let repo = runner.model_repo();
        eprintln!("e2e model={repo}");

        let started = Instant::now();
        runner.install().expect("install the environment");
        eprintln!("e2e install: {} ms", started.elapsed().as_millis());
        let started = Instant::now();
        runner.download().expect("find or download the weights");
        eprintln!("e2e download/find: {} ms", started.elapsed().as_millis());

        let started = Instant::now();
        let port = runner.start(READY_TIMEOUT).expect("start the sidecar");
        eprintln!("e2e spawn to ready: {} ms", started.elapsed().as_millis());
        runner.prewarm();

        let provider =
            crate::transcribe::Provider::from_str(&format!("custom:http://127.0.0.1:{port}/v1"));
        let runtime = tokio::runtime::Runtime::new().expect("runtime");

        let cold = Instant::now();
        let first = runtime
            .block_on(crate::transcribe::transcribe_audio(
                wav.clone(),
                "",
                None,
                &provider,
                Some(&repo),
                None,
            ))
            .expect("the first transcription");
        let cold = cold.elapsed();
        assert!(!first.trim().is_empty(), "the runner returned no text");
        eprintln!("e2e cold: {} ms\ne2e text: {first}", cold.as_millis());

        let mut warm = Vec::new();
        for _ in 0..3 {
            let started = Instant::now();
            let text = runtime
                .block_on(crate::transcribe::transcribe_audio(
                    wav.clone(),
                    "",
                    None,
                    &provider,
                    Some(&repo),
                    None,
                ))
                .expect("a warm transcription");
            warm.push(started.elapsed().as_millis());
            assert!(!text.trim().is_empty());
        }
        eprintln!("e2e warm: {warm:?} ms");
        runner.refresh_health();
        eprintln!(
            "e2e health: {:?} resident={:?}",
            runner.status().phase,
            runner.status().resident_bytes
        );
        runner.stop();
    }
}
