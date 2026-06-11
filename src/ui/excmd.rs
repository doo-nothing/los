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
    /// Still editing; redraw the prompt. Any live preview should revert.
    Pending,
    /// Prompt dismissed without a command. Revert any live preview.
    Cancelled,
    /// Enter pressed: execute this command (or commit the live preview if
    /// it matches).
    Submit(ExCommand),
    /// An arrow key moved the completion menu: the module MAY apply this
    /// full command line as a live, revertible preview (audition). Only
    /// arrows audition — typing + Tab + Enter stay silent and clean.
    Preview(String),
}

/// A completion source: `(head, word)` are the command line split at the
/// word being completed ("set cycle", "pi") → candidates for that word.
/// Modules provide one per context; [`standard_completer`] covers modules
/// with no custom values (command names + patch names).
pub type Completer<'a> = &'a dyn Fn(&str, &str) -> Vec<String>;

/// Command names every module's ex line understands.
const COMMANDS: &[&str] = &["w", "e", "q", "q!", "wq", "set"];

/// Keep only candidates matching the typed prefix.
pub fn filter_prefix(items: Vec<String>, word: &str) -> Vec<String> {
    items.into_iter().filter(|c| c.starts_with(word)).collect()
}

/// The completer for modules without custom value menus: command names in
/// the first word, patch names after `:e`/`:w`/`:wq`.
pub fn standard_completer(patches: Vec<String>) -> impl Fn(&str, &str) -> Vec<String> {
    move |head, word| {
        let items: Vec<String> = if head.is_empty() {
            COMMANDS.iter().map(|s| s.to_string()).collect()
        } else if matches!(head, "e" | "edit" | "w" | "write" | "wq" | "x") {
            patches.clone()
        } else {
            Vec::new()
        };
        filter_prefix(items, word)
    }
}

/// The interactive `:` prompt state. Inactive until [`ExLine::open`].
///
/// Carries per-module command history (`Up` recalls when no menu row is
/// selected) and a completion menu (`Down`/`Up` move it — and audition,
/// see [`ExEvent::Preview`]; `Tab` completes/cycles silently; typing
/// filters).
#[derive(Debug, Default)]
pub struct ExLine {
    buffer: String,
    active: bool,
    history: Vec<String>,
    hist_pos: Option<usize>,
    menu: Vec<String>,
    menu_sel: Option<usize>,
    /// Byte offset where the word being completed starts.
    word_start: usize,
}

