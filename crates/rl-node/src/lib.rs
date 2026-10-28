//! Node.js integration: finding the developer's `node`, materializing the JavaScript helpers
//! embedded in the binary, and supervising child processes (whole process trees, output streamed
//! line by line with a label).
//!
//! Everything rayfin-local runs besides itself goes through this crate: the one-shot helpers
//! (`js/*.mjs`, see [`run_helper`]), the function host, the functions build, and Vite.
//!
//! The public API below is a contract other crates code against; extend it, but keep these
//! signatures.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
    time::Duration,
};

/// The oldest Node.js major version rayfin-local supports.
pub const MIN_NODE_MAJOR: u32 = 20;

/// A Node.js or child-process failure.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error(
        "Node.js was not found. Install Node.js {min} or later (https://nodejs.org), or set \
         RAYFIN_LOCAL_NODE to the node executable."
    )]
    NotFound { min: u32 },
    #[error("Node.js {found} is too old: rayfin-local needs Node.js {min} or later.")]
    TooOld { found: String, min: u32 },
    #[error("could not start `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{label}` exited with {status}")]
    Failed { label: String, status: ExitStatus },
    #[error("helper `{helper}` failed: {message}")]
    Helper { helper: String, message: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The Node.js runtime used for helpers, the function host and Vite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRuntime {
    /// The `node` executable.
    pub path: PathBuf,
    /// The version without the leading `v`, e.g. `24.22.1`.
    pub version: String,
    pub major: u32,
}

impl NodeRuntime {
    /// Finds Node.js: `RAYFIN_LOCAL_NODE` if set, else `node` on `PATH`; checks the version is
    /// at least [`MIN_NODE_MAJOR`].
    pub async fn discover() -> Result<Self, NodeError> {
        todo!("node-helpers-infra")
    }
}

/// A JavaScript helper compiled into the binary, e.g.
/// `Helper::new("analyze", include_str!("../../../js/analyze.mjs"))`.
#[derive(Debug, Clone, Copy)]
pub struct Helper {
    pub name: &'static str,
    pub source: &'static str,
}

impl Helper {
    pub const fn new(name: &'static str, source: &'static str) -> Self {
        Self { name, source }
    }

    /// Writes the helper to `<cache_dir>/js/<name>-<content hash>.mjs` unless it is already
    /// there, and returns that path. Safe to call concurrently.
    pub fn materialize(&self, cache_dir: &Path) -> Result<PathBuf, NodeError> {
        let _ = cache_dir;
        todo!("node-helpers-infra")
    }
}

/// Which output stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Receives a child's output one line at a time (without the line terminator): the spec's
/// label, the stream, and the line.
pub type LineSink = Arc<dyn Fn(&str, Stream, &str) + Send + Sync>;

/// The default sink: writes `[label] line` to this process's stderr.
pub fn default_sink() -> LineSink {
    todo!("node-helpers-infra")
}

/// What to run, where, and with which environment.
#[derive(Clone)]
pub struct ProcessSpec {
    /// Short name used to prefix log lines, e.g. `vite`, `fn`, `build`.
    pub label: String,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// When set, `program`/`args` are ignored and this command line runs in the platform shell
    /// (`cmd.exe /d /s /c` on Windows, `sh -c` elsewhere), e.g. `npm run build`.
    pub shell_command: Option<String>,
    pub cwd: PathBuf,
    /// Variables to set (after `env_clear`, if set).
    pub env: Vec<(OsString, OsString)>,
    /// Variables to remove from the inherited environment.
    pub env_remove: Vec<OsString>,
    /// Start from an empty environment (`PATH` and the variables Windows needs to run programs,
    /// such as `SystemRoot`, are still passed through).
    pub env_clear: bool,
    /// Where stdout and stderr lines go.
    pub sink: LineSink,
}

impl ProcessSpec {
    /// Runs `program` in `cwd`, inheriting this process's environment, output to [`default_sink`].
    pub fn new(
        label: impl Into<String>,
        program: impl Into<PathBuf>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        let _ = (label.into(), program.into(), cwd.into());
        todo!("node-helpers-infra")
    }

    /// Runs `command_line` with the platform shell in `cwd`.
    pub fn shell(
        label: impl Into<String>,
        command_line: impl Into<String>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        let _ = (label.into(), command_line.into(), cwd.into());
        todo!("node-helpers-infra")
    }

    /// Runs `node <script>` with the given runtime in `cwd`.
    pub fn node(
        label: impl Into<String>,
        node: &NodeRuntime,
        script: &Path,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        let _ = (label.into(), node, script, cwd.into());
        todo!("node-helpers-infra")
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn sink(mut self, sink: LineSink) -> Self {
        self.sink = sink;
        self
    }
}

/// A running child process. [`ManagedChild::kill`] and dropping the handle end its whole process
/// tree (Windows job objects, Unix process groups), so grandchildren such as esbuild or
/// `npm run` scripts never outlive it.
pub struct ManagedChild {
    _private: (),
}

impl ManagedChild {
    /// Starts the process; its stdout and stderr are streamed to the spec's sink.
    pub fn spawn(spec: ProcessSpec) -> Result<Self, NodeError> {
        let _ = spec;
        todo!("node-helpers-infra")
    }

    pub fn pid(&self) -> Option<u32> {
        todo!("node-helpers-infra")
    }

    /// Waits for the process to exit (output already streamed is flushed to the sink first).
    pub async fn wait(&mut self) -> Result<ExitStatus, NodeError> {
        todo!("node-helpers-infra")
    }

    /// Asks the process tree to stop, then force-kills whatever is left after `grace`.
    pub async fn kill(&mut self, grace: Duration) -> Result<(), NodeError> {
        let _ = grace;
        todo!("node-helpers-infra")
    }
}

/// Runs a process to completion, streaming its output; fails with [`NodeError::Failed`] on a
/// non-zero exit.
pub async fn run_to_completion(spec: ProcessSpec) -> Result<(), NodeError> {
    let _ = spec;
    todo!("node-helpers-infra")
}

/// Runs a one-shot helper: `node <helper> --input <in.json> --output <out.json>`, in `cwd`.
///
/// The helper reads its input (`input`, serialized) from the `--input` file and writes exactly one
/// JSON document to the `--output` file: `{"ok": true, "result": <value>}` or
/// `{"ok": false, "error": {"message": "...", "stack": "..."}}`. Its stdout and stderr are free for
/// logs (imported user modules may print) and go to `sink`. Returns `result`, deserialized.
pub async fn run_helper<T: serde::de::DeserializeOwned>(
    node: &NodeRuntime,
    helper: &Helper,
    cache_dir: &Path,
    cwd: &Path,
    input: &serde_json::Value,
    sink: LineSink,
) -> Result<T, NodeError> {
    let _ = (node, helper, cache_dir, cwd, input, sink);
    todo!("node-helpers-infra")
}

/// Makes every child this process starts die with it, even on a crash (Windows: a kill-on-close
/// job object for the whole process; elsewhere: best effort). Call once at startup.
pub fn init_process_supervision() -> Result<(), NodeError> {
    todo!("node-helpers-infra")
}
