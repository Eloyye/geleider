//! Boxed panels of styled text for the terminal, as `geleider status` draws them.
//! Widths are counted in characters, which is what a terminal shows for the
//! text geleider prints; text from Sessions is put on one line by [`Span::shown`].

use crate::text::{self, Paint, Style};

/// Columns assumed when stdout is not a terminal and `COLUMNS` is not set.
const DEFAULT_WIDTH: usize = 80;

#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    text: String,
    style: Option<Style>,
}

impl Span {
    /// geleider's own words.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: None,
        }
    }

    pub fn styled(style: Style, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Some(style),
        }
    }

    /// Text a Session chose, such as a branch, on one line.
    pub fn shown(text: &str) -> Self {
        Self::plain(text::one_line(text))
    }

    pub fn with(mut self, style: Style) -> Self {
        self.style = Some(style);
        self
    }

    /// Padded with spaces to `to` characters, if shorter.
    pub fn padded(mut self, to: usize) -> Self {
        let pad = to.saturating_sub(width(&self.text));
        self.text.push_str(&" ".repeat(pad));
        self
    }

    pub fn width(&self) -> usize {
        width(&self.text)
    }
}

pub type Line = Vec<Span>;

fn width(text: &str) -> usize {
    text.chars().count()
}

pub fn line_width(line: &[Span]) -> usize {
    line.iter().map(Span::width).sum()
}

/// `line` cut to `max` characters, ending in `…` where it was cut.
pub fn truncate(line: &[Span], max: usize) -> Line {
    if line_width(line) <= max {
        return line.to_vec();
    }
    let mut out = Vec::new();
    let mut left = max.saturating_sub(1);
    for span in line {
        if left == 0 {
            break;
        }
        let text: String = span.text.chars().take(left).collect();
        left -= width(&text);
        out.push(Span {
            text,
            ..span.clone()
        });
    }
    if max > 0 {
        out.push(Span::styled(Style::Dim, "…"));
    }
    out
}

/// `line` cut or padded to exactly `to` characters.
fn fit(line: &[Span], to: usize) -> Line {
    let mut out = truncate(line, to);
    let pad = to - line_width(&out);
    out.push(Span::plain(" ".repeat(pad)));
    out
}

/// `lines` in a rounded box `width` characters wide, titled `title`.
pub fn boxed(title: &str, lines: &[Line], width: usize) -> Vec<Line> {
    let inner = width.saturating_sub(4);
    let title = truncate(&[Span::styled(Style::Bold, title)], inner.saturating_sub(1));
    let rule = width.saturating_sub(line_width(&title) + 5);
    let mut top = vec![Span::styled(Style::Dim, "╭─ ")];
    top.extend(title);
    top.push(Span::styled(Style::Dim, format!(" {}╮", "─".repeat(rule))));
    let mut out = vec![top];
    for line in lines {
        let mut row = vec![Span::styled(Style::Dim, "│ ")];
        row.extend(fit(line, inner));
        row.push(Span::styled(Style::Dim, " │"));
        out.push(row);
    }
    out.push(vec![Span::styled(
        Style::Dim,
        format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
    )]);
    out
}

/// `left` and `right` side by side, `left` padded to `left_width` and a space between.
pub fn beside(left: &[Line], right: &[Line], left_width: usize) -> Vec<Line> {
    (0..left.len().max(right.len()))
        .map(|i| {
            let mut line = fit(left.get(i).map_or(&[][..], |l| l), left_width);
            line.push(Span::plain(" "));
            line.extend(right.get(i).cloned().unwrap_or_default());
            line
        })
        .collect()
}

