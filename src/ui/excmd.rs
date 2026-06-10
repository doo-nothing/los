//! Shared vi ex-style command line (`:` prompt) used by every module.
//!
//! The component owns the input buffer and key handling; executing a parsed
//! [`ExCommand`] is module-specific. See docs/keybindings.md for the command
//! set: `:w [name]`, `:e <name>`, `:q`, `:q!`, `:x`/`:wq [name]`,
//! `:set <key> <value>`.

use std::path::Path;

use crossterm::event::KeyCode;

/// A parsed ex command.
#[derive(Debug, Clone, PartialEq)]
pub enum ExCommand {
    /// `:w [name]` — save patch (current name when omitted)
    Write(Option<String>),
    /// `:e <name>` — load patch
    Edit(String),
    /// `:q` / `:q!`
    Quit { force: bool },
    /// `:x` / `:wq [name]` — save patch and quit
    WriteQuit(Option<String>),
    /// `:set <key> <value>`
    Set(String, String),
    /// Anything unrecognized (reported in the status line)
    Unknown(String),
}

/// Parse one ex command line (without the leading `:`).
pub fn parse(input: &str) -> ExCommand {
    let input = input.trim();
    let (cmd, rest) = match input.split_once(char::is_whitespace) {
        Some((c, r)) => (c, r.trim()),
        None => (input, ""),
    };
    let arg = || {
        if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }
    };
    match cmd {
        "w" | "write" => ExCommand::Write(arg()),
        "e" | "edit" => match arg() {
            Some(name) => ExCommand::Edit(name),
            None => ExCommand::Unknown(input.to_string()),
        },
        "q" | "quit" => ExCommand::Quit { force: false },
        "q!" | "quit!" => ExCommand::Quit { force: true },
        "x" | "wq" => ExCommand::WriteQuit(arg()),
        "set" => match rest.split_once(char::is_whitespace) {
            Some((k, v)) if !v.trim().is_empty() => {
                ExCommand::Set(k.to_string(), v.trim().to_string())
            }
            _ => ExCommand::Unknown(input.to_string()),
        },
        _ => ExCommand::Unknown(input.to_string()),
    }
}

/// What the module loop should do with a key event routed to the ex line.
#[derive(Debug, Clone, PartialEq)]
pub enum ExEvent {
    /// Still editing; redraw the prompt.
    Pending,
    /// Prompt dismissed without a command.
    Cancelled,
    /// Enter pressed: execute this command.
    Submit(ExCommand),
}

/// The interactive `:` prompt state. Inactive until [`ExLine::open`].
#[derive(Debug, Default)]
pub struct ExLine {
    buffer: String,
    active: bool,
}

impl ExLine {
    pub fn open(&mut self) {
        self.active = true;
        self.buffer.clear();
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The prompt text to render (including the leading `:`).
    pub fn display(&self) -> String {
        format!(":{}", self.buffer)
    }

    /// Feed a key event into the prompt. `complete` supplies candidates for
    /// Tab completion of the current last word (e.g. patch names).
    pub fn handle_key(&mut self, code: KeyCode, complete: &[String]) -> ExEvent {
        match code {
            KeyCode::Enter => {
                self.active = false;
                let line = std::mem::take(&mut self.buffer);
                if line.trim().is_empty() {
                    ExEvent::Cancelled
                } else {
                    ExEvent::Submit(parse(&line))
                }
            }
            KeyCode::Esc => {
                self.active = false;
                self.buffer.clear();
                ExEvent::Cancelled
            }
            KeyCode::Backspace => {
                if self.buffer.pop().is_none() {
                    // backspace on an empty prompt dismisses it, like vi
                    self.active = false;
                    return ExEvent::Cancelled;
                }
                ExEvent::Pending
            }
            KeyCode::Tab => {
                self.complete_last_word(complete);
                ExEvent::Pending
            }
            KeyCode::Char(c) => {
                self.buffer.push(c);
                ExEvent::Pending
            }
            _ => ExEvent::Pending,
        }
    }

    fn complete_last_word(&mut self, candidates: &[String]) {
        let (head, last) = match self.buffer.rsplit_once(char::is_whitespace) {
            Some((h, l)) => (format!("{} ", h.trim_end()), l.to_string()),
            None => (String::new(), self.buffer.clone()),
        };
        if last.is_empty() {
            return;
        }
        let matches: Vec<&String> = candidates
            .iter()
            .filter(|c| c.starts_with(&last))
            .collect();
        match matches.len() {
            0 => {}
            1 => self.buffer = format!("{}{}", head, matches[0]),
            _ => {
                // extend to the longest common prefix
                let mut prefix = matches[0].clone();
                for m in &matches[1..] {
                    while !m.starts_with(&prefix) {
                        prefix.pop();
                    }
                }
                if prefix.len() > last.len() {
                    self.buffer = format!("{}{}", head, prefix);
                }
            }
        }
    }
}

/// Save a patch for `:w`/`:x`: resolves the target name (explicit arg or the
/// module's current patch), saves, and updates `patch_name` + `baseline`.
/// Returns the status message either way.
pub fn ex_write<P: serde::Serialize>(
    name: Option<String>,
    patch_name: &mut Option<String>,
    baseline: &mut String,
    params: &P,
) -> Result<String, String> {
    let name = name
        .or_else(|| patch_name.clone())
        .ok_or_else(|| String::from("No patch name (use :w <name>)"))?;
    crate::state::save_patch(&name, params).map_err(|e| e.to_string())?;
    *patch_name = Some(name.clone());
    *baseline = crate::state::to_toml_string(params).unwrap_or_default();
    Ok(format!("Wrote {}", name))
}

/// True when the current params no longer match the last-written baseline.
pub fn is_dirty<P: serde::Serialize>(params: &P, baseline: &str) -> bool {
    crate::state::to_toml_string(params).unwrap_or_default() != baseline
}

/// Patch names (without `.toml`) in the patches dir, for Tab completion.
pub fn patch_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    e.file_name()
                        .to_str()
                        .and_then(|n| n.strip_suffix(".toml"))
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_write_forms() {
        assert_eq!(parse("w"), ExCommand::Write(None));
        assert_eq!(parse("w bass-lead"), ExCommand::Write(Some("bass-lead".into())));
        assert_eq!(parse("write  spaced "), ExCommand::Write(Some("spaced".into())));
    }

