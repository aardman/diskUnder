//! Plain-text progress for cron, pipes and --no-tui

use std::sync::mpsc::Receiver;

use crate::fmt::fmt_bytes;
use crate::progress::AppState;
use crate::runlog::RunLog;
use crate::scan::WorkerMsg;

pub(crate) fn run_plain(state: &mut AppState, rx: Receiver<WorkerMsg>, runlog: &mut RunLog) {
    eprintln!("{} {}", state.mode, state.target_path);

    for msg in rx {
        // Per-file read errors are only counted (there can be thousands); traversal and
        // database errors are rare and need explaining, so print them
        match &msg {
            WorkerMsg::Info(line) => eprintln!("{}", line),
            WorkerMsg::Error { context, error } => eprintln!("warn: {} — {}", context, error),
            WorkerMsg::Fatal { context, error } => eprintln!("error: {} — {}", context, error),
            _ => {}
        }
        runlog.observe(&msg);
        let print_progress = matches!(msg, WorkerMsg::BatchWritten(_) | WorkerMsg::BatchFailed { .. });
        let done = matches!(msg, WorkerMsg::Done);

        state.apply(msg);

        if print_progress {
            let processed = state.files_hashed + state.files_errored;
            let total_str = state
                .total_files
                .map(|n| n.to_string())
                .unwrap_or_else(|| "…".into());
            eprintln!(
                "  {}/{} processed — {} written, {} errors",
                processed, total_str, state.files_written, state.errors_total()
            );
        }

        if done {
            break;
        }
    }

    eprintln!(
        "complete: {} files — {} {}, {} written, {} removed, {}, {} duplicate groups",
        state.files_found,
        state.files_skipped,
        state.skipped_label(),
        state.files_written,
        state.files_removed,
        state.errors_summary(),
        state.dup_keys.len(),
    );
    if !state.dup_keys.is_empty() {
        eprintln!("wasted: {}", fmt_bytes(state.dup_wasted));
    }
}
