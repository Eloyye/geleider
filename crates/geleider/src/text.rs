//! Making text geleider did not write safe to show: in a terminal, in a GitHub
//! comment posted as the user, or in another Session's context. Job args, Job
//! output, branch names and directory names are all chosen by a model.

/// The most characters of a branch name shown outside its own Session.
const MAX_BRANCH_CHARS: usize = 100;

/// Characters that are never shown: control characters such as terminal escapes,
/// and the invisible ones that reorder or hide text (bidi overrides, zero widths).
pub fn hidden(c: char) -> bool {
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
