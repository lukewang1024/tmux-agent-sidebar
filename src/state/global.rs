use std::collections::HashMap;
use std::time::Instant;

use crate::tmux;

use super::filter::{RepoFilter, StatusFilter};

/// State shared across all sidebar instances via tmux global variables.
/// Synced from tmux at startup and on pane focus change (SIGUSR1).
pub struct GlobalState {
    pub status_filter: StatusFilter,
    /// Cursor owned by this frontend process. Unlike filters, it is never
    /// persisted to tmux or broadcast to peer sidebar instances.
    pub selected_pane_row: usize,
    pub repo_filter: RepoFilter,
    /// Last filter value successfully written to tmux.
    last_saved_filter: StatusFilter,
    /// Last repo filter value successfully written to tmux.
    last_saved_repo_filter: RepoFilter,
    /// When a shared-state write last queued a peer broadcast that still needs
    /// sending. Debounced so rapid changes (e.g. cycling the filter back and
    /// forth) collapse into a single SIGUSR1 fan-out instead of flooding every
    /// other instance on each keystroke.
    pending_broadcast_since: Option<Instant>,
}

impl Default for GlobalState {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalState {
    pub fn new() -> Self {
        Self {
            status_filter: StatusFilter::All,
            selected_pane_row: 0,
            repo_filter: RepoFilter::All,
            last_saved_filter: StatusFilter::All,
            last_saved_repo_filter: RepoFilter::All,
            pending_broadcast_since: None,
        }
    }

    /// Queue a debounced SIGUSR1 fan-out to peer sidebars. Called after writing
    /// a shared tmux option; the actual broadcast is sent by
    /// [`flush_pending_broadcast`] once writes go quiet.
    fn queue_broadcast(&mut self) {
        self.pending_broadcast_since = Some(Instant::now());
    }

    /// Send the queued peer broadcast once it has been idle for at least
    /// `debounce`. Peers reload the latest value straight from tmux, so
    /// collapsing a burst of changes into one fan-out loses nothing.
    pub fn flush_pending_broadcast(&mut self, debounce: std::time::Duration) {
        let Some(queued_at) = self.pending_broadcast_since else {
            return;
        };
        if queued_at.elapsed() < debounce {
            return;
        }
        self.pending_broadcast_since = None;
        tmux::notify_other_sidebars();
    }

    /// Save filter to tmux global variable.
    /// Only updates `last_saved_filter` on success so that a failed write
    /// does not cause sync to overwrite the user's choice.
    pub fn save_filter(&mut self) {
        if tmux::run_tmux(&[
            "set",
            "-g",
            tmux::SIDEBAR_FILTER,
            self.status_filter.as_str(),
        ])
        .is_some()
        {
            self.last_saved_filter = self.status_filter;
            self.queue_broadcast();
        }
    }

    /// Save repo filter to tmux global variable.
    pub fn save_repo_filter(&mut self) {
        if tmux::run_tmux(&[
            "set",
            "-g",
            tmux::SIDEBAR_REPO_FILTER,
            self.repo_filter.as_str(),
        ])
        .is_some()
        {
            self.last_saved_repo_filter = self.repo_filter.clone();
            self.queue_broadcast();
        }
    }

    /// Load all global state from tmux variables.
    /// Called at startup and on SIGUSR1 (pane focus change).
    pub fn load_from_tmux(&mut self) {
        let opts = tmux::get_all_global_options();
        self.apply_all(&opts);
    }

    /// Apply all global options from tmux (filter, cursor, repo filter).
    pub fn apply_all(&mut self, opts: &HashMap<String, String>) {
        if let Some(filter_str) = opts.get(tmux::SIDEBAR_FILTER) {
            let tmux_filter = StatusFilter::from_label(filter_str);
            if tmux_filter != self.last_saved_filter {
                self.status_filter = tmux_filter;
                self.last_saved_filter = tmux_filter;
            }
        }
        if let Some(repo_str) = opts.get(tmux::SIDEBAR_REPO_FILTER) {
            let tmux_repo = RepoFilter::from_label(repo_str);
            if tmux_repo != self.last_saved_repo_filter {
                self.repo_filter = tmux_repo.clone();
                self.last_saved_repo_filter = tmux_repo;
            }
        }
    }
}
