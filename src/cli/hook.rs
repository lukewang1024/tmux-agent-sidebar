#[cfg(not(target_os = "linux"))]
use std::collections::HashMap;

use crate::event::{AgentEvent, resolve_adapter};

use super::{read_stdin_json, tmux_pane};

mod activity;
mod context;
mod handlers;
mod notifications;

use context::sync_pane_location;
use notifications::notification_settings;

fn pane_owns_process(pane: &str, process_id: u32) -> bool {
    let Ok(pane_pid) = crate::tmux::display_message(pane, "#{pane_pid}")
        .trim()
        .parse::<u32>()
    else {
        return false;
    };
    process_is_descendant_of(process_id, pane_pid)
}

fn follows_parent_chain<F>(mut process_id: u32, ancestor_id: u32, mut parent_of: F) -> bool
where
    F: FnMut(u32) -> Option<u32>,
{
    // A normal hook is only a handful of generations below the pane shell.
    // The bound also protects against corrupt/cyclic process data.
    for _ in 0..256 {
        if process_id == ancestor_id {
            return true;
        }
        let Some(parent) = parent_of(process_id) else {
            return false;
        };
        if parent == 0 || parent == process_id {
            return false;
        }
        process_id = parent;
    }
    false
}

#[cfg(target_os = "linux")]
fn process_is_descendant_of(process_id: u32, ancestor_id: u32) -> bool {
    follows_parent_chain(process_id, ancestor_id, |pid| {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // `comm` is parenthesized and may contain spaces or `)`, so split at
        // the final `) `; the following fields begin with state then PPID.
        let (_, fields) = stat.rsplit_once(") ")?;
        fields.split_whitespace().nth(1)?.parse().ok()
    })
}

#[cfg(not(target_os = "linux"))]
fn process_is_descendant_of(process_id: u32, ancestor_id: u32) -> bool {
    // Portable fallback: one lightweight process-table query. Unlike the old
    // shared ProcessSnapshot scan this does not request or allocate comm/args.
    let output = std::process::Command::new("ps")
        .args(["-eo", "pid=,ppid="])
        .output()
        .ok();
    let Some(output) = output.filter(|output| output.status.success()) else {
        return false;
    };
    let parents: HashMap<u32, u32> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect();
    follows_parent_chain(process_id, ancestor_id, |pid| parents.get(&pid).copied())
}

// ─── hook subcommand ────────────────────────────────────────────────────────

pub(crate) fn cmd_hook(args: &[String]) -> i32 {
    let agent_name = args.first().map(|s| s.as_str()).unwrap_or("");
    let event_name = args.get(1).map(|s| s.as_str()).unwrap_or("");

    if agent_name.is_empty() || event_name.is_empty() {
        return 0;
    }

    let Some(adapter) = resolve_adapter(agent_name) else {
        return 0;
    };

    let pane = tmux_pane();
    // TMUX_PANE can survive in an environment after the coding agent has
    // escaped or been launched outside that pane. Treat it only as a hint:
    // hooks may write pane state only when this helper is actually a
    // descendant of the pane's shell process. Otherwise the sidebar would
    // render a non-navigable ghost entry for an unrelated agent.
    if pane.is_empty() || !pane_owns_process(&pane, std::process::id()) {
        return 0;
    }

    let input = read_stdin_json();
    let Some(event) = adapter.parse(event_name, &input) else {
        return 0;
    };

    let should_notify = !matches!(
        &event,
        AgentEvent::ActivityLog { .. }
            | AgentEvent::TaskCreated { .. }
            | AgentEvent::WorktreeCreate
    );
    let code = handle_event(&pane, agent_name, event);
    crate::shared_snapshot::invalidate();
    // Hook writes happen in a short-lived helper process, while every sidebar
    // TUI has its own event loop. Wake those loops after invalidating the
    // shared snapshot so lifecycle/status changes do not wait for the normal
    // 2s foreground (or 60s background) refresh interval.
    if should_notify {
        crate::tmux::notify_other_sidebars();
    }
    code
}

// ─── event handler ──────────────────────────────────────────────────────────

