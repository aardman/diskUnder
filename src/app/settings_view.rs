//! The Settings view: every setting and type group, edited in place

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::Span,
    widgets::{Block, Borders, Cell, Row, Table},
    Frame,
};
use std::path::{Path, PathBuf};

use crate::settings::{self, Kind, Settings, BUILTIN_GROUPS, SETTINGS};

/// The sections, in order, with the setting-key prefix each covers
pub(crate) const SECTIONS: [(&str, &str); 5] = [
    ("Start", "start."),
    ("Defaults for new jobs", "defaults."),
    ("File type groups — use @name in any type field", "types."),
    ("Deletion — takes effect in v6", "deletion."),
    ("Logging — takes effect next time diskunder starts", "log."),
];

#[derive(Clone, PartialEq)]
pub(crate) enum RowKind {
    Header(usize),
    Setting(&'static str),
    Group(String),
    AddGroup,
}

pub(crate) struct SettingsView {
    pub(crate) rows: Vec<RowKind>,
    pub(crate) selected: usize,
    pub(crate) editing: Option<String>, // the value being typed
    pub(crate) confirming: bool,         // turning off "dry run first" asks
    store: PathBuf,
}

pub(crate) enum SettingsOutcome {
    Stay,
    Back,
    Changed(String), // a message; the app reloads its settings
    Message(String),
}

fn rows_for(s: &Settings) -> Vec<RowKind> {
    let mut rows = Vec::new();
    for (i, (_, prefix)) in SECTIONS.iter().enumerate() {
        rows.push(RowKind::Header(i));
        if *prefix == "types." {
            rows.extend(s.groups().into_iter().map(|(name, _)| RowKind::Group(name)));
            rows.push(RowKind::AddGroup);
        } else {
            rows.extend(SETTINGS.iter().filter(|k| k.0.starts_with(prefix)).map(|k| RowKind::Setting(k.0)));
        }
    }
    rows
}

impl SettingsView {
    /// Opens at `section` (its first row)
    pub(crate) fn new(store: &Path, s: &Settings, section: usize) -> Self {
        let rows = rows_for(s);
        let selected = rows.iter().position(|r| *r == RowKind::Header(section)).map_or(1, |i| i + 1);
        SettingsView { rows, selected, editing: None, confirming: false, store: store.to_path_buf() }
    }

    fn key_of(&self) -> Option<String> {
        match &self.rows[self.selected] {
            RowKind::Setting(k) => Some(k.to_string()),
            RowKind::Group(n) => Some(format!("types.{}", n)),
            _ => None,
        }
    }

    /// The command that does what's selected (or being typed)
    pub(crate) fn command(&self, s: &Settings) -> String {
        let q = |v: &str| if v.is_empty() || v.contains([' ', ',', '*', '@']) { format!("'{}'", v) } else { v.to_string() };
        match (&self.rows[self.selected], &self.editing) {
            (RowKind::AddGroup, Some(text)) => {
                let (name, exts) = text.split_once(':').unwrap_or((text, ""));
                format!("diskunder settings set types.{} {}", name.trim(), q(exts.trim()))
            }
            (RowKind::AddGroup, None) => "diskunder settings set types.NAME ext,ext".into(),
            (RowKind::Header(_), _) => "diskunder settings".into(),
            _ => {
                let key = self.key_of().unwrap();
                let value = self.editing.clone().unwrap_or_else(|| s.get(&key).to_string());
                format!("diskunder settings set {} {}", key, q(&value))
            }
        }
    }

    fn reload(&mut self, s: &Settings) {
        let keep = self.rows.get(self.selected).cloned();
        self.rows = rows_for(s);
        self.selected = keep.and_then(|k| self.rows.iter().position(|r| *r == k)).unwrap_or(self.selected).min(self.rows.len() - 1);
        if matches!(self.rows[self.selected], RowKind::Header(_)) {
            self.selected += 1;
        }
    }

