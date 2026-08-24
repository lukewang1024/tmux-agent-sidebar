//! Main application orchestration: prime the [`AppState`], spawn background
//! workers, and run the crossterm event loop. Split out from `src/main.rs` so
//! the binary entry point only handles CLI arg parsing, signal wiring, and
//! TUI session setup.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{self};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::SPINNER_PULSE;
use crate::state::BottomTab;

const FOREGROUND_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const BACKGROUND_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const FOREGROUND_SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(16);
const BACKGROUND_SIGNAL_POLL_INTERVAL: Duration = Duration::from_secs(1);
const URGENT_REFRESH_POLL_INTERVAL: Duration = Duration::from_millis(16);
const URGENT_REFRESH_TIMEOUT: Duration = Duration::from_secs(2);

fn refresh_interval(sidebar_visible: bool) -> Duration {
    if sidebar_visible {
        FOREGROUND_REFRESH_INTERVAL
    } else {
        BACKGROUND_REFRESH_INTERVAL
    }
}

fn signal_poll_interval(sidebar_visible: bool) -> Duration {
    if sidebar_visible {
        FOREGROUND_SIGNAL_POLL_INTERVAL
    } else {
        BACKGROUND_SIGNAL_POLL_INTERVAL
    }
}

fn should_handle_input(event: &event::Event, sidebar_focused: bool) -> bool {
    sidebar_focused || matches!(event, event::Event::Resize(_, _))
}

mod input;
mod render;
mod setup;
mod workers;

