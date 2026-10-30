//! What the functions are doing, for hosts ([`FunctionsConfig::on_state`]): building, ready or
//! failed, with the compiler's messages.
//!
//! [`FunctionsConfig::on_state`]: crate::FunctionsConfig::on_state

use std::sync::{Arc, Mutex, PoisonError};

/// What the functions are doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FunctionsState {
    /// The package is being built.
    Building,
    /// The function host runs the package; `names` are the functions the routes invoke.
    Ready { names: Vec<String> },
    /// The build (`build: true`) or the loading of the package failed. `errors` are the
    /// compiler's messages, one per message; `names` are the functions a previous build still
    /// serves.
    Failed {
        message: String,
        errors: Vec<String>,
        build: bool,
        names: Vec<String>,
    },
}

/// Receives each new [`FunctionsState`].
pub type StateObserver = Arc<dyn Fn(&FunctionsState) + Send + Sync>;

/// Tells the observer about each new state: not a second `Building` in a row, and not the same
/// failure again (`tsc --watch` reports a failed build once more when it starts).
pub(crate) struct Reporter {
    observer: Option<StateObserver>,
    last: Mutex<Option<FunctionsState>>,
}

impl Reporter {
    pub(crate) fn new(observer: Option<StateObserver>) -> Self {
        Self {
            observer,
            last: Mutex::new(None),
        }
    }

    pub(crate) fn report(&self, state: FunctionsState) {
        let Some(observer) = &self.observer else {
            return;
        };
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let repeated = match (&*last, &state) {
            (Some(FunctionsState::Building), FunctionsState::Building) => true,
            (Some(failed @ FunctionsState::Failed { .. }), FunctionsState::Failed { .. }) => {
                *failed == state
            }
            _ => false,
        };
        if !repeated {
            // Under the lock, so that observers see the states in order.
            observer(&state);
            *last = Some(state);
        }
    }

    /// The state reported last.
    pub(crate) fn last(&self) -> Option<FunctionsState> {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// A milestone of `tsc --watch` in the build's output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Milestone {
    /// A compilation started: `File change detected. Starting incremental compilation...`
    /// (`announce`), or the first one, `Starting compilation in watch mode...`, which repeats the
    /// build that just ran.
    Started { announce: bool },
    /// `Found N errors. Watching for file changes.`, with the messages of that compilation.
    Finished { count: usize, errors: Vec<String> },
}

/// Follows the build's output for the compiler's messages (`tsc`'s
/// `src/app.ts(3,7): error TS2322: …`, with their indented continuation lines) and the
/// milestones of `tsc --watch`.
#[derive(Debug, Default)]
pub(crate) struct CompilerOutput {
    messages: Vec<String>,
    /// Whether the last message may continue on the next line.
    open: bool,
}

impl CompilerOutput {
    pub(crate) fn line(&mut self, line: &str) -> Option<Milestone> {
        let line = rl_node::strip_ansi(line);
        let text = line.trim_end();
        let message = watch_message(text);
        if message.starts_with("File change detected") {
            self.take();
            return Some(Milestone::Started { announce: true });
        }
        if message.starts_with("Starting compilation in watch mode") {
            self.take();
            return Some(Milestone::Started { announce: false });
        }
        if let Some(count) = found_errors(message) {
            return Some(Milestone::Finished {
                count,
                errors: self.take(),
            });
        }
        if is_compiler_error(text) {
            self.messages.push(text.trim_start().to_owned());
            self.open = true;
        } else if self.open && text.starts_with(char::is_whitespace) && !text.trim().is_empty() {
            if let Some(last) = self.messages.last_mut() {
                last.push('\n');
                last.push_str(text);
            }
        } else {
            self.open = false;
        }
        None
    }

    /// The messages since the last call.
    pub(crate) fn take(&mut self) -> Vec<String> {
        self.open = false;
        std::mem::take(&mut self.messages)
    }
}

/// A `tsc --watch` status line without its time stamp: `[10:42:01 AM] ` in a terminal,
/// `10:42:01 AM - ` otherwise.
fn watch_message(text: &str) -> &str {
    if let Some((_, message)) = text
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("] "))
    {
        return message;
    }
    match text.split_once(" - ") {
        Some((time, message)) if is_time(time) => message,
        _ => text,
    }
}