/// A usage gauge `cells` wide: green, yellow from `busy`, red from `near`.
pub fn gauge(used: Option<f64>, cells: usize, busy: f64, near: f64) -> Line {
    let Some(used) = used else {
        return vec![Span::styled(Style::Dim, "·".repeat(cells))];
    };
    let filled = ((used / 100.0) * cells as f64)
        .round()
        .clamp(0.0, cells as f64) as usize;
    let style = if used >= near {
        Style::Red
    } else if used >= busy {
        Style::Yellow
    } else {
        Style::Green
    };
    vec![
        Span::styled(style, "█".repeat(filled)),
        Span::styled(Style::Dim, "░".repeat(cells - filled)),
    ]
}

/// The lines as text, styled by `paint`, without trailing spaces.
pub fn render(lines: &[Line], paint: Paint) -> String {
    let mut out = String::new();
    for line in lines {
        let mut text = String::new();
        let mut spaces = String::new();
        for span in line {
            // Trailing padding is held back until something follows it.
            let body = span.text.trim_end_matches(' ');
            let trailing = &span.text[body.len()..];
            if !body.is_empty() {
                text.push_str(&spaces);
                spaces.clear();
                match span.style {
                    Some(style) => text.push_str(&paint.style(style, body)),
                    None => text.push_str(body),
                }
            }
            spaces.push_str(trailing);
        }
        out.push_str(&text);
        out.push('\n');
    }
    out
}

/// The terminal's width, else `COLUMNS`, else 80.
pub fn terminal_width() -> usize {
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

    fn plain(lines: &[Line]) -> String {
        render(lines, Paint::PLAIN)
    }

    #[test]
    fn truncation_marks_the_cut_and_keeps_styles() {
        let line = vec![
            Span::styled(Style::Bold, "lanternfall"),
            Span::plain("  claude/issue-42"),
        ];
        assert_eq!(truncate(&line, 100), line);
        let cut = truncate(&line, 15);
        assert_eq!(line_width(&cut), 15);
        assert_eq!(plain(std::slice::from_ref(&cut)), "lanternfall  c…\n");
        assert_eq!(cut[0].style, Some(Style::Bold));
    }

    #[test]
    fn boxes_are_as_wide_as_asked_whatever_their_contents() {
        let lines = vec![
            vec![Span::plain("short")],
            vec![Span::plain("x".repeat(50))],
            vec![],
        ];
        let shown = plain(&boxed("Sessions", &lines, 20));
        assert_eq!(
            shown,
            "╭─ Sessions ───────╮\n\
             │ short            │\n\
             │ xxxxxxxxxxxxxxx… │\n\
             │                  │\n\
             ╰──────────────────╯\n"
        );
    }

    #[test]
    fn panels_sit_side_by_side() {
        let left = boxed("A", &[vec![Span::plain("1")]], 7);
        let right = boxed("B", &[vec![Span::plain("2")], vec![Span::plain("3")]], 7);
        assert_eq!(
            plain(&beside(&left, &right, 7)),
            "╭─ A ─╮ ╭─ B ─╮\n\
             │ 1   │ │ 2   │\n\
             ╰─────╯ │ 3   │\n\
             \x20       ╰─────╯\n"
        );
    }

    #[test]
    fn gauges_fill_and_colour_by_use() {
        let gauge = |used| gauge(used, 10, 60.0, 85.0);
        assert_eq!(plain(&[gauge(Some(23.0))]), "██░░░░░░░░\n");
        assert_eq!(gauge(Some(23.0))[0].style, Some(Style::Green));
        assert_eq!(gauge(Some(62.0))[0].style, Some(Style::Yellow));
        assert_eq!(gauge(Some(87.0))[0].style, Some(Style::Red));
        assert_eq!(plain(&[gauge(Some(150.0))]), "██████████\n");
        assert_eq!(plain(&[gauge(None)]), "··········\n");
    }

    #[test]
    fn rendering_styles_words_but_not_padding() {
        let line = vec![
            Span::styled(Style::Red, "failed").padded(9),
            Span::plain("x"),
            Span::styled(Style::Dim, "  ").padded(5),
        ];
        assert_eq!(
            render(&[line], Paint::COLOR),
            "\u{1b}[31mfailed\u{1b}[0m   x\n"
        );
    }
}
