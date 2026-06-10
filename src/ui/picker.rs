//! Shared `@` source-picker overlay: a list of live modulation sources read
//! from the manifest. j/k navigate, Enter binds, Esc cancels, x (or choosing
//! the "— none —" row) unbinds.

use crossterm::event::KeyCode;

use crate::routing::SourceAddr;

#[derive(Debug, Clone, PartialEq)]
pub enum PickerEvent {
    Pending,
    Cancelled,
    /// Some(addr) = bind to this source; None = the first special row
    /// (usually "— none —" = unbind).
    Chosen(Option<SourceAddr>),
    /// A special row other than the first (e.g. "— off —" at index 1).
    ChosenSpecial(usize),
}

#[derive(Debug, Default)]
pub struct Picker {
    specials: Vec<String>,
    sources: Vec<SourceAddr>,
    selected: usize,
    active: bool,
}

impl Picker {
    pub fn open(&mut self, sources: Vec<SourceAddr>, current: Option<&SourceAddr>) {
        self.open_with(vec![String::from("— none —")], sources, current, 0);
    }

    /// Open with custom special rows ahead of the sources (e.g. the
    /// envelope trigger's "— any note —" / "— off —"). `current_special`
    /// selects a special row when `current` is None.
    pub fn open_with(
        &mut self,
        specials: Vec<String>,
        sources: Vec<SourceAddr>,
        current: Option<&SourceAddr>,
        current_special: usize,
    ) {
        let n = specials.len();
        self.selected = current
            .and_then(|c| sources.iter().position(|s| s == c))
            .map(|i| i + n)
            .unwrap_or(current_special.min(n.saturating_sub(1)));
        self.specials = specials;
        self.sources = sources;
        self.active = true;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Rows to render, with the selected index.
    pub fn rows(&self) -> (Vec<String>, usize) {
        let mut rows = self.specials.clone();
        rows.extend(self.sources.iter().map(|s| s.to_string()));
        (rows, self.selected)
    }

    /// Per-row source address (None for special rows) — for cable coloring.
    pub fn row_sources(&self) -> Vec<Option<&SourceAddr>> {
        let mut v: Vec<Option<&SourceAddr>> = vec![None; self.specials.len()];
        v.extend(self.sources.iter().map(Some));
        v
    }

    pub fn handle_key(&mut self, code: KeyCode) -> PickerEvent {
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected =
                    (self.selected + 1).min(self.specials.len() + self.sources.len() - 1);
                PickerEvent::Pending
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PickerEvent::Pending
            }
            KeyCode::Char('g') => {
                self.selected = 0;
                PickerEvent::Pending
            }
            KeyCode::Char('G') => {
                self.selected = self.specials.len() + self.sources.len() - 1;
                PickerEvent::Pending
            }
            KeyCode::Enter => {
                self.active = false;
                let n = self.specials.len();
                if self.selected == 0 {
                    PickerEvent::Chosen(None)
                } else if self.selected < n {
                    PickerEvent::ChosenSpecial(self.selected)
                } else {
                    PickerEvent::Chosen(Some(self.sources[self.selected - n].clone()))
                }
            }
            KeyCode::Char('x') => {
                self.active = false;
                PickerEvent::Chosen(None)
            }
            KeyCode::Esc | KeyCode::Char('@') | KeyCode::Char('q') => {
                self.active = false;
                PickerEvent::Cancelled
            }
            _ => PickerEvent::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources() -> Vec<SourceAddr> {
        ["sequencer/0/t1", "sequencer/0/t2", "envelope/0/ch1"]
            .iter()
            .map(|s| SourceAddr::parse(s).unwrap())
            .collect()
    }

    #[test]
    fn navigate_and_choose() {
        let mut p = Picker::default();
        p.open(sources(), None);
        assert!(p.is_active());
        assert_eq!(p.rows().1, 0);
        p.handle_key(KeyCode::Char('j'));
        p.handle_key(KeyCode::Char('j'));
        let ev = p.handle_key(KeyCode::Enter);
        assert_eq!(
            ev,
            PickerEvent::Chosen(Some(SourceAddr::parse("sequencer/0/t2").unwrap()))
        );
        assert!(!p.is_active());
    }

    #[test]
    fn none_row_unbinds_and_esc_cancels() {
        let mut p = Picker::default();
        p.open(sources(), None);
        assert_eq!(p.handle_key(KeyCode::Enter), PickerEvent::Chosen(None));

        p.open(sources(), None);
        assert_eq!(p.handle_key(KeyCode::Esc), PickerEvent::Cancelled);
        assert!(!p.is_active());

        p.open(sources(), None);
        assert_eq!(p.handle_key(KeyCode::Char('x')), PickerEvent::Chosen(None));
    }

    #[test]
    fn opens_on_current_binding() {
        let mut p = Picker::default();
        let cur = SourceAddr::parse("envelope/0/ch1").unwrap();
        p.open(sources(), Some(&cur));
        assert_eq!(p.rows().1, 3, "selection starts on the bound source");
        assert_eq!(p.handle_key(KeyCode::Enter), PickerEvent::Chosen(Some(cur)));
    }

    #[test]
    fn selection_clamps() {
        let mut p = Picker::default();
        p.open(sources(), None);
        for _ in 0..10 {
            p.handle_key(KeyCode::Char('j'));
        }
        assert_eq!(p.rows().1, 3, "clamped to last row");
        p.handle_key(KeyCode::Char('g'));
        assert_eq!(p.rows().1, 0);
    }

    #[test]
    fn specials_offset_sources_and_report_off() {
        let mut p = Picker::default();
        p.open_with(
            vec!["— any note —".into(), "— off —".into()],
            sources(),
            None,
            0,
        );
        // row 1 = off
        p.handle_key(KeyCode::Char('j'));
        assert_eq!(p.handle_key(KeyCode::Enter), PickerEvent::ChosenSpecial(1));

        // row 2 = first source
        p.open_with(
            vec!["— any note —".into(), "— off —".into()],
            sources(),
            None,
            1,
        );
        assert_eq!(p.rows().1, 1, "opens on the current special");
        p.handle_key(KeyCode::Char('j'));
        assert_eq!(
            p.handle_key(KeyCode::Enter),
            PickerEvent::Chosen(Some(SourceAddr::parse("sequencer/0/t1").unwrap()))
        );
    }
}
