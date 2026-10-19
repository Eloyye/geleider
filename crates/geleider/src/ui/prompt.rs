//! The three questions geleider asks. Each writes to stderr and decides for
//! itself what to do without a terminal, so commands never branch on it. A
//! cancelled prompt returns no answer; the command says what did not happen.

use anyhow::{Context, Result, bail};

use super::style::Paint;
use super::{Line, Term, panel};

impl Term {
    /// `question [y/N]`. Only `y` or `yes`, in any case, is yes; anything else,
    /// including the end of input, is no.
    pub fn confirm(&mut self, question: &str) -> Result<bool> {
        self.ask(&format!("{question} [y/N] "));
        let answer = self.read_line()?.unwrap_or_default();
        Ok(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }

    /// Checkboxes over `items`, `checked` ticked to start with; the indices
    /// ticked when accepted, in order. Without a terminal it lists numbered
    /// rows and reads numbers, where Enter alone keeps `checked`; when every
    /// row starts ticked it only lists them and asks nothing.
    pub fn pick_many(
        &mut self,
        prompt: &str,
        items: &[Line],
        checked: &[bool],
    ) -> Result<Option<Vec<usize>>> {
        if items.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let checked: Vec<bool> = (0..items.len())
            .map(|i| checked.get(i) == Some(&true))
            .collect();
        if self.interactive() {
            return dialoguer::MultiSelect::new()
                .with_prompt(format!(
                    "{} (space toggles, enter accepts, esc cancels)",
                    plain_line(prompt)
                ))
                .items(rows(items))
                .defaults(&checked)
                .interact_opt()
                .context("showing the picker");
        }
        let defaults: Vec<usize> = (0..items.len()).filter(|&i| checked[i]).collect();
        if defaults.len() == items.len() {
            for row in rows(items) {
                self.say_err(&format!("  {row}"));
            }
            return Ok(Some(defaults));
        }
        self.say_err(prompt);
        let marks = !defaults.is_empty();
        for (i, row) in rows(items).iter().enumerate() {
            let mark = match (marks, defaults.contains(&i)) {
                (false, _) => "",
                (true, true) => "[x] ",
                (true, false) => "[ ] ",
            };
            self.say_err(&format!("  {}. {mark}{row}", i + 1));
        }
        self.ask(if marks {
            "Numbers (e.g. 1 3), or Enter for the ticked ones: "
        } else {
            "Numbers (e.g. 1 3), or Enter for none: "
        });
        let Some(answer) = self.read_line()? else {
            return Ok(None);
        };
        if answer.trim().is_empty() {
            return Ok(Some(defaults));
        }
        let mut chosen = Vec::new();
        for word in answer.split([' ', ',', '\t']).filter(|w| !w.is_empty()) {
            chosen.push(row_number(word, items.len())?);
        }
        chosen.sort_unstable();
        chosen.dedup();
        Ok(Some(chosen))
    }

    /// A list of `items` to choose one from. Without a terminal it lists
    /// numbered rows and reads a number or a row's name, its first span, where
    /// Enter alone cancels.
    pub fn pick_one(&mut self, prompt: &str, items: &[Line]) -> Result<Option<usize>> {
        if items.is_empty() {
            return Ok(None);
        }
        if self.interactive() {
            return dialoguer::Select::new()
                .with_prompt(format!(
                    "{} (enter accepts, esc cancels)",
                    plain_line(prompt)
                ))
                .items(rows(items))
                .default(0)
                .interact_opt()
                .context("showing the picker");
        }
        self.say_err(prompt);
        for (i, row) in rows(items).iter().enumerate() {
            self.say_err(&format!("  {}. {row}", i + 1));
        }
        self.ask("Number or name, or Enter to cancel: ");
        let answer = self.read_line()?.unwrap_or_default();
        let answer = answer.trim();
        if answer.is_empty() {
            return Ok(None);
        }
        if answer.bytes().all(|b| b.is_ascii_digit()) {
            return row_number(answer, items.len()).map(Some);
        }
        match items
            .iter()
            .position(|line| line.first().is_some_and(|name| name.text.trim() == answer))
        {
            Some(i) => Ok(Some(i)),
            None => bail!("`{}` is not one of the choices listed", plain_line(answer)),
        }
    }
}

/// The 0-based index of row `word` of `count`, numbered from 1.
fn row_number(word: &str, count: usize) -> Result<usize> {
    match word.parse::<usize>() {
        Ok(n) if (1..=count).contains(&n) => Ok(n - 1),
        _ => bail!("`{}` is not a number from 1 to {count}", plain_line(word)),
    }
}

/// Each item on one line, unpainted and with no escapes, for a picker.
fn rows(items: &[Line]) -> Vec<String> {
    items
        .iter()
        .map(|line| {
            let shown = panel::render(std::slice::from_ref(line), Paint::PLAIN);
            super::style::printable_styled(shown.trim_end_matches('\n'))
        })
        .collect()
}

fn plain_line(text: &str) -> String {
    crate::text::one_line(text)
}

#[cfg(test)]
mod tests {
    use super::super::Span;
    use super::*;

