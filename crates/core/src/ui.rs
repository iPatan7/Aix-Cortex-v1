//! Terminal output: colour, symbols, and step reporting.
//!
//! No dependency on a TUI crate — the whole point of this binary is a fast
//! static build and instant startup. ANSI escapes and `isatty` are enough.
//!
//! Colour is suppressed when stdout is not a terminal, when `NO_COLOR` is
//! set (the community standard), or when `TERM=dumb`, so piping cortex into
//! a log or a CI job produces clean text.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

static COLOR: AtomicBool = AtomicBool::new(false);
static INIT: std::sync::Once = std::sync::Once::new();

fn color_enabled() -> bool {
    INIT.call_once(|| {
        let enabled = std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);
        COLOR.store(enabled, Ordering::Relaxed);
    });
    COLOR.load(Ordering::Relaxed)
}

/// Force colour off, e.g. for `--no-color` or JSON output.
pub fn disable_color() {
    INIT.call_once(|| {});
    COLOR.store(false, Ordering::Relaxed);
}

macro_rules! paint {
    ($name:ident, $code:expr) => {
        pub fn $name(s: &str) -> String {
            if color_enabled() {
                format!("\x1b[{}m{s}\x1b[0m", $code)
            } else {
                s.to_string()
            }
        }
    };
}

paint!(bold, "1");
paint!(dim, "2");
paint!(red, "31");
paint!(green, "32");
paint!(yellow, "33");
paint!(blue, "34");
paint!(cyan, "36");

/// A step that is running, and will resolve to ok/fail.
///
/// Prints `⠿ label` immediately and rewrites the line in place when it
/// resolves, so a long operation shows progress without scrolling. When
/// stdout is not a terminal the line is simply printed once on resolution,
/// which keeps CI logs readable.
pub struct Step {
    label: String,
    interactive: bool,
    resolved: bool,
}

impl Step {
    pub fn start(label: impl Into<String>) -> Self {
        let label = label.into();
        let interactive = color_enabled();
        if interactive {
            print!("  {} {}", dim("○"), dim(&label));
            let _ = std::io::stdout().flush();
        }
        Self {
            label,
            interactive,
            resolved: false,
        }
    }

    fn finish(&mut self, symbol: String, text: String) {
        self.resolved = true;
        if self.interactive {
            // \r + clear-to-EOL, then the resolved line.
            print!("\r\x1b[2K");
        }
        println!("  {symbol} {text}");
    }

    pub fn ok(mut self) {
        let (s, t) = (green("✔"), self.label.clone());
        self.finish(s, t);
    }

    /// Resolve successfully with an extra note (a version, a duration).
    pub fn ok_with(mut self, note: &str) {
        let (s, t) = (green("✔"), format!("{} {}", self.label, dim(note)));
        self.finish(s, t);
    }

    pub fn fail(mut self, why: &str) {
        let (s, t) = (red("✘"), format!("{} {}", self.label, red(why)));
        self.finish(s, t);
    }

    pub fn skip(mut self, why: &str) {
        let (s, t) = (dim("–"), dim(&format!("{} ({why})", self.label)));
        self.finish(s, t);
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        // A step abandoned by `?` must not leave a dangling spinner line.
        if !self.resolved && self.interactive {
            print!("\r\x1b[2K");
            let _ = std::io::stdout().flush();
        }
    }
}

/// A titled section, e.g. `▸ sandbox`.
pub fn section(title: &str) {
    println!("{} {}", cyan("▸"), bold(title));
}

pub fn info(msg: &str) {
    println!("  {} {msg}", dim("·"));
}

pub fn warn(msg: &str) {
    eprintln!("{} {msg}", yellow("!"));
}

/// A fatal, actionable error. `hint` tells the operator what to do next —
/// an error message without a next step is a dead end.
pub fn error(msg: &str, hint: Option<&str>) {
    eprintln!("{} {}", red("✘"), bold(msg));
    if let Some(h) = hint {
        eprintln!("  {} {}", dim("→"), h);
    }
}

/// The headline result of a command.
pub fn committed(entry_id: &str, changes: usize) {
    println!(
        "\n{} {}  {}",
        green("✔"),
        bold("committed"),
        dim(&format!("{changes} change(s) · entry {entry_id}"))
    );
    println!("  {} {}", dim("undo:"), bold("cortex undo"));
}

pub fn rolled_back(why: &str) {
    println!("\n{} {}  {}", yellow("↩"), bold("rolled back"), dim(why));
    println!("  {}", dim("the system was not changed"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a tty, every painter must be the identity function, so piped
    /// output and CI logs never contain escape sequences.
    #[test]
    fn no_color_when_not_a_terminal() {
        // Tests do not run under a tty, so this exercises the real path.
        assert_eq!(bold("x"), "x");
        assert_eq!(red("x"), "x");
        assert_eq!(green("hello"), "hello");
        assert!(!dim("y").contains('\x1b'));
    }
}