/// Run the TUI event loop. Returns when the loop exits (currently only on
/// fatal I/O error, since the loop is `loop { ... }`).
///
/// `needs_refresh` is the process-wide SIGUSR1 flag owned by `main.rs` — the
/// signal handler must reference a static visible at signal-handler time,
/// so the static stays with the `extern "C"` handler in the binary crate and
/// we just borrow it here.
pub fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    tmux_pane: String,
    needs_refresh: &'static AtomicBool,
) -> io::Result<()> {
    let mut state = setup::init_state(tmux_pane);
    let mut window_inactive_count: u32 = 0;

    let workers = workers::spawn(&state);
    let workers::Workers {
        git_rx,
        session_rx,
        version_rx,
        git_tab_active,
    } = workers;

    let mut last_refresh = std::time::Instant::now();
    let mut last_spinner = std::time::Instant::now();
    let spinner_interval = Duration::from_millis(200);
    // Start conservatively until the first refresh establishes whether this
    // instance belongs to the window an attached client is actually viewing.
    let mut sidebar_visible = true;
    let mut needs_redraw = true;
    let mut urgent_refresh_deadline: Option<std::time::Instant> = None;
    let mut urgent_refresh_epoch = 0;

    loop {
        if needs_redraw {
            render::render_frame(terminal, &mut state)?;
            needs_redraw = false;
        }

        let refresh_timeout =
            refresh_interval(sidebar_visible).saturating_sub(last_refresh.elapsed());
        let spinner_timeout = if sidebar_visible {
            spinner_interval.saturating_sub(last_spinner.elapsed())
        } else {
            // Hidden instances do not render animation, so the spinner clock
            // must not keep waking their event loops five times per second.
            refresh_timeout
        };
        let urgent_refresh_pending = urgent_refresh_deadline.is_some();
        let timeout = if needs_refresh.load(Ordering::Relaxed) {
            Duration::ZERO
        } else {
            let base = refresh_timeout
                .min(spinner_timeout)
                .min(signal_poll_interval(sidebar_visible));
            if urgent_refresh_pending {
                base.min(URGENT_REFRESH_POLL_INTERVAL)
            } else {
                base
            }
        };
        if event::poll(timeout)? {
            loop {
                let mut handled_input = false;
                match event::read()? {
                    // tmux moved focus off this pane (window/session switch).
                    // Drop the selection highlight immediately so a backgrounded
                    // sidebar doesn't leave a stale "keyboard focus" residual in
                    // its buffer that flashes back on a fast switch — instead of
                    // waiting up to a full refresh tick for session_attached to
                    // catch up.
                    event::Event::FocusLost => {
                        if state.focus_state.sidebar_focused {
                            state.focus_state.sidebar_focused = false;
                            needs_redraw = true;
                        }
                    }
                    // Regained focus: re-derive focus/selection state now.
                    event::Event::FocusGained => {
                        // tmux delivers FocusGained before the input that
                        // selected this pane. Mark it focused immediately so
                        // the following event in the same drain batch is not
                        // discarded while waiting for the refresh pass.
                        state.focus_state.sidebar_focused = true;
                        needs_refresh.store(true, Ordering::Relaxed);
                    }
                    ev => {
                        if should_handle_input(&ev, state.focus_state.sidebar_focused)
                            && input::handle_event(ev, &mut state, &git_tab_active, terminal)
                        {
                            needs_redraw = true;
                            handled_input = true;
                        }
                    }
                }
                // Give each user input a frame before consuming more queued
                // events. This keeps selection feedback immediate and avoids
                // starving rendering behind terminal key-repeat batches.
                if handled_input {
                    break;
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }

        if last_spinner.elapsed() >= spinner_interval {
            state.spinner_frame = (state.spinner_frame + 1) % SPINNER_PULSE.len();
            let has_running_agent = state
                .repo_groups
                .iter()
                .flat_map(|group| group.panes.iter())
                .any(|(pane, _)| pane.status.is_active());
            if sidebar_visible && state.pet_enabled {
                let term_width = terminal.size().map(|s| s.width).unwrap_or(60);
                state.tick_pet(term_width);
            }
            last_spinner = std::time::Instant::now();
            // Hidden panes do not need animation frames written into their
            // PTYs. Static foreground sidebars likewise redraw only when
            // state changes instead of five times per second forever.
            if sidebar_visible && (state.pet_enabled || has_running_agent) {
                needs_redraw = true;
            }
        }

        let sigusr1 = needs_refresh.swap(false, Ordering::Relaxed);
        if sigusr1 {
            urgent_refresh_deadline = Some(std::time::Instant::now() + URGENT_REFRESH_TIMEOUT);
            urgent_refresh_epoch = crate::shared_snapshot::next_async_refresh_epoch();
        }
        let urgent_refresh_pending =
            urgent_refresh_deadline.is_some_and(|deadline| std::time::Instant::now() < deadline);
        if urgent_refresh_deadline.is_some() && !urgent_refresh_pending {
            urgent_refresh_deadline = None;
        }
        if sigusr1
            || urgent_refresh_pending
            || last_refresh.elapsed() >= refresh_interval(sidebar_visible)
        {
            let previous_focused_pane_id = state.focus_state.focused_pane_id.clone();
            let required_epoch = if urgent_refresh_pending {
                urgent_refresh_epoch
            } else {
                0
            };
            let (new_sidebar_visible, snapshot_ready) = state.refresh(required_epoch);
            sidebar_visible = new_sidebar_visible;
            if snapshot_ready {
                urgent_refresh_deadline = None;
            }
            // A SIGUSR1 poke is either a focus hook or a peer broadcasting a
            // shared-state change (status/repo filter, selection cursor). Reload
            // globally-shared options now so the change lands immediately, even
            // on a hidden/background instance — which otherwise only reloads
            // when its own window next becomes active.
            if sigusr1 {
                state.global.load_from_tmux();
                state.rebuild_row_targets();
            }
            if snapshot_ready && state.focus_state.focused_pane_id != previous_focused_pane_id {
                render::refresh_git_for_focused_pane(&mut state);
            }
            needs_redraw = true;
            if snapshot_ready && sidebar_visible {
                if window_inactive_count >= 2 {
                    state.global.load_from_tmux();
                    state.rebuild_row_targets();
                }
                window_inactive_count = 0;
            } else if snapshot_ready {
                window_inactive_count = window_inactive_count.saturating_add(1);
            }
            git_tab_active.store(state.bottom_tab == BottomTab::GitStatus, Ordering::Relaxed);
            if snapshot_ready {
                last_refresh = std::time::Instant::now();
            }
        }

        if let Ok(data) = git_rx.try_recv() {
            state.apply_git_data(data);
            needs_redraw = true;
        }

        if let Ok(names) = session_rx.try_recv() {
            state.sessions.names = names;
            state.sessions.dirty = true;
            needs_redraw = true;
        }

        if let Ok(notice) = version_rx.try_recv() {
            state.version_notice = Some(notice);
            needs_redraw = true;
        }

        state
            .global
            .flush_pending_broadcast(std::time::Duration::from_millis(150));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_sidebars_poll_and_refresh_less_often() {
        assert_eq!(refresh_interval(true), Duration::from_secs(2));
        assert_eq!(refresh_interval(false), Duration::from_secs(60));
        assert_eq!(signal_poll_interval(true), Duration::from_millis(16));
        assert_eq!(signal_poll_interval(false), Duration::from_secs(1));
    }

    #[test]
    fn unfocused_sidebars_ignore_input_but_still_handle_resize() {
        assert!(!should_handle_input(
            &event::Event::Key(event::KeyEvent::new(
                event::KeyCode::Tab,
                event::KeyModifiers::NONE,
            )),
            false,
        ));
        assert!(!should_handle_input(
            &event::Event::Mouse(event::MouseEvent {
                kind: event::MouseEventKind::Down(event::MouseButton::Left),
                column: 1,
                row: 0,
                modifiers: event::KeyModifiers::NONE,
            }),
            false,
        ));
        assert!(should_handle_input(&event::Event::Resize(80, 24), false));
        assert!(should_handle_input(
            &event::Event::Key(event::KeyEvent::new(
                event::KeyCode::Tab,
                event::KeyModifiers::NONE,
            )),
            true,
        ));
    }
}