    fn save(&mut self, s: &Settings, key: &str, value: &str) -> SettingsOutcome {
        match settings::set(&self.store, key, value) {
            Ok(v) => {
                self.editing = None;
                self.reload(&Settings::load(&self.store));
                if key.starts_with("types.") && v.is_empty() {
                    SettingsOutcome::Changed(format!("removed @{}", &key[6..]))
                } else {
                    let _ = s;
                    SettingsOutcome::Changed(format!("{} = {}", key, if v.is_empty() { "—" } else { &v }))
                }
            }
            Err(e) => SettingsOutcome::Message(e),
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent, s: &Settings) -> SettingsOutcome {
        if self.confirming {
            self.confirming = false;
            return match key.code {
                KeyCode::Char('y') => self.save(s, "deletion.dry_run_default", "no"),
                _ => SettingsOutcome::Message("dry run first stays on".into()),
            };
        }
        if let Some(text) = &mut self.editing {
            match key.code {
                KeyCode::Esc => self.editing = None,
                KeyCode::Enter => {
                    let text = text.clone();
                    return match &self.rows[self.selected] {
                        RowKind::AddGroup => match text.split_once(':') {
                            Some((name, exts)) if !name.trim().is_empty() && !exts.trim().is_empty() => {
                                self.save(s, &format!("types.{}", name.trim()), exts.trim())
                            }
                            _ => SettingsOutcome::Message("type the group as name: ext,ext — e.g. raw: cr2,nef,arw".into()),
                        },
                        _ => {
                            let key = self.key_of().unwrap();
                            self.save(s, &key, &text)
                        }
                    };
                }
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
                KeyCode::Char(c) => text.push(c),
                _ => {}
            }
            return SettingsOutcome::Stay;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return SettingsOutcome::Back,
            KeyCode::Up | KeyCode::Char('k') => {
                let mut i = self.selected;
                while i > 0 {
                    i -= 1;
                    if !matches!(self.rows[i], RowKind::Header(_)) {
                        self.selected = i;
                        break;
                    }
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let mut i = self.selected;
                while i + 1 < self.rows.len() {
                    i += 1;
                    if !matches!(self.rows[i], RowKind::Header(_)) {
                        self.selected = i;
                        break;
                    }
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let kind = match &self.rows[self.selected] {
                    RowKind::Setting(k) => SETTINGS.iter().find(|x| x.0 == *k).map(|x| x.2),
                    _ => None,
                };
                if kind == Some(Kind::YesNo) {
                    let key = self.key_of().unwrap();
                    let turning_off = s.yes(&key);
                    if key == "deletion.dry_run_default" && turning_off {
                        self.confirming = true;
                        return SettingsOutcome::Stay;
                    }
                    return self.save(s, &key, if turning_off { "no" } else { "yes" });
                }
                if kind == Some(Kind::Mode) {
                    let key = self.key_of().unwrap();
                    let next = if s.get(&key) == "update" { "index" } else { "update" };
                    return self.save(s, &key, next);
                }
                self.editing = Some(match &self.rows[self.selected] {
                    RowKind::AddGroup => String::new(),
                    _ => s.get(&self.key_of().unwrap()).to_string(),
                });
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                let Some(key) = self.key_of() else { return SettingsOutcome::Stay };
                let builtin = key.strip_prefix("types.").is_some_and(|n| BUILTIN_GROUPS.iter().any(|(b, _)| *b == n));
                let result = if key.starts_with("types.") && !builtin {
                    settings::unset(&self.store, &key).map(|_| format!("removed @{}", &key[6..]))
                } else if key.starts_with("types.") {
                    settings::set(&self.store, &key, "").map(|_| format!("removed @{} (unset types.{} to restore it)", &key[6..], &key[6..]))
                } else {
                    settings::unset(&self.store, &key).map(|_| format!("{} back to its default", key))
                };
                return match result {
                    Ok(msg) => {
                        self.reload(&Settings::load(&self.store));
                        SettingsOutcome::Changed(msg)
                    }
                    Err(e) => SettingsOutcome::Message(e),
                };
            }
            _ => {}
        }
        SettingsOutcome::Stay
    }

    pub(crate) fn draw(&self, f: &mut Frame, s: &Settings, area: Rect) {
        let dim = Style::new().fg(Color::DarkGray);
        let visible = area.height.saturating_sub(2) as usize;
        let skip = self.selected.saturating_sub(visible.saturating_sub(2));
        let rows: Vec<Row> = self
            .rows
            .iter()
            .enumerate()
            .skip(skip)
            .map(|(i, r)| {
                let on = i == self.selected;
                let style = if on && self.editing.is_none() { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
                let value = |v: String| {
                    if on {
                        if let Some(text) = &self.editing {
                            return Cell::from(Span::styled(format!("{}▏", text), Style::new().fg(Color::Cyan)));
                        }
                    }
                    Cell::from(if v.is_empty() { "—".to_string() } else { v })
                };
                match r {
                    RowKind::Header(n) => Row::new(vec![Cell::from(Span::styled(SECTIONS[*n].0, Style::new().add_modifier(Modifier::BOLD))), Cell::from(""), Cell::from("")]),
                    RowKind::Setting(k) => {
                        let (_, default, _, about) = SETTINGS.iter().find(|x| x.0 == *k).unwrap();
                        let note = if s.get(k) == *default { format!("{} (default)", about) } else { about.to_string() };
                        Row::new(vec![Cell::from(format!("  {}", k)), value(s.get(k).to_string()), Cell::from(Span::styled(note, dim))]).style(style)
                    }
                    RowKind::Group(name) => {
                        let exts = s.groups().into_iter().find(|(n, _)| n == name).map(|(_, v)| v).unwrap_or_default();
                        Row::new(vec![Cell::from(format!("  @{}", name)), value(exts), Cell::from(Span::styled("x removes", dim))]).style(style)
                    }
                    RowKind::AddGroup => Row::new(vec![
                        Cell::from("  + add a group"),
                        value(String::new()),
                        Cell::from(Span::styled("⏎ then type  name: ext,ext", dim)),
                    ])
                    .style(style),
                }
            })
            .collect();
        let title = if self.confirming {
            " Settings — turn off dry run first? Dedupe in v6 could then act without a preview. y turns it off · any key keeps it "
        } else {
            " Settings "
        };
        f.render_widget(
            Table::new(rows, [Constraint::Length(30), Constraint::Percentage(45), Constraint::Min(20)])
                .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
    }
}
