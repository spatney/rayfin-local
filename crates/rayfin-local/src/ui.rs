//! Terminal output: status lines, warnings, errors, child-process output and the startup banner.
//!
//! Colors are plain ANSI codes, used only when the stream is a terminal and `NO_COLOR` is unset.

use std::fmt::Display;
use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, OnceLock};

use rl_node::{LineSink, Stream};

#[derive(Clone, Copy)]
struct ColorSupport {
    stdout: bool,
    stderr: bool,
}

static COLORS: OnceLock<ColorSupport> = OnceLock::new();

/// Decides once whether stdout and stderr get colors (and turns ANSI codes on in a Windows
/// console). Call before printing anything.
pub fn init() {
    colors();
}

fn colors() -> ColorSupport {
    *COLORS.get_or_init(|| {
        let allowed = std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty());
        ColorSupport {
            stdout: allowed && io::stdout().is_terminal() && enable_ansi(StdStream::Stdout),
            stderr: allowed && io::stderr().is_terminal() && enable_ansi(StdStream::Stderr),
        }
    })
}

/// Whether stderr (status lines and logs) gets colors.
pub fn stderr_colors() -> bool {
    colors().stderr
}

#[derive(Clone, Copy)]
enum StdStream {
    Stdout,
    Stderr,
}

#[cfg(windows)]
fn enable_ansi(stream: StdStream) -> bool {
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle,
        STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    let which = match stream {
        StdStream::Stdout => STD_OUTPUT_HANDLE,
        StdStream::Stderr => STD_ERROR_HANDLE,
    };
    // SAFETY: console-mode calls on this process's own standard handle, with a valid out pointer.
    unsafe {
        let handle = GetStdHandle(which);
        let mut mode: CONSOLE_MODE = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return false;
        }
        mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
            || SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
fn enable_ansi(_stream: StdStream) -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Bold,
    Dim,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
}

impl Color {
    fn code(self) -> &'static str {
        match self {
            Color::Bold => "1",
            Color::Dim => "2",
            Color::Red => "31",
            Color::Green => "32",
            Color::Yellow => "33",
            Color::Blue => "34",
            Color::Magenta => "35",
            Color::Cyan => "36",
        }
    }
}

/// Paints text for one output stream.
#[derive(Clone, Copy)]
pub struct Painter {
    enabled: bool,
}

impl Painter {
    pub fn stdout() -> Self {
        Self {
            enabled: colors().stdout,
        }
    }

    pub fn stderr() -> Self {
        Self {
            enabled: colors().stderr,
        }
    }

    /// A painter for a stream known to take colors or not.
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    pub fn paint(self, color: Color, text: impl Display) -> String {
        if self.enabled {
            format!("\x1b[{}m{text}\x1b[0m", color.code())
        } else {
            text.to_string()
        }
    }
}

/// A finished startup step: `✓ message`.
pub fn step(message: impl Display) {
    let painter = Painter::stderr();
    eprintln!("{} {message}", painter.paint(Color::Green, "✓"));
}

/// A plain status line.
pub fn status(message: impl Display) {
    eprintln!("{message}");
}

/// `warning: message`.
pub fn warn(message: impl Display) {
    let painter = Painter::stderr();
    eprintln!("{} {message}", painter.paint(Color::Yellow, "warning:"));
}

/// `error: message`, then each cause on its own line (causes whose text the previous messages
/// already include are left out).
pub fn error(err: &anyhow::Error) {
    let painter = Painter::stderr();
    let mut shown: Vec<String> = Vec::new();
    for cause in err.chain() {
        let text = cause.to_string();
        let text = text.trim();
        if text.is_empty() || shown.iter().any(|earlier| earlier.contains(text)) {
            continue;
        }
        shown.push(text.to_owned());
    }
    let mut lines = shown.iter();
    let first = lines.next().map_or("unknown error", String::as_str);
    let mut out = format!("{} {first}\n", painter.paint(Color::Red, "error:"));
    for cause in lines {
        for line in cause.lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }
    let _ = io::stderr().lock().write_all(out.as_bytes());
}

/// The sink for child processes and helpers: `[label] line` on stderr, each label in its own
/// stable color.
pub fn child_sink() -> LineSink {
    Arc::new(|label: &str, _stream: Stream, line: &str| {
        let tag = Painter::stderr().paint(label_color(label), format_args!("[{label}]"));
        let _ = writeln!(io::stderr().lock(), "{tag} {line}");
    })
}

fn label_color(label: &str) -> Color {
    const PALETTE: [Color; 5] = [
        Color::Cyan,
        Color::Magenta,
        Color::Blue,
        Color::Yellow,
        Color::Green,
    ];
    // FNV-1a: the same label gets the same color on every run.
    let hash = label.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    PALETTE[hash as usize % PALETTE.len()]
}

/// What the startup banner shows: the app's name, then labeled rows (URLs in color).
pub struct Banner<'a> {
    pub app_name: &'a str,
    pub rows: Vec<(&'static str, String)>,
}

pub fn banner(banner: &Banner<'_>) {
    let painter = Painter::stderr();
    let width = banner
        .rows
        .iter()
        .map(|(label, _)| label.len())
        .max()
        .unwrap_or(0)
        + 2;
    let mut out = format!(
        "\n  {} is running locally\n\n",
        painter.paint(Color::Bold, banner.app_name)
    );
    for (label, value) in &banner.rows {
        let label = painter.paint(Color::Bold, format_args!("{label:<width$}"));
        let value = if value.starts_with("http://") || value.starts_with("https://") {
            painter.paint(Color::Cyan, value)
        } else {
            value.clone()
        };
        out.push_str(&format!("  {label}{value}\n"));
    }
    out.push_str(&format!(
        "\n  {}\n\n",
        painter.paint(Color::Dim, "Press Ctrl+C to stop.")
    ));
    let _ = io::stderr().lock().write_all(out.as_bytes());
}
