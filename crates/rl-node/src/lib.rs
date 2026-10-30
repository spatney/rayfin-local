//! Node.js integration: finding the developer's `node`, materializing the JavaScript helpers
//! embedded in the binary, and supervising child processes (whole process trees, output streamed
//! line by line with a label).
//!
//! Everything rayfin-local runs besides itself goes through this crate: the one-shot helpers
//! (`js/*.mjs`, see [`run_helper`]), the function host, the functions build, and Vite.
//!
//! The public API below is a contract other crates code against; extend it, but keep these
//! signatures.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::{
    collections::VecDeque,
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    task::JoinHandle,
    time::timeout,
};

/// The oldest Node.js major version rayfin-local supports.
pub const MIN_NODE_MAJOR: u32 = 20;

/// Lines longer than this are split, so a child that never prints a newline can't grow memory.
const MAX_LINE: usize = 64 * 1024;
/// How many stderr lines of a failed helper are kept for its error message.
const TAIL_LINES: usize = 20;
/// How long output still in the pipes may take to arrive once a process has exited.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

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
        let not_found = || NodeError::NotFound {
            min: MIN_NODE_MAJOR,
        };
        let path = match std::env::var_os("RAYFIN_LOCAL_NODE").filter(|p| !p.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => which::which("node").map_err(|_| not_found())?,
        };
        let output = Command::new(&path)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|source| match source.kind() {
                io::ErrorKind::NotFound => not_found(),
                _ => NodeError::Spawn {
                    program: path.display().to_string(),
                    source,
                },
            })?;
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let version = raw.trim_start_matches('v').to_owned();
        let major = version
            .split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok())
            .ok_or_else(|| NodeError::Spawn {
                program: path.display().to_string(),
                source: io::Error::other(format!("unexpected `node --version` output {raw:?}")),
            })?;
        if major < MIN_NODE_MAJOR {
            return Err(NodeError::TooOld {
                found: version,
                min: MIN_NODE_MAJOR,
            });
        }
        Ok(Self {
            path,
            version,
            major,
        })
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
    ///
    /// A name with a `.mjs`, `.cjs`, `.js` or `.json` extension keeps it:
    /// `shim.cjs` → `<cache_dir>/js/shim-<content hash>.cjs`.
    pub fn materialize(&self, cache_dir: &Path) -> Result<PathBuf, NodeError> {
        let (stem, extension) = split_extension(self.name);
        let file_name = format!("{stem}-{}.{extension}", content_hash(&[self.source]));
        write_once(&cache_dir.join("js"), &file_name, self.source)
    }
}

/// Writes helpers that import each other by relative path (`import './shim.cjs'`) into one
/// folder, each under its exact `name`: `<cache_dir>/js/<bundle>-<hash of all sources>/<name>`.
/// Returns the folder. Safe to call concurrently.
pub fn materialize_bundle(
    cache_dir: &Path,
    bundle: &str,
    helpers: &[Helper],
) -> Result<PathBuf, NodeError> {
    let mut parts = Vec::with_capacity(helpers.len() * 2);
    for helper in helpers {
        parts.push(helper.name);
        parts.push(helper.source);
    }
    let dir = cache_dir
        .join("js")
        .join(format!("{bundle}-{}", content_hash(&parts)));
    for helper in helpers {
        write_once(&dir, helper.name, helper.source)?;
    }
    Ok(dir)
}

fn split_extension(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((stem, extension)) if matches!(extension, "mjs" | "cjs" | "js" | "json") => {
            (stem, extension)
        }
        _ => (name, "mjs"),
    }
}

/// The first 12 hex digits of the SHA-256 of the parts (length-prefixed, so boundaries count).
fn content_hash(parts: &[&str]) -> String {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    for part in parts {
        context.update(&(part.len() as u64).to_le_bytes());
        context.update(part.as_bytes());
    }
    hex::encode(&context.finish().as_ref()[..6])
}