/// A time as `tsc --watch` prints it in the current locale, such as `10:42:01 AM` or `22:42:01`.
fn is_time(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_digit())
        && text.contains(':')
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ':' | '.' | ' ' | 'A' | 'P' | 'M'))
}

/// `N` of `Found N error(s)…`.
fn found_errors(message: &str) -> Option<usize> {
    let rest = message.strip_prefix("Found ")?;
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let count = rest[..digits].parse().ok()?;
    rest[digits..].starts_with(" error").then_some(count)
}

/// Whether `text` has an `error TS<code>:`.
fn is_compiler_error(text: &str) -> bool {
    text.match_indices("error TS").any(|(at, found)| {
        let code = &text[at + found.len()..];
        let digits = code.len() - code.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        digits > 0 && code[digits..].starts_with(':')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(output: &mut CompilerOutput, lines: &[&str]) -> Vec<Milestone> {
        lines.iter().filter_map(|line| output.line(line)).collect()
    }

    #[test]
    fn compiler_messages_are_collected_with_their_continuation_lines() {
        let mut output = CompilerOutput::default();
        let milestones = feed(
            &mut output,
            &[
                "> rayfin-functions@1.0.0 build",
                "> tsc --build",
                "",
                "src/function_app.ts(3,7): error TS2322: Type 'number' is not assignable to type \
                 'string'.",
                "src/function_app.ts(9,1): error TS2741: Property 'a' is missing in type '{}'.",
                "  'a' is declared here.",
                "\x1b[96msrc/types.ts\x1b[0m:\x1b[93m1\x1b[0m:\x1b[93m8\x1b[0m - \x1b[91merror\
                 \x1b[0m\x1b[90m TS2304: \x1b[0mCannot find name 'x'.",
                "",
                "1 export x;",
                "         ~",
            ],
        );
        assert!(milestones.is_empty());
        assert_eq!(
            output.take(),
            [
                "src/function_app.ts(3,7): error TS2322: Type 'number' is not assignable to type \
                 'string'.",
                "src/function_app.ts(9,1): error TS2741: Property 'a' is missing in type '{}'.\n  \
                 'a' is declared here.",
                "src/types.ts:1:8 - error TS2304: Cannot find name 'x'.",
            ]
        );
        assert!(output.take().is_empty());
    }

    #[test]
    fn watch_compilations_start_and_finish() {
        let mut output = CompilerOutput::default();
        let milestones = feed(
            &mut output,
            &[
                "[10:42:01 AM] Starting compilation in watch mode...",
                "[10:42:02 AM] Found 0 errors. Watching for file changes.",
                "9:43:00 PM - File change detected. Starting incremental compilation...",
                "src/a.ts(1,7): error TS2322: Type 'number' is not assignable to type 'string'.",
                "21:43:01 - Found 1 error. Watching for file changes.",
            ],
        );
        assert_eq!(
            milestones,
            [
                Milestone::Started { announce: false },
                Milestone::Finished {
                    count: 0,
                    errors: vec![]
                },
                Milestone::Started { announce: true },
                Milestone::Finished {
                    count: 1,
                    errors: vec![
                        "src/a.ts(1,7): error TS2322: Type 'number' is not assignable to type \
                         'string'."
                            .to_owned()
                    ]
                },
            ]
        );
        assert_eq!(
            found_errors("Found 2 errors in the same file, starting at: src/a.ts:3"),
            Some(2)
        );
        assert_eq!(found_errors("Found the bug"), None);
    }

    #[test]
    fn repeated_states_are_reported_once() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observer: StateObserver = {
            let seen = Arc::clone(&seen);
            Arc::new(move |state: &FunctionsState| seen.lock().unwrap().push(state.clone()))
        };
        let reporter = Reporter::new(Some(observer));
        let failed = FunctionsState::Failed {
            message: "functions build failed".into(),
            errors: vec!["error TS1005: ';' expected.".into()],
            build: true,
            names: vec![],
        };
        let ready = FunctionsState::Ready {
            names: vec!["ping".into()],
        };
        for state in [
            FunctionsState::Building,
            FunctionsState::Building,
            failed.clone(),
            failed.clone(),
            ready.clone(),
            ready.clone(),
        ] {
            reporter.report(state);
        }
        assert_eq!(
            *seen.lock().unwrap(),
            [
                FunctionsState::Building,
                failed,
                ready.clone(),
                ready.clone()
            ]
        );
        assert_eq!(reporter.last(), Some(ready));
    }
}