fn handle_event(pane: &str, agent_name: &str, event: AgentEvent) -> i32 {
    match event {
        AgentEvent::SessionStart {
            agent,
            cwd,
            permission_mode,
            source,
            worktree,
            session_id,
            ..
        } => handlers::on_session_start(
            pane,
            &context::make_ctx(&agent, &cwd, &permission_mode, &worktree, &session_id),
            &source,
        ),
        AgentEvent::SessionEnd { end_reason } => {
            let notifications = notification_settings();
            handlers::on_session_end(pane, agent_name, &end_reason, &notifications)
        }
        AgentEvent::UserPromptSubmit {
            agent,
            cwd,
            permission_mode,
            prompt,
            worktree,
            session_id,
            ..
        } => handlers::on_user_prompt_submit(
            pane,
            &context::make_ctx(&agent, &cwd, &permission_mode, &worktree, &session_id),
            &prompt,
        ),
        AgentEvent::Notification {
            agent,
            cwd,
            permission_mode,
            wait_reason,
            meta_only,
            worktree,
            session_id,
            ..
        } => {
            let notifications = notification_settings();
            handlers::on_notification(
                pane,
                &context::make_ctx(&agent, &cwd, &permission_mode, &worktree, &session_id),
                &wait_reason,
                meta_only,
                &notifications,
            )
        }
        AgentEvent::Stop {
            agent,
            cwd,
            permission_mode,
            last_message,
            transcript_path,
            response,
            worktree,
            session_id,
            ..
        } => {
            let notifications = notification_settings();
            handlers::on_stop(
                pane,
                &context::make_ctx(&agent, &cwd, &permission_mode, &worktree, &session_id),
                &last_message,
                &transcript_path,
                response.as_deref(),
                &notifications,
            )
        }
        AgentEvent::StopFailure {
            agent,
            cwd,
            permission_mode,
            error,
            worktree,
            session_id,
            ..
        } => {
            let notifications = notification_settings();
            handlers::on_stop_failure(
                pane,
                &context::make_ctx(&agent, &cwd, &permission_mode, &worktree, &session_id),
                &error,
                &notifications,
            )
        }
        AgentEvent::SubagentStart {
            agent_type,
            agent_id,
        } => handlers::on_subagent_start(pane, &agent_type, agent_id.as_deref()),
        AgentEvent::SubagentStop { agent_id, .. } => {
            handlers::on_subagent_stop(pane, agent_id.as_deref())
        }
        AgentEvent::ActivityLog {
            tool_name,
            tool_input,
            tool_response,
        } => activity::handle_activity_log(pane, &tool_name, &tool_input, &tool_response),
        AgentEvent::PermissionDenied {
            agent,
            cwd,
            permission_mode,
            worktree,
            session_id,
            ..
        } => {
            let notifications = notification_settings();
            handlers::on_permission_denied(
                pane,
                &context::make_ctx(&agent, &cwd, &permission_mode, &worktree, &session_id),
                &notifications,
            )
        }
        AgentEvent::CwdChanged {
            cwd,
            worktree,
            session_id,
            ..
        } => {
            sync_pane_location(pane, &cwd, &worktree, &session_id);
            0
        }
        AgentEvent::TaskCreated { .. } => 0,
        AgentEvent::TaskCompleted {
            task_id,
            task_subject,
        } => {
            super::set_attention(pane, "notification");
            let notifications = notification_settings();
            handlers::on_task_completed(pane, agent_name, &task_id, &task_subject, &notifications)
        }
        AgentEvent::TeammateIdle {
            teammate_name,
            idle_reason,
            ..
        } => handlers::on_teammate_idle(pane, &teammate_name, &idle_reason),
        AgentEvent::WorktreeCreate => 0,
        AgentEvent::WorktreeRemove { .. } => handlers::on_worktree_remove(pane),
    }
}

#[cfg(test)]
mod tests {
    use super::follows_parent_chain;
    use std::collections::HashMap;

    #[test]
    fn accepts_hook_process_inside_pane_tree() {
        let parents = HashMap::from([(100, 1), (200, 100), (300, 200)]);

        assert!(follows_parent_chain(300, 100, |pid| parents
            .get(&pid)
            .copied()));
    }

    #[test]
    fn rejects_process_outside_pane_tree_even_with_stale_pane_env() {
        let parents = HashMap::from([(100, 1), (200, 100), (400, 1), (500, 400)]);

        assert!(!follows_parent_chain(500, 100, |pid| parents
            .get(&pid)
            .copied()));
    }

    #[test]
    fn rejects_a_corrupt_parent_cycle() {
        let parents = HashMap::from([(200, 300), (300, 200)]);

        assert!(!follows_parent_chain(300, 100, |pid| parents
            .get(&pid)
            .copied()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_proc_walk_finds_the_real_parent() {
        assert!(super::process_is_descendant_of(
            std::process::id(),
            unsafe { libc::getppid() as u32 },
        ));
    }
}
