//! Drives & mounts: the mounts in two sections, and a folder picker into any of them — space
//! uses a folder for a new job

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Row, Table},
    Frame,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::fmt::{fmt_count, truncate_path};
use crate::host::{mount_rows, Mount};

const DIM: Style = Style::new().fg(Color::DarkGray);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

/// A folder being browsed: its subfolders (".", the folder itself, first) and how many files
/// it holds directly
pub(crate) struct Folder {
    pub(crate) dir: PathBuf,
    pub(crate) names: Vec<String>,
    pub(crate) files: usize,
    pub(crate) selected: usize,
}

pub(crate) struct DrivesView {
    pub(crate) mounts: Vec<Mount>,
    pub(crate) selected: usize,
    pub(crate) folder: Option<Folder>,
    jobs: Vec<(String, String)>, // (path, name) of every job, to mark folders they cover
}

pub(crate) enum DrivesOutcome {
    Stay,
    Back,
    Use(String), // a new job for this path
    Message(String),
}

impl Folder {
    fn read(dir: PathBuf) -> Result<Folder, String> {
        let mut names = Vec::new();
        let mut files = 0;
        for entry in fs::read_dir(&dir).map_err(|e| format!("can't open {}: {}", dir.display(), e))?.flatten() {
            // Symlinked folders are listed (as they'd be indexed: not followed, but shown)
            match entry.file_type() {
                Ok(t) if t.is_dir() => names.push(entry.file_name().to_string_lossy().into_owned()),
                Ok(_) => files += 1,
                Err(_) => {}
            }
        }
        names.sort_by_key(|n| (n.starts_with('.'), n.to_lowercase()));
        names.insert(0, ".".into());
        Ok(Folder { dir, names, files, selected: 0 })
    }

    /// The highlighted folder's path
    pub(crate) fn chosen(&self) -> PathBuf {
        match self.names.get(self.selected).map(String::as_str) {
            Some(".") | None => self.dir.clone(),
            Some(name) => self.dir.join(name),
        }
    }
}

impl DrivesView {
    pub(crate) fn new(mounts: Vec<Mount>, jobs: Vec<(String, String)>) -> Self {
        DrivesView { mounts, selected: 0, folder: None, jobs }
    }

    /// The footer's command: what space would create, or `diskunder host` on the drives list
    pub(crate) fn command(&self) -> String {
        match &self.folder {
            Some(f) => format!("diskunder jobs add NAME update {}", f.chosen().display()),
            None => "diskunder host".into(),
        }
    }