/// Writes `contents` to `dir/file_name` atomically unless the file exists. File names carry a
/// content hash (or live in a content-hashed folder), so an existing file has the right content.
fn write_once(dir: &Path, file_name: &str, contents: &str) -> Result<PathBuf, NodeError> {
    let path = dir.join(file_name);
    if path.is_file() {
        return Ok(path);
    }
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".{file_name}.{}.tmp", unique_suffix()));
    std::fs::write(&temp, contents)?;
    match std::fs::rename(&temp, &path) {
        Ok(()) => Ok(path),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            // A concurrent writer may have won (and may hold the file open on Windows).
            if path.is_file() {
                Ok(path)
            } else {
                Err(error.into())
            }
        }
    }
}

fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!(
        "{}-{}-{nanos}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
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
    Arc::new(|label, _stream, line| {
        let stderr = io::stderr();
        let mut stderr = stderr.lock();
        let _ = writeln!(stderr, "[{label}] {line}");
    })
}

/// `line` without terminal escape sequences (colors, cursor moves, hyperlinks), which children
/// print when they think they write to a terminal.
pub fn strip_ansi(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains('\x1b') {
        return std::borrow::Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, up to a final byte in `@`..=`~`.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC (such as a hyperlink): up to BEL or ST (`ESC \`).
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    std::borrow::Cow::Owned(out)
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

/// What survives `env_clear`: what programs (and Node) need to start and find tools.
#[cfg(windows)]
const PASSTHROUGH_ENV: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "HOMEDRIVE",
    "HOMEPATH",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "CommonProgramFiles",
    "PROCESSOR_ARCHITECTURE",
    "NUMBER_OF_PROCESSORS",
    "OS",
];
#[cfg(not(windows))]
const PASSTHROUGH_ENV: &[&str] = &["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "USER", "SHELL"];

impl ProcessSpec {
    /// Runs `program` in `cwd`, inheriting this process's environment, output to [`default_sink`].
    pub fn new(
        label: impl Into<String>,
        program: impl Into<PathBuf>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            label: label.into(),
            program: program.into(),
            args: Vec::new(),
            shell_command: None,
            cwd: cwd.into(),
            env: Vec::new(),
            env_remove: Vec::new(),
            env_clear: false,
            sink: default_sink(),
        }
    }

    /// Runs `command_line` with the platform shell in `cwd`.
    pub fn shell(
        label: impl Into<String>,
        command_line: impl Into<String>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        let mut spec = Self::new(label, PathBuf::new(), cwd);
        spec.shell_command = Some(command_line.into());
        spec
    }

    /// Runs `node <script>` with the given runtime in `cwd`.
    pub fn node(
        label: impl Into<String>,
        node: &NodeRuntime,
        script: &Path,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self::new(label, node.path.clone(), cwd).arg(script.as_os_str())
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

    /// What the spec runs, for messages.
    fn describe(&self) -> String {
        match &self.shell_command {
            Some(line) => line.clone(),
            None => {
                let mut text = self.program.display().to_string();
                for arg in &self.args {
                    text.push(' ');
                    text.push_str(&arg.to_string_lossy());
                }
                text
            }
        }
    }

    fn command(&self) -> Command {
        let mut command = match &self.shell_command {
            Some(line) => shell_command(line),
            None => {
                let mut command = Command::new(&self.program);
                command.args(&self.args);
                command
            }
        };
        if self.env_clear {
            command.env_clear();
            for key in PASSTHROUGH_ENV {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
        }
        for key in &self.env_remove {
            command.env_remove(key);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }
}

#[cfg(windows)]
fn shell_command(line: &str) -> Command {
    let shell = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
    let mut command = Command::new(shell);
    // `/s` makes cmd strip exactly the outer quotes and keep the rest of the line verbatim.
    command.raw_arg("/d /s /c").raw_arg(format!("\"{line}\""));
    command
}

#[cfg(not(windows))]
fn shell_command(line: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(line);
    command
}

/// A running child process. [`ManagedChild::kill`] and dropping the handle end its whole process
/// tree (Windows job objects, Unix process groups), so grandchildren such as esbuild or
/// `npm run` scripts never outlive it.
pub struct ManagedChild {
    child: tokio::process::Child,
    label: String,
    pid: Option<u32>,
    readers: Vec<JoinHandle<()>>,
    status: Option<ExitStatus>,
    #[cfg(windows)]
    job: Option<windows::Job>,
    /// Shell commands on Windows get no Ctrl+Break: cmd.exe running a batch file such as
    /// `npm.cmd` answers it with "Terminate batch job (Y/N)?" instead of stopping.
    #[cfg(windows)]
    graceful_stop: bool,
}

impl ManagedChild {
    /// Starts the process; its stdout and stderr are streamed to the spec's sink. Must be called
    /// within a Tokio runtime.
    pub fn spawn(spec: ProcessSpec) -> Result<Self, NodeError> {
        let mut command = spec.command();
        command
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(windows::CREATION_FLAGS);
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn().map_err(|source| NodeError::Spawn {
            program: spec.describe(),
            source,
        })?;
        #[cfg(windows)]
        let job = windows::attach_job(&child, &spec.label);
        let pid = child.id();
        tracing::debug!(label = %spec.label, pid, cwd = %spec.cwd.display(), "started `{}`", spec.describe());

        let label: Arc<str> = Arc::from(spec.label.as_str());
        let mut readers = Vec::with_capacity(2);
        if let Some(stdout) = child.stdout.take() {
            readers.push(tokio::spawn(pump(
                stdout,
                Arc::clone(&label),
                Stream::Stdout,
                Arc::clone(&spec.sink),
            )));
        }
        if let Some(stderr) = child.stderr.take() {
            readers.push(tokio::spawn(pump(
                stderr,
                label,
                Stream::Stderr,
                Arc::clone(&spec.sink),
            )));
        }
        Ok(Self {
            child,
            label: spec.label,
            pid,
            readers,
            status: None,
            #[cfg(windows)]
            job,
            #[cfg(windows)]
            graceful_stop: spec.shell_command.is_none(),
        })
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The spec's label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Waits for the process to exit (output already streamed is flushed to the sink first).
    ///
    /// When the process exits on its own, processes it started and left running (a browser it
    /// opened, for example) are released rather than killed; [`ManagedChild::kill`] still ends
    /// them.
    pub async fn wait(&mut self) -> Result<ExitStatus, NodeError> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let status = self.child.wait().await?;
        self.exited(status);
        self.drain_readers(DRAIN_TIMEOUT).await;
        Ok(status)
    }

    /// Records a natural exit and releases what the process left running.
    fn exited(&mut self, status: ExitStatus) {
        self.status = Some(status);
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.release();
        }
    }

    /// The exit status if the process has exited, without waiting.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, NodeError> {
        if self.status.is_none()
            && let Some(status) = self.child.try_wait()?
        {
            self.exited(status);
        }
        Ok(self.status)
    }

    /// Asks the process tree to stop, then force-kills whatever is left after `grace`.
    pub async fn kill(&mut self, grace: Duration) -> Result<(), NodeError> {
        if self.status.is_none()
            && !grace.is_zero()
            && self.request_stop()
            && let Ok(Ok(status)) = timeout(grace, self.child.wait()).await
        {
            self.status = Some(status);
        }
        // Even after the leader exits, grandchildren may remain in the tree.
        self.terminate_tree();
        if self.status.is_none() {
            let _ = self.child.start_kill();
            if let Ok(Ok(status)) = timeout(Duration::from_secs(5), self.child.wait()).await {
                self.status = Some(status);
            }
        }
        self.drain_readers(Duration::from_secs(1)).await;
        tracing::debug!(label = %self.label, pid = self.pid, "stopped");
        Ok(())
    }

    /// Sends the graceful stop request; `false` when it could not be delivered.
    fn request_stop(&self) -> bool {
        let Some(pid) = self.pid else {
            return false;
        };
        #[cfg(windows)]
        {
            self.graceful_stop && windows::send_ctrl_break(pid)
        }
        #[cfg(unix)]
        {
            unix::signal_group(pid, libc::SIGTERM)
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = pid;
            false
        }
    }

    /// Force-kills every process left in the tree.
    fn terminate_tree(&self) {
        #[cfg(windows)]
        match (&self.job, self.pid) {
            (Some(job), _) => job.terminate(),
            (None, Some(pid)) if self.status.is_none() => windows::taskkill_tree(pid),
            _ => {}
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unix::signal_group(pid, libc::SIGKILL);
        }
    }

    /// Waits up to `limit` for the output readers to reach the end of the pipes. A grandchild
    /// that inherited a pipe can keep it open after the process exits; then the rest is dropped.
    async fn drain_readers(&mut self, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        for reader in self.readers.drain(..) {
            let abort = reader.abort_handle();
            if tokio::time::timeout_at(deadline, reader).await.is_err() {
                abort.abort();
            }
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        #[cfg(windows)]
        match self.job.take() {
            // Closing the last handle of the kill-on-close job ends the tree.
            Some(job) => drop(job),
            None => {
                if let (Some(pid), None) = (self.pid, self.status) {
                    windows::taskkill_tree(pid);
                }
            }
        }
        #[cfg(unix)]
        if let (Some(pid), None) = (self.pid, self.status) {
            // Only while the leader is unreaped: its pid (the group id) can't be reused yet.
            unix::signal_group(pid, libc::SIGKILL);
        }
        for reader in &self.readers {
            reader.abort();
        }
    }
}

/// Forwards a pipe to the sink line by line.
async fn pump<R>(reader: R, label: Arc<str>, stream: Stream, sink: LineSink)
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::with_capacity(16 * 1024, reader);
    let mut line = Vec::new();
    loop {
        let available = match reader.fill_buf().await {
            Ok(available) if !available.is_empty() => available,
            _ => break,
        };
        let (used, complete) = match available.iter().position(|&b| b == b'\n') {
            Some(end) => {
                line.extend_from_slice(&available[..end]);
                (end + 1, true)
            }
            None => {
                line.extend_from_slice(available);
                (available.len(), false)
            }
        };
        reader.consume(used);
        if complete || line.len() >= MAX_LINE {
            emit(&label, stream, &sink, &mut line);
        }
    }
    if !line.is_empty() {
        emit(&label, stream, &sink, &mut line);
    }
}

fn emit(label: &str, stream: Stream, sink: &LineSink, line: &mut Vec<u8>) {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    sink(label, stream, &String::from_utf8_lossy(line));
    line.clear();
}

/// Runs a process to completion, streaming its output; fails with [`NodeError::Failed`] on a
/// non-zero exit.
pub async fn run_to_completion(spec: ProcessSpec) -> Result<(), NodeError> {
    let label = spec.label.clone();
    let mut child = ManagedChild::spawn(spec)?;
    let status = child.wait().await?;
    if status.success() {
        Ok(())
    } else {
        Err(NodeError::Failed { label, status })
    }
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
    let script = helper.materialize(cache_dir)?;
    let temp_dir = cache_dir.join("tmp");
    create_private_dir(&temp_dir)?;
    let suffix = unique_suffix();
    let input_path = temp_dir.join(format!("{}-{suffix}.in.json", helper.name));
    let output_path = temp_dir.join(format!("{}-{suffix}.out.json", helper.name));
    let _cleanup = RemoveOnDrop(vec![input_path.clone(), output_path.clone()]);
    std::fs::write(
        &input_path,
        serde_json::to_vec(input).map_err(io::Error::other)?,
    )?;

    let tail = Arc::new(Mutex::new(VecDeque::with_capacity(TAIL_LINES)));
    let recorder = {
        let tail = Arc::clone(&tail);
        let forward = sink;
        let recorder: LineSink = Arc::new(move |label, stream, line| {
            if stream == Stream::Stderr {
                let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
                if tail.len() == TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line.to_owned());
            }
            forward(label, stream, line);
        });
        recorder
    };
    let spec = ProcessSpec::node(helper.name, node, &script, cwd)
        .arg("--input")
        .arg(&input_path)
        .arg("--output")
        .arg(&output_path)
        .sink(recorder);
    let mut child = ManagedChild::spawn(spec)?;
    let status = child.wait().await?;

    let fail = |message: String| NodeError::Helper {
        helper: helper.name.to_owned(),
        message,
    };
    let Ok(bytes) = std::fs::read(&output_path) else {
        let tail = tail.lock().unwrap_or_else(|e| e.into_inner());
        let detail = if tail.is_empty() {
            String::new()
        } else {
            format!(":\n{}", tail.iter().cloned().collect::<Vec<_>>().join("\n"))
        };
        return Err(fail(format!(
            "exited with {status} without writing a result{detail}"
        )));
    };
    let envelope: Envelope = serde_json::from_slice(&bytes)
        .map_err(|e| fail(format!("wrote an unreadable result: {e}")))?;
    if envelope.ok {
        serde_json::from_value(envelope.result.unwrap_or(serde_json::Value::Null))
            .map_err(|e| fail(format!("returned an unexpected result: {e}")))
    } else {
        let error = envelope.error.unwrap_or_default();
        if let Some(stack) = &error.stack {
            tracing::debug!(helper = helper.name, "helper stack: {stack}");
        }
        Err(fail(error.message.unwrap_or_else(|| {
            format!("failed without a message ({status})")
        })))
    }
}

#[derive(serde::Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<EnvelopeError>,
}

#[derive(serde::Deserialize, Default)]
struct EnvelopeError {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    stack: Option<String>,
}

struct RemoveOnDrop(Vec<PathBuf>);

/// Creates `dir` readable only by the current user where the platform has such permissions:
/// helper results can carry access tokens.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Makes every child this process starts die with it, even on a crash.
///
/// Nothing to set up today: on Windows each [`ManagedChild`] runs in its own kill-on-close job
/// object whose only handle rayfin-local holds, and the OS closes it (ending the tree) however
/// rayfin-local exits; a job for the whole process would also kill browsers opened with `--open`.
/// On Unix children lead their own process group, stopped by [`ManagedChild::kill`] and on drop;
/// a hard kill of rayfin-local can leave them running. Kept as the place for future hardening.
pub fn init_process_supervision() -> Result<(), NodeError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_kept_and_default_to_mjs() {
        assert_eq!(split_extension("analyze"), ("analyze", "mjs"));
        assert_eq!(split_extension("shim.cjs"), ("shim", "cjs"));
        assert_eq!(split_extension("rayfin-token"), ("rayfin-token", "mjs"));
        assert_eq!(split_extension("v1.2"), ("v1.2", "mjs"));
    }

    #[test]
    fn content_hash_depends_on_boundaries() {
        assert_ne!(content_hash(&["ab", "c"]), content_hash(&["a", "bc"]));
        assert_eq!(content_hash(&["x"]).len(), 12);
    }

    #[test]
    fn escape_sequences_are_stripped() {
        assert!(matches!(
            strip_ansi("plain"),
            std::borrow::Cow::Borrowed("plain")
        ));
        assert_eq!(
            strip_ansi(
                "\x1b[32m  ➜\x1b[39m  \x1b[1mLocal\x1b[22m: \x1b[36mhttp://localhost:5173/\x1b[39m"
            ),
            "  ➜  Local: http://localhost:5173/"
        );
        assert_eq!(
            strip_ansi("\x1b]8;;https://vite.dev\x07docs\x1b]8;;\x1b\\ \x1b[2K\x1b[1Gdone"),
            "docs done"
        );
    }
}
