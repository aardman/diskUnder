//! Formatting helpers for sizes, counts, paths and timestamps

use chrono::{DateTime, Utc};
use std::time::SystemTime;

pub(crate) fn fmt_time(t: SystemTime) -> String {
    let dt: DateTime<Utc> = t.into();
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

// ─── UI helpers ──────────────────────────────────────────────────────────────

pub(crate) fn fmt_bytes(b: u64) -> String {
    const GB: u64 = 1 << 30;
    const MB: u64 = 1 << 20;
    const KB: u64 = 1 << 10;
    if b >= GB {
        format!("{:.1} GB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.1} MB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.1} KB", b as f64 / KB as f64)
    } else {
        format!("{} B", b)
    }
}

/// Truncate a path string to fit within `width`, adding a leading ellipsis.
pub(crate) fn truncate_path(s: &str, width: usize) -> String {
    if s.len() <= width {
        s.to_owned()
    } else {
        format!("…{}", &s[s.len().saturating_sub(width.saturating_sub(1))..])
    }
}

/// 1234567 → "1,234,567"
pub(crate) fn fmt_count(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{}", out) } else { out }
}

/// Stored UTC timestamp → local time for display
pub(crate) fn fmt_local(utc: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(utc, "%Y-%m-%d %H:%M:%S")
        .map(|t| {
            t.and_utc()
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| utc.to_string())
}
