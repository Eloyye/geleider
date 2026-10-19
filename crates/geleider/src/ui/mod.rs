//! Every line geleider writes for people and every question it asks them (ADR
//! 0008). `main` builds one [`Term`] and passes it to each command. It knows
//! whether colour is allowed, how wide the terminal is and whether prompts can
//! be drawn, and it is the only thing that paints, sanitizes and writes. Output
//! read by programs (the status line, hook, MCP and background-process stdio,
//! `--json`) does not come here.
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "ui is where terminals are detected and pickers drawn"
)]

mod panel;
mod prompt;
mod style;

#[cfg(test)]
use std::collections::VecDeque;
use std::io::{BufRead, IsTerminal, Write};

use anyhow::Result;

pub use panel::{Line, Span, beside, boxed, gauge};
use style::Paint;
pub use style::Style;

use crate::text;

/// Columns assumed when stdout is not a terminal and `COLUMNS` is not set.
const DEFAULT_WIDTH: usize = 80;

enum Mode {
    /// stdin and stderr are a terminal: pickers are drawn by `dialoguer`.
    Terminal,
    /// No terminal to draw on, such as piped input: prompts read lines.
    Lines,
    /// A test's answers, read as lines, with all output kept.
    #[cfg(test)]
    Scripted {
        answers: VecDeque<String>,
        out: String,
        err: String,
    },
}

#[derive(Clone, Copy)]
enum Stream {
    Out,
    Err,
}

pub struct Term {
    mode: Mode,
    paint: Paint,
    width: usize,
}

impl Term {
    /// The process's own terminal: colour only when stdout is one and neither
    /// `NO_COLOR` nor `TERM=dumb` is set.
    pub fn detect() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let dumb = std::env::var_os("TERM").is_some_and(|t| t == "dumb");
        let color = std::io::stdout().is_terminal() && !no_color && !dumb;
        let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        Self {
            mode: if interactive {
                Mode::Terminal
            } else {
                Mode::Lines
            },
            paint: if color { Paint::COLOR } else { Paint::PLAIN },
            width: terminal_width(),
        }
    }

    /// A `width`-column Term without colour that answers prompts with
    /// `answers`, one line each, and keeps what is written.
    #[cfg(test)]
    pub fn scripted<'a>(answers: impl IntoIterator<Item = &'a str>, width: usize) -> Self {
        Self {
            mode: Mode::Scripted {
                answers: answers.into_iter().map(String::from).collect(),
                out: String::new(),
                err: String::new(),
            },
            paint: Paint::PLAIN,
            width,
        }
    }

    #[cfg(test)]
    pub fn colored(mut self) -> Self {
        self.paint = Paint::COLOR;
        self
    }

    /// What a scripted Term wrote to stdout.
    #[cfg(test)]
    pub fn stdout(&self) -> &str {
        match &self.mode {
            Mode::Scripted { out, .. } => out,
            _ => "",
        }
    }

    /// What a scripted Term wrote to stderr, where prompts go.
    #[cfg(test)]
    pub fn stderr(&self) -> &str {
        match &self.mode {
            Mode::Scripted { err, .. } => err,
            _ => "",
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    /// Writes `lines` to stdout, painted if colour is allowed, without
    /// trailing spaces, and with every escape dropped but geleider's own colours.
    pub fn show(&mut self, lines: &[Line]) {
        let shown = style::printable_styled(&panel::render(lines, self.paint));
        self.write(Stream::Out, &shown);
    }

    /// Writes plain `text` to stdout, line by line, as [`text::printable`]
    /// leaves it.
    pub fn say(&mut self, text: &str) {
        self.show(&text_lines(text));
    }

    /// [`Term::say`] for stderr: warnings, refusals and progress.
    pub fn say_err(&mut self, text: &str) {
        self.show_err(&text_lines(text));
    }

    /// Writes `lines` to stderr, unpainted, as [`Term::show`] filters them.
    fn show_err(&mut self, lines: &[Line]) {
        let shown = style::printable_styled(&panel::render(lines, Paint::PLAIN));
        self.write(Stream::Err, &shown);
    }

    /// Writes a question to stderr on one line, leaving the cursor after it.
    fn ask(&mut self, question: &str) {
        self.write(Stream::Err, &text::one_line(question));
    }

    /// Whether prompts can be drawn on a terminal rather than read as lines.
    fn interactive(&self) -> bool {
        matches!(self.mode, Mode::Terminal)
    }

    fn write(&mut self, stream: Stream, shown: &str) {
        // A closed pipe, as with `| head`, is not worth failing a command for.
        let _ = match (&mut self.mode, stream) {
            (Mode::Terminal | Mode::Lines, Stream::Out) => {
                let mut out = std::io::stdout().lock();
                out.write_all(shown.as_bytes()).and_then(|()| out.flush())
            }
            (Mode::Terminal | Mode::Lines, Stream::Err) => {
                std::io::stderr().lock().write_all(shown.as_bytes())
            }
            #[cfg(test)]
            (Mode::Scripted { out, .. }, Stream::Out) => {
                out.push_str(shown);
                Ok(())
            }
            #[cfg(test)]
            (Mode::Scripted { err, .. }, Stream::Err) => {
                err.push_str(shown);
                Ok(())
            }
        };
    }

    /// One line of input without its line ending, or `None` at the end of input.
    fn read_line(&mut self) -> Result<Option<String>> {
        match &mut self.mode {
            Mode::Terminal | Mode::Lines => {
                let mut line = String::new();
                if std::io::stdin().lock().read_line(&mut line)? == 0 {
                    return Ok(None);
                }
                Ok(Some(line.trim_end_matches(['\n', '\r']).to_string()))
            }
            #[cfg(test)]
            Mode::Scripted { answers, err, .. } => {
                let answer = answers.pop_front();
                // As a terminal would echo it.
                err.push_str(answer.as_deref().unwrap_or_default());
                err.push('\n');
                Ok(answer)
            }
        }
    }
}

/// Each line of `text`, without hidden characters. Tabs are kept, unlike in
/// [`Span::shown`], so log tails stay aligned; nothing measures these lines.
fn text_lines(text: &str) -> Vec<Line> {
    text.lines()
        .map(|line| vec![Span::plain(text::printable(line))])
        .collect()
}

/// The terminal's width, else `COLUMNS`, else 80.
fn terminal_width() -> usize {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes a winsize through the valid pointer.
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) } == 0;
    if ok && size.ws_col > 0 {
        return usize::from(size.ws_col);
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .filter(|c| *c > 0)
        .unwrap_or(DEFAULT_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shown_text_keeps_its_own_colours_and_drops_every_other_escape() {
        let mut term = Term::scripted([], 80).colored();
        term.show(&[vec![
            Span::styled(Style::Red, "failed"),
            Span::plain(" "),
            Span::shown("x\u{1b}]52;c;aGk=\u{7}\nfake"),
        ]]);
        assert_eq!(
            term.stdout(),
            "\u{1b}[31mfailed\u{1b}[0m x ]52;c;aGk=  fake\n"
        );
        term.say("one\n\ntwo\t\u{1b}[2J");
        assert!(term.stdout().ends_with("one\n\ntwo\t[2J\n"));
        term.say_err("Skipped \u{1b}[8mhidden");
        assert_eq!(term.stderr(), "Skipped [8mhidden\n");
    }

    #[test]
    fn a_plain_term_writes_no_escapes() {
        let mut term = Term::scripted([], 80);
        term.show(&[vec![Span::styled(Style::Bold, "Sessions")]]);
        assert_eq!(term.stdout(), "Sessions\n");
    }
}