impl ExLine {
    pub fn open(&mut self) {
        self.active = true;
        self.buffer.clear();
        self.hist_pos = None;
        self.menu.clear();
        self.menu_sel = None;
        self.word_start = 0;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The prompt text to render (including the leading `:`).
    pub fn display(&self) -> String {
        format!(":{}", self.buffer)
    }

    /// The completion menu for rendering: (items, selected row). Empty
    /// slice = nothing to show.
    pub fn menu(&self) -> (&[String], Option<usize>) {
        (&self.menu, self.menu_sel)
    }

    /// Recompute the menu for the current buffer (modules call this right
    /// after `open()` so the command list shows immediately).
    pub fn refresh(&mut self, complete: Completer) {
        let ws = self
            .buffer
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.word_start = ws;
        let head = self.buffer[..ws].trim().to_string();
        let word = self.buffer[ws..].to_string();
        self.menu = complete(&head, &word);
        self.menu_sel = None;
    }

    /// Replace the completed word with the selected menu row.
    fn apply_menu(&mut self) {
        if let Some(item) = self.menu_sel.and_then(|i| self.menu.get(i)) {
            self.buffer.truncate(self.word_start);
            self.buffer.push_str(item);
        }
    }

    /// Feed a key event into the prompt.
    pub fn handle_key(&mut self, code: KeyCode, complete: Completer) -> ExEvent {
        match code {
            KeyCode::Enter => {
                self.active = false;
                self.menu.clear();
                self.menu_sel = None;
                let line = std::mem::take(&mut self.buffer);
                if line.trim().is_empty() {
                    ExEvent::Cancelled
                } else {
                    if self.history.last().map(String::as_str) != Some(line.as_str()) {
                        self.history.push(line.clone());
                    }
                    self.hist_pos = None;
                    ExEvent::Submit(parse(&line))
                }
            }
            KeyCode::Esc => {
                self.active = false;
                self.buffer.clear();
                self.menu.clear();
                self.menu_sel = None;
                ExEvent::Cancelled
            }
            KeyCode::Backspace => {
                self.menu_sel = None;
                self.hist_pos = None;
                if self.buffer.pop().is_none() {
                    // backspace on an empty prompt dismisses it, like vi
                    self.active = false;
                    return ExEvent::Cancelled;
                }
                self.refresh(complete);
                ExEvent::Pending
            }
            // Tab completes silently: common-prefix first, then cycles
            KeyCode::Tab => {
                if self.menu.is_empty() {
                    self.refresh(complete);
                }
                match (self.menu_sel, self.menu.len()) {
                    (_, 0) => {}
                    (Some(i), n) => {
                        self.menu_sel = Some((i + 1) % n);
                        self.apply_menu();
                    }
                    (None, 1) => {
                        self.menu_sel = Some(0);
                        self.apply_menu();
                    }
                    (None, _) => {
                        // extend to the longest common prefix; if the word
                        // is already there, start cycling
                        let mut prefix = self.menu[0].clone();
                        for m in &self.menu[1..] {
                            while !m.starts_with(prefix.as_str()) {
                                prefix.pop();
                            }
                        }
                        let word = &self.buffer[self.word_start..];
                        if prefix.len() > word.len() {
                            self.buffer.truncate(self.word_start);
                            self.buffer.push_str(&prefix);
                        } else {
                            self.menu_sel = Some(0);
                            self.apply_menu();
                        }
                    }
                }
                ExEvent::Pending
            }
            // Down enters/moves the menu — this is what arms the audition
            KeyCode::Down => {
                if !self.menu.is_empty() {
                    let n = self.menu.len();
                    self.menu_sel = Some(self.menu_sel.map_or(0, |i| (i + 1) % n));
                    self.apply_menu();
                    return ExEvent::Preview(self.buffer.clone());
                }
                ExEvent::Pending
            }
            // Up moves the menu when a row is selected; otherwise history
            KeyCode::Up => {
                if let Some(i) = self.menu_sel {
                    let n = self.menu.len();
                    self.menu_sel = Some((i + n - 1) % n);
                    self.apply_menu();
                    return ExEvent::Preview(self.buffer.clone());
                }
                if !self.history.is_empty() {
                    let pos = match self.hist_pos {
                        None => self.history.len() - 1,
                        Some(0) => 0,
                        Some(p) => p - 1,
                    };
                    self.hist_pos = Some(pos);
                    self.buffer = self.history[pos].clone();
                    self.refresh(complete);
                }
                ExEvent::Pending
            }
            KeyCode::Char(c) => {
                self.menu_sel = None;
                self.hist_pos = None;
                self.buffer.push(c);
                self.refresh(complete);
                ExEvent::Pending
            }
            _ => ExEvent::Pending,
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
        assert_eq!(
            parse("w bass-lead"),
            ExCommand::Write(Some("bass-lead".into()))
        );
        assert_eq!(
            parse("write  spaced "),
            ExCommand::Write(Some("spaced".into()))
        );
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
        assert_eq!(
            parse("set bpm 128"),
            ExCommand::Set("bpm".into(), "128".into())
        );
        assert_eq!(parse("set bpm"), ExCommand::Unknown("set bpm".into()));
        assert_eq!(parse("nonsense"), ExCommand::Unknown("nonsense".into()));
    }

    /// A completer with no candidates (the old bare-tab behavior).
    fn none(_: &str, _: &str) -> Vec<String> {
        Vec::new()
    }

    #[test]
    fn exline_submit_flow() {
        let mut ex = ExLine::default();
        ex.open();
        assert!(ex.is_active());
        for c in "w foo".chars() {
            assert_eq!(ex.handle_key(KeyCode::Char(c), &none), ExEvent::Pending);
        }
        assert_eq!(ex.display(), ":w foo");
        assert_eq!(
            ex.handle_key(KeyCode::Enter, &none),
            ExEvent::Submit(ExCommand::Write(Some("foo".into())))
        );
        assert!(!ex.is_active());
    }

    #[test]
    fn exline_esc_and_empty_backspace_cancel() {
        let mut ex = ExLine::default();
        ex.open();
        ex.handle_key(KeyCode::Char('q'), &none);
        assert_eq!(ex.handle_key(KeyCode::Esc, &none), ExEvent::Cancelled);
        assert!(!ex.is_active());

        ex.open();
        assert_eq!(ex.handle_key(KeyCode::Backspace, &none), ExEvent::Cancelled);
        assert!(!ex.is_active());
    }

    #[test]
    fn exline_empty_enter_cancels() {
        let mut ex = ExLine::default();
        ex.open();
        assert_eq!(ex.handle_key(KeyCode::Enter, &none), ExEvent::Cancelled);
    }

    #[test]
    fn tab_completes_unique_and_common_prefix() {
        let completer =
            standard_completer(vec!["bass-lead".into(), "bass-sub".into(), "arp".into()]);
        let mut ex = ExLine::default();
        ex.open();
        for c in "e a".chars() {
            ex.handle_key(KeyCode::Char(c), &completer);
        }
        ex.handle_key(KeyCode::Tab, &completer);
        assert_eq!(ex.display(), ":e arp");

        let mut ex = ExLine::default();
        ex.open();
        for c in "e b".chars() {
            ex.handle_key(KeyCode::Char(c), &completer);
        }
        ex.handle_key(KeyCode::Tab, &completer);
        assert_eq!(ex.display(), ":e bass-", "extends to common prefix");
        // a second Tab starts cycling the matches
        ex.handle_key(KeyCode::Tab, &completer);
        assert_eq!(ex.display(), ":e bass-lead");
        ex.handle_key(KeyCode::Tab, &completer);
        assert_eq!(ex.display(), ":e bass-sub");
    }

    #[test]
    fn up_recalls_history_down_walks_menu() {
        let completer = standard_completer(vec!["bass".into(), "lead".into()]);
        let mut ex = ExLine::default();
        // submit once to seed history
        ex.open();
        for c in "e bass".chars() {
            ex.handle_key(KeyCode::Char(c), &completer);
        }
        ex.handle_key(KeyCode::Enter, &completer);
        // Up with no menu selection = history recall
        ex.open();
        assert_eq!(ex.handle_key(KeyCode::Up, &completer), ExEvent::Pending);
        assert_eq!(ex.display(), ":e bass");
        // Down enters the menu and ARMS the audition (Preview)
        ex.open();
        for c in "e ".chars() {
            ex.handle_key(KeyCode::Char(c), &completer);
        }
        let ev = ex.handle_key(KeyCode::Down, &completer);
        assert_eq!(ev, ExEvent::Preview(String::from("e bass")));
        assert_eq!(ex.menu().1, Some(0));
        let ev = ex.handle_key(KeyCode::Down, &completer);
        assert_eq!(ev, ExEvent::Preview(String::from("e lead")));
        // Up moves back within the menu once a row is selected
        let ev = ex.handle_key(KeyCode::Up, &completer);
        assert_eq!(ev, ExEvent::Preview(String::from("e bass")));
        // typing clears the selection (the module reverts its preview)
        ex.handle_key(KeyCode::Backspace, &completer);
        assert_eq!(ex.menu().1, None);
    }

    #[test]
    fn menus_filter_by_prefix_and_history_dedups() {
        let completer = standard_completer(vec![]);
        let mut ex = ExLine::default();
        ex.open();
        ex.refresh(&completer);
        assert!(ex.menu().0.len() >= 5, "command list shows on open");
        ex.handle_key(KeyCode::Char('s'), &completer);
        assert_eq!(ex.menu().0, &[String::from("set")]);
        ex.handle_key(KeyCode::Enter, &completer);
        ex.open();
        for c in "set".chars() {
            ex.handle_key(KeyCode::Char(c), &completer);
        }
        ex.handle_key(KeyCode::Enter, &completer);
        ex.open();
        ex.handle_key(KeyCode::Up, &completer);
        assert_eq!(ex.display(), ":set");
    }
}