    fn items(names: &[&str]) -> Vec<Line> {
        names
            .iter()
            .map(|n| vec![Span::plain(*n), Span::plain("  (row)")])
            .collect()
    }

    #[test]
    fn only_an_explicit_yes_confirms() {
        for answer in [None, Some(""), Some("no"), Some("maybe"), Some("true")] {
            let mut term = Term::scripted(answer, 80);
            assert!(
                !term.confirm("Remove this worktree?").unwrap(),
                "{answer:?}"
            );
            assert!(
                term.stderr().starts_with("Remove this worktree? [y/N] "),
                "{}",
                term.stderr()
            );
            assert_eq!(term.stdout(), "");
        }
        for answer in ["y", "yes", "YES", " Yes "] {
            let mut term = Term::scripted([answer], 80);
            assert!(term.confirm("Start?").unwrap(), "{answer}");
        }
    }

    #[test]
    fn picking_many_without_a_terminal_reads_row_numbers() {
        let rows = items(&["a", "b", "c"]);
        let none = [false; 3];
        let mut term = Term::scripted(["3, 1 3"], 80);
        assert_eq!(
            term.pick_many("Choose", &rows, &none).unwrap(),
            Some(vec![0, 2])
        );
        let asked = term.stderr();
        assert!(
            asked.starts_with("Choose\n  1. a  (row)\n  2. b"),
            "{asked}"
        );
        assert!(asked.contains("or Enter for none: 3, 1 3\n"), "{asked}");
        assert_eq!(term.stdout(), "");

        let mut term = Term::scripted([""], 80);
        assert_eq!(
            term.pick_many("Choose", &rows, &none).unwrap(),
            Some(vec![])
        );
        let mut term = Term::scripted([""], 80);
        let some = [false, true, false];
        assert_eq!(
            term.pick_many("Choose", &rows, &some).unwrap(),
            Some(vec![1])
        );
        assert!(
            term.stderr().contains("  2. [x] b  (row)"),
            "{}",
            term.stderr()
        );
        let mut term = Term::scripted([], 80);
        assert_eq!(term.pick_many("Choose", &rows, &none).unwrap(), None);
        for bad in ["0", "4", "b"] {
            let mut term = Term::scripted([bad], 80);
            assert!(term.pick_many("Choose", &rows, &none).is_err(), "{bad}");
        }
    }

    #[test]
    fn picking_many_with_every_row_ticked_asks_nothing_without_a_terminal() {
        let rows = items(&["a", "b"]);
        // The answer is left for the confirmation that follows.
        let mut term = Term::scripted(["y"], 80);
        assert_eq!(
            term.pick_many("Uncheck to keep", &rows, &[true, true])
                .unwrap(),
            Some(vec![0, 1])
        );
        assert_eq!(term.stderr(), "  a  (row)\n  b  (row)\n");
        assert!(term.confirm("Remove these 2 worktrees?").unwrap());
    }

    #[test]
    fn picking_one_without_a_terminal_reads_a_number_or_a_name() {
        let rows = items(&["claude-p", "codex-p"]);
        let mut term = Term::scripted(["2"], 80);
        assert_eq!(term.pick_one("Profile to use", &rows).unwrap(), Some(1));
        assert!(
            term.stderr()
                .starts_with("Profile to use\n  1. claude-p  (row)\n  2. codex-p  (row)\n"),
            "{}",
            term.stderr()
        );
        let mut term = Term::scripted([" claude-p "], 80);
        assert_eq!(term.pick_one("Profile to use", &rows).unwrap(), Some(0));
        for cancel in [None, Some("")] {
            let mut term = Term::scripted(cancel, 80);
            assert_eq!(term.pick_one("Profile to use", &rows).unwrap(), None);
        }
        for bad in ["3", "opus"] {
            let mut term = Term::scripted([bad], 80);
            assert!(term.pick_one("Profile to use", &rows).is_err(), "{bad}");
        }
    }
}
