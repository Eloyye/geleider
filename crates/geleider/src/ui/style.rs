//! The colours geleider writes itself, and the filter that keeps only those.

use crate::text;

/// Colour and weight for geleider's own words in a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Bold,
    Dim,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
}

impl Style {
    const ALL: [Style; 8] = [
        Style::Bold,
        Style::Dim,
        Style::Red,
        Style::Green,
        Style::Yellow,
        Style::Blue,
        Style::Magenta,
        Style::Cyan,
    ];

    fn sgr(self) -> &'static str {
        match self {
            Style::Bold => "\x1b[1m",
            Style::Dim => "\x1b[2m",
            Style::Red => "\x1b[31m",
            Style::Green => "\x1b[32m",
            Style::Yellow => "\x1b[33m",
            Style::Blue => "\x1b[34m",
            Style::Magenta => "\x1b[35m",
            Style::Cyan => "\x1b[36m",
        }
    }
}

const RESET: &str = "\x1b[0m";

/// Whether to style output: only on a terminal, and never with `NO_COLOR` set
/// or `TERM=dumb`. [`super::Term`] decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paint {
    enabled: bool,
}

impl Paint {
    pub const PLAIN: Paint = Paint { enabled: false };
    pub const COLOR: Paint = Paint { enabled: true };

    pub fn style(self, style: Style, text: &str) -> String {
        if self.enabled && !text.is_empty() {
            format!("{}{text}{RESET}", style.sgr())
        } else {
            text.to_string()
        }
    }
}

/// [`text::printable`], except that it keeps the exact sequences [`Paint`]
/// writes, so styled output can still be filtered as a whole. Every other
/// escape is dropped, so text from a Session can at most recolour itself.
pub fn printable_styled(shown: &str) -> String {
    let mut out = String::with_capacity(shown.len());
    let mut rest = shown;
    while let Some(c) = rest.chars().next() {
        let kept = Style::ALL
            .iter()
            .map(|s| s.sgr())
            .chain([RESET])
            .find(|sgr| rest.starts_with(sgr));
        if let Some(sgr) = kept {
            out.push_str(sgr);
            rest = &rest[sgr.len()..];
            continue;
        }
        if !text::hidden(c) || matches!(c, '\n' | '\t') {
            out.push(c);
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styled_output_keeps_only_its_own_styles() {
        let red = Paint::COLOR.style(Style::Red, "failed");
        assert_eq!(red, "\u{1b}[31mfailed\u{1b}[0m");
        assert_eq!(printable_styled(&red), red);
        assert_eq!(Paint::PLAIN.style(Style::Red, "failed"), "failed");
        // Cursor moves, clipboard writes and hidden text are not Paint's.
        assert_eq!(
            printable_styled("a\u{1b}[2Jb\u{1b}]52;c;aGk=\u{7}c\u{1b}[8md\u{202e}e"),
            "a[2Jb]52;c;aGk=c[8mde"
        );
        assert_eq!(printable_styled("tabs\tand\nlines"), "tabs\tand\nlines");
    }
}
