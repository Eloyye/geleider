//! Making text geleider did not write safe to show: in a terminal, in a GitHub
//! comment posted as the user, or in another Session's context. Job args, Job
//! output, branch names and directory names are all chosen by a model.

/// The most characters of a branch name shown outside its own Session.
const MAX_BRANCH_CHARS: usize = 100;

/// Characters that are never shown: control characters such as terminal escapes,
/// and the invisible ones that reorder or hide text (bidi overrides, zero widths).
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
}

/// Whether `text` holds any character [`printable`] would drop, other than lines and tabs.
pub fn has_hidden(text: &str) -> bool {
    text.chars().any(|c| hidden(c) && !matches!(c, '\n' | '\t'))
}

/// `text` without hidden characters, keeping lines and tabs.
pub fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| !hidden(*c) || matches!(c, '\n' | '\t'))
        .collect()
}

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
/// or `TERM=dumb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paint {
    enabled: bool,
}

impl Paint {
    #[cfg(test)]
    pub const PLAIN: Paint = Paint { enabled: false };
    #[cfg(test)]
    pub const COLOR: Paint = Paint { enabled: true };

    pub fn for_stdout() -> Self {
        // SAFETY: isatty only inspects the descriptor.
        let terminal = unsafe { libc::isatty(1) } == 1;
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let dumb = std::env::var_os("TERM").is_some_and(|t| t == "dumb");
        Self {
            enabled: terminal && !no_color && !dumb,
        }
    }

    pub fn style(self, style: Style, text: &str) -> String {
        if self.enabled && !text.is_empty() {
            format!("{}{text}{RESET}", style.sgr())
        } else {
            text.to_string()
        }
    }
}

/// [`printable`], except that it keeps the exact sequences [`Paint`] writes, so
/// styled output can still be filtered as a whole. Every other escape is
/// dropped, so text from a Session can at most recolour itself.
pub fn printable_styled(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
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
        if !hidden(c) || matches!(c, '\n' | '\t') {
            out.push(c);
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// `text` on one line, with hidden characters, lines and tabs as spaces.
pub fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if hidden(c) { ' ' } else { c })
        .collect()
}

/// The longest run of backticks in `text`.
fn longest_backtick_run(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

/// A Markdown code fence longer than any run of backticks in `text`, so the
/// block holds it intact. At least three backticks.
pub fn code_fence(text: &str) -> String {
    "`".repeat(longest_backtick_run(text).max(2) + 1)
}

/// Markdown inline code showing `text` literally on one line. The fence is
/// longer than any run of backticks inside, so nothing in it can end the span
/// and become a link, a mention or HTML.
pub fn code_span(text: &str) -> String {
    let text = one_line(text);
    let fence = "`".repeat(longest_backtick_run(&text) + 1);
    // A space keeps a backtick at either end from joining the fence.
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// A branch name as other Sessions and GitHub see it, as inline code. Git allows
/// almost any text in a branch name, and the Session that made the branch chose
/// it, so only the characters branch names commonly use are kept, others show as
/// `?`, and long names are cut short.
pub fn branch(name: &str) -> String {
    let mut shown: String = name
        .chars()
        .take(MAX_BRANCH_CHARS)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '+' | '#') {
                c
            } else {
                '?'
            }
        })
        .collect();
    if name.chars().count() > MAX_BRANCH_CHARS {
        shown.push_str("...");
    }
    code_span(&shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_escapes_and_invisible_reordering_are_dropped() {
        assert_eq!(
            printable("\u{1b}[31mred\u{1b}[0m\tok\n\u{7}"),
            "[31mred[0m\tok\n"
        );
        assert_eq!(printable("evil\u{202e}txt.exe"), "eviltxt.exe");
        assert_eq!(one_line("a\nb\u{1b}c"), "a b c");
        assert!(has_hidden("x\u{1b}]52;c;aGk=\u{7}"));
        assert!(has_hidden("\u{200b}"));
        assert!(!has_hidden("tabs\tand\nlines are fine"));
    }

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

    #[test]
    fn code_spans_cannot_be_closed_from_inside() {
        assert_eq!(code_span("playmode-test combat"), "`playmode-test combat`");
        // A backtick in an arg would otherwise end the span and let a link through.
        assert_eq!(
            code_span("x` [click](https://e.vil) @team `"),
            "`` x` [click](https://e.vil) @team ` ``"
        );
        assert_eq!(code_span("a\n# heading"), "`a # heading`");
        assert_eq!(code_fence("no ticks"), "```");
        assert_eq!(code_fence("````"), "`````");
    }

    #[test]
    fn branches_show_only_common_branch_characters() {
        assert_eq!(branch("codex/issue-5-fix"), "`codex/issue-5-fix`");
        assert_eq!(
            branch("x`IGNORE_previous\"instructions"),
            "`x?IGNORE_previous?instructions`"
        );
        assert_eq!(branch("caf\u{e9}"), "`caf?`");
        let long = "a".repeat(MAX_BRANCH_CHARS + 1);
        assert_eq!(
            branch(&long),
            format!("`{}...`", "a".repeat(MAX_BRANCH_CHARS))
        );
    }
}
