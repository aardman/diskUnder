//! The Quarantine view: batches of quarantined files, what's in each, and restoring or purging
//! a batch

use crossterm::event::{KeyCode, KeyEvent};

use crate::quarantine::{batch_files, batch_rows, BatchRow};

/// A change waiting for y
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Confirm {
    Restore,
    Purge,
}

pub(crate) enum QuarantineOutcome {
    Stay,
    Back,
    Restore(String),
    Purge(String),
}

pub(crate) struct QuarantineView {
    pub(crate) db: String,
    pub(crate) batches: Vec<BatchRow>,
    pub(crate) selected: usize,
    pub(crate) files: Vec<(String, i64, String)>, // the selected batch's: (state, size, original)
    pub(crate) error: Option<String>,
    pub(crate) confirm: Option<Confirm>,
}

impl QuarantineView {
    pub(crate) fn new(db: &str) -> Self {
        let mut v = QuarantineView { db: db.to_string(), batches: Vec::new(), selected: 0, files: Vec::new(), error: None, confirm: None };
        v.reload();
        // Newest first is what's usually wanted
        v.selected = v.batches.len().saturating_sub(1);
        v.load_files();
        v
    }

    pub(crate) fn reload(&mut self) {
        match batch_rows(&self.db) {
            Ok(b) => {
                self.batches = b;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        self.selected = self.selected.min(self.batches.len().saturating_sub(1));
        self.load_files();
    }

    fn load_files(&mut self) {
        self.files = self.batch().map_or_else(Vec::new, |b| batch_files(&self.db, &b.batch).unwrap_or_default());
    }

    pub(crate) fn batch(&self) -> Option<&BatchRow> {
        self.batches.get(self.selected)
    }

    pub(crate) fn command(&self) -> String {
        let db = &self.db;
        match (self.batch(), self.confirm) {
            (Some(b), Some(Confirm::Restore)) => format!("diskunder quarantine restore {} --db {}", b.batch, db),
            (Some(b), Some(Confirm::Purge)) => format!("diskunder quarantine purge {} --older-than 0d --db {}", b.batch, db),
            (Some(b), None) => format!("diskunder quarantine show {} --db {}", b.batch, db),
            (None, _) => format!("diskunder quarantine list --db {}", db),
        }
    }

    pub(crate) fn keys(&self) -> &'static str {
        match self.confirm {
            Some(Confirm::Restore) => "y restore the batch · any other key cancels",
            Some(Confirm::Purge) => "y delete for good · any other key cancels",
            None => "↑↓ batches · r restore · p purge (delete for good) · c copy command · Esc back",
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> QuarantineOutcome {
        if let Some(c) = self.confirm.take() {
            let Some(b) = self.batch() else { return QuarantineOutcome::Stay };
            return match (key.code, c) {
                (KeyCode::Char('y'), Confirm::Restore) => QuarantineOutcome::Restore(b.batch.clone()),
                (KeyCode::Char('y'), Confirm::Purge) => QuarantineOutcome::Purge(b.batch.clone()),
                _ => QuarantineOutcome::Stay,
            };
        }
        let before = self.selected;
        let held = self.batch().is_some_and(|b| b.files > 0);
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => return QuarantineOutcome::Back,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1).min(self.batches.len().saturating_sub(1)),
            KeyCode::Char('r') if held => self.confirm = Some(Confirm::Restore),
            KeyCode::Char('p') if held => self.confirm = Some(Confirm::Purge),
            _ => {}
        }
        if self.selected != before {
            self.load_files();
        }
        QuarantineOutcome::Stay
    }
}