    pub(crate) fn keys(&self) -> &'static str {
        match &self.folder {
            Some(_) => "↑↓ select · ⏎ / → into folder · ⌫ / ← up · space new job for this folder · c copy command · Esc back",
            None => "↑↓ select · ⏎ / → browse its folders · space new job for this mount · c copy command · Esc back",
        }
    }

    /// The job at `path`, or a note that jobs cover folders inside it
    fn job_note(&self, path: &Path) -> Option<String> {
        if let Some((_, name)) = self.jobs.iter().find(|(p, _)| Path::new(p) == path) {
            return Some(format!("job {}", name));
        }
        self.jobs.iter().any(|(p, _)| Path::new(p).starts_with(path)).then(|| "jobs inside".to_string())
    }

    fn open(&mut self, dir: PathBuf) -> DrivesOutcome {
        match Folder::read(dir) {
            Ok(f) => {
                self.folder = Some(f);
                DrivesOutcome::Stay
            }
            Err(e) => DrivesOutcome::Message(e),
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> DrivesOutcome {
        let Some(folder) = &mut self.folder else {
            return match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected = self.selected.saturating_sub(1);
                    DrivesOutcome::Stay
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected = (self.selected + 1).min(self.mounts.len().saturating_sub(1));
                    DrivesOutcome::Stay
                }
                KeyCode::Enter | KeyCode::Right => match self.mounts.get(self.selected) {
                    Some(m) => self.open(PathBuf::from(&m.point)),
                    None => DrivesOutcome::Stay,
                },
                KeyCode::Char(' ') | KeyCode::Char('n') => match self.mounts.get(self.selected) {
                    Some(m) => DrivesOutcome::Use(m.point.clone()),
                    None => DrivesOutcome::Stay,
                },
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Left => DrivesOutcome::Back,
                _ => DrivesOutcome::Stay,
            };
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => folder.selected = folder.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => folder.selected = (folder.selected + 1).min(folder.names.len() - 1),
            KeyCode::Enter | KeyCode::Right if folder.selected > 0 => {
                let dir = folder.chosen();
                return self.open(dir);
            }
            KeyCode::Char(' ') | KeyCode::Char('n') => return DrivesOutcome::Use(folder.chosen().to_string_lossy().into_owned()),
            KeyCode::Backspace | KeyCode::Left => {
                // Up a folder; from the mount itself, back to the drives list
                let mount = self.mounts.get(self.selected).map(|m| PathBuf::from(&m.point));
                let (dir, at_mount) = (folder.dir.clone(), mount.as_deref() == Some(folder.dir.as_path()));
                match dir.parent() {
                    Some(up) if !at_mount => {
                        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
                        if let DrivesOutcome::Message(e) = self.open(up.to_path_buf()) {
                            return DrivesOutcome::Message(e);
                        }
                        // Land on the folder we came out of
                        if let (Some(f), Some(name)) = (&mut self.folder, name) {
                            f.selected = f.names.iter().position(|n| *n == name).unwrap_or(0);
                        }
                    }
                    _ => self.folder = None,
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => self.folder = None,
            _ => {}
        }
        DrivesOutcome::Stay
    }

    pub(crate) fn draw(&self, f: &mut Frame, area: Rect) {
        match &self.folder {
            Some(folder) => self.draw_folder(f, folder, area),
            None => self.draw_mounts(f, area),
        }
    }

    fn draw_folder(&self, f: &mut Frame, folder: &Folder, area: Rect) {
        let width = area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = folder
            .names
            .iter()
            .map(|name| {
                let path = if name == "." { folder.dir.clone() } else { folder.dir.join(name) };
                let label = if name == "." {
                    format!(". (this folder — {} files, {} folders)", fmt_count(folder.files as i64), fmt_count(folder.names.len() as i64 - 1))
                } else {
                    format!("{}/", name)
                };
                let note = self.job_note(&path).map(|n| format!("   {}", n)).unwrap_or_default();
                let style = if name.starts_with('.') && name != "." { DIM } else { Style::new() };
                let room = width.saturating_sub(note.chars().count() + 2);
                ListItem::new(Line::from(vec![Span::styled(truncate_path(&label, room), style), Span::styled(note, Style::new().fg(Color::Cyan))]))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(folder.selected));
        let title = format!(" {} ", truncate_path(&folder.dir.to_string_lossy(), width.saturating_sub(2)));
        f.render_stateful_widget(
            List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▸ "),
            area,
            &mut state,
        );
    }

    /// A Local section then a Network one; `selected` counts mounts only
    fn draw_mounts(&self, f: &mut Frame, area: Rect) {
        let cells = mount_rows(&self.mounts);
        let mut rows: Vec<Row> = Vec::new();
        let mut at = 0; // table row of the selection, for scrolling
        for (title, network) in [("Local drives", false), ("Network mounts", true)] {
            if !rows.is_empty() {
                rows.push(Row::new(vec![""]));
            }
            rows.push(Row::new(vec![Span::styled(title, BOLD)]));
            let mut any = false;
            for (i, (m, c)) in self.mounts.iter().zip(&cells).enumerate().filter(|(_, (m, _))| m.network == network) {
                any = true;
                let style = if i == self.selected {
                    at = rows.len();
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new()
                };
                let mut c = c.clone();
                c[0] = format!("  {}", m.point);
                c.insert(5, self.job_note(Path::new(&m.point)).unwrap_or_default());
                rows.push(Row::new(c).style(style));
            }
            if !any {
                rows.push(Row::new(vec![Span::styled("  (none)", DIM)]));
            }
        }
        let visible = area.height.saturating_sub(3) as usize;
        let skip = at.saturating_sub(visible.saturating_sub(1));
        let widths = [
            Constraint::Min(24),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(15),
            Constraint::Length(9),
            Constraint::Length(14),
            Constraint::Min(16),
        ];
        let header = Row::new(vec!["MOUNT", "TYPE", "SIZE", "USED", "FREE", "JOBS", "SOURCE"]).style(DIM);
        f.render_widget(
            Table::new(rows.into_iter().skip(skip), widths)
                .header(header)
                .block(Block::default().borders(Borders::ALL).title(" Drives & mounts — ⏎ browse · space new job ")),
            area,
        );
    }
}