    #[test]
    fn parse_edit_requires_name() {
        assert_eq!(parse("e bass"), ExCommand::Edit("bass".into()));
        assert_eq!(parse("e"), ExCommand::Unknown("e".into()));
    }

    #[test]
    fn parse_quit_forms() {
        assert_eq!(parse("q"), ExCommand::Quit { force: false });
        assert_eq!(parse("q!"), ExCommand::Quit { force: true });
        assert_eq!(parse("x"), ExCommand::WriteQuit(None));
        assert_eq!(parse("wq name"), ExCommand::WriteQuit(Some("name".into())));
    }

    #[test]
    fn parse_set() {
        assert_eq!(parse("set bpm 128"), ExCommand::Set("bpm".into(), "128".into()));
        assert_eq!(parse("set bpm"), ExCommand::Unknown("set bpm".into()));
        assert_eq!(parse("nonsense"), ExCommand::Unknown("nonsense".into()));
    }

    #[test]
    fn exline_submit_flow() {
        let mut ex = ExLine::default();
        ex.open();
        assert!(ex.is_active());
        for c in "w foo".chars() {
            assert_eq!(ex.handle_key(KeyCode::Char(c), &[]), ExEvent::Pending);
        }
        assert_eq!(ex.display(), ":w foo");
        assert_eq!(
            ex.handle_key(KeyCode::Enter, &[]),
            ExEvent::Submit(ExCommand::Write(Some("foo".into())))
        );
        assert!(!ex.is_active());
    }

    #[test]
    fn exline_esc_and_empty_backspace_cancel() {
        let mut ex = ExLine::default();
        ex.open();
        ex.handle_key(KeyCode::Char('q'), &[]);
        assert_eq!(ex.handle_key(KeyCode::Esc, &[]), ExEvent::Cancelled);
        assert!(!ex.is_active());

        ex.open();
        assert_eq!(ex.handle_key(KeyCode::Backspace, &[]), ExEvent::Cancelled);
        assert!(!ex.is_active());
    }

    #[test]
    fn exline_empty_enter_cancels() {
        let mut ex = ExLine::default();
        ex.open();
        assert_eq!(ex.handle_key(KeyCode::Enter, &[]), ExEvent::Cancelled);
    }

    #[test]
    fn tab_completes_unique_and_common_prefix() {
        let candidates = vec!["bass-lead".to_string(), "bass-sub".to_string(), "arp".to_string()];
        let mut ex = ExLine::default();
        ex.open();
        for c in "e a".chars() {
            ex.handle_key(KeyCode::Char(c), &candidates);
        }
        ex.handle_key(KeyCode::Tab, &candidates);
        assert_eq!(ex.display(), ":e arp");

        let mut ex = ExLine::default();
        ex.open();
        for c in "e b".chars() {
            ex.handle_key(KeyCode::Char(c), &candidates);
        }
        ex.handle_key(KeyCode::Tab, &candidates);
        assert_eq!(ex.display(), ":e bass-", "extends to common prefix");
    }
}
