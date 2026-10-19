# All terminal output and prompts go through one `ui` module

geleider shows text from Sessions, such as branches, paths and logs, so everything it draws for you must drop terminal escapes except the colours geleider writes itself. That rule, along with colour, width and "is this a terminal?", used to be decided separately in each command: `status` sanitized its panels by hand, `clean` checked `stdin` and `stderr` itself before showing a `dialoguer` picker, and eight prompts each read their own line from stdin. The next command could easily forget one of them.

So every human-facing line of CLI output and every prompt goes through a `ui` module. `main` builds one `Term`, which knows whether colour is allowed, how wide the terminal is and whether prompts can be shown. Each command receives it. `Term` is the only thing that paints, sanitizes and writes. Panels are built as plain `Line`s, and text from Sessions only enters a `Line` through `Span::shown`.

`ui` offers three prompts, and each one decides for itself what happens without a terminal, so commands do not branch on it:

- `confirm`: a `[y/N]` question. Without a terminal it reads one line, and only `y` or `yes` means yes.
- `pick_many`: checkboxes. Without a terminal it lists numbered rows and reads numbers, where Enter alone keeps the defaults. When every row starts checked it asks nothing, so a script that pipes `y` to the confirmation that follows still works.
- `pick_one`: a select list. Without a terminal it reads a number or a name, and Enter alone cancels.

Prompts write to stderr, so stdout carries only results. A cancelled prompt returns no answer, and the command says what did not happen. `dialoguer` draws the pickers on a terminal, and only `ui` imports it.

Some output is read by programs, not people, and does not go through `ui`: the status line for Claude Code, hook, MCP and background-process stdio, `--json` output (which serde already escapes) and the final error message in `main`. Clippy's `disallowed-macros` and `disallowed-methods` ban `print!`-style macros, `dialoguer` and `is_terminal` everywhere else, and each exempt module allows them with a one-line reason.

## Considered Options

- **A separate crate for the terminal UI**: rejected for now. geleider is its only user, and a crate boundary would add public API without a second consumer.
- **A full-screen framework such as ratatui**: rejected for now. Nothing redraws live yet, and the `dialoguer` pickers cover every prompt. It can sit behind `ui` later.
- **Hand-written raw-mode key handling for the pickers**: rejected. `dialoguer` already handles it and is hidden behind `ui`.
- **Keeping prompts on stdout**: rejected. It mixed questions into output that scripts read.

## Consequences

A new command gets colour, width, sanitizing and terminal-less prompting by taking `&mut Term`, and cannot print around it without an explicit clippy allow. Tests drive prompts with a scripted `Term` instead of passing a reader through each command. Implementation is tracked in issue #28.
