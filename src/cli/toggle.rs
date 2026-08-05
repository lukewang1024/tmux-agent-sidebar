use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use crate::tmux;

pub(crate) fn cmd_toggle(args: &[String]) -> i32 {
    let mut create_only = false;
    let mut positional = Vec::new();

    for arg in args {
        if arg == "--create-only" {
            create_only = true;
        } else {
            positional.push(arg.as_str());
        }
    }

    let window_id = match positional.first() {
        Some(id) => *id,
        None => return 0,
    };
    let pane_path = positional.get(1).copied().unwrap_or("~");

    // `client-resized` and `after-resize-pane` hooks run in the background and
    // may reach this check/create sequence concurrently. Serialize it per tmux
    // window so two processes cannot both observe "no sidebar" and split one.
    let _create_lock = match SidebarCreateLock::acquire(window_id) {
        Some(lock) => lock,
        None => return 0,
    };

    // Optional extra auto-create width gate. The built-in sidebar + main-area
    // fit check below applies to every creation path; this legacy option lets
    // users impose an even larger threshold on programmatic creation only.
    if create_only {
        let min_width: u32 = tmux::display_message(
            window_id,
            &format!("#{{{}}}", tmux::SIDEBAR_AUTO_CREATE_MIN_WIDTH),
        )
        .trim()
        .parse()
        .unwrap_or(0);
        if min_width > 0 {
            let window_width: u32 = tmux::display_message(window_id, "#{window_width}")
                .trim()
                .parse()
                .unwrap_or(0);
            if auto_create_blocked_by_width(min_width, window_width) {
                return 0;
            }
        }
    }

    // Check sidebar width setting
    let sidebar_width_setting = {
        let s = tmux::display_message(window_id, &format!("#{{{}}}", tmux::SIDEBAR_WIDTH));
        if s.is_empty() { "48".to_string() } else { s }
    };

    let window_width = tmux::display_message(window_id, "#{window_width}")
        .parse()
        .unwrap_or(0);
    let resolved_sidebar_width = resolve_sidebar_width(&sidebar_width_setting, window_width);
    let sidebar_width = resolved_sidebar_width
        .map(|width| width.to_string())
        .unwrap_or(sidebar_width_setting);

    let sidebar_position = SidebarPosition::from_setting(&tmux::display_message(
        window_id,
        &format!("#{{{}}}", tmux::SIDEBAR_POSITION),
    ));

    // Check for existing sidebar
    let pane_id_role_format = pane_id_role_format();
    let panes_output = tmux::run_tmux(&["list-panes", "-t", window_id, "-F", &pane_id_role_format])
        .unwrap_or_default();

    let existing_sidebars = sidebar_panes(&panes_output);

    if let Some((sidebar_pane, duplicate_sidebars)) = existing_sidebars.split_first() {
        // Heal panes left behind by an older racing implementation. A
        // create-only request preserves one; a manual toggle removes all.
        for duplicate in duplicate_sidebars {
            let _ = tmux::run_tmux(&["kill-pane", "-t", duplicate]);
        }
        if create_only {
            return 0;
        }
        clear_auto_hidden_marker(window_id);
        let _ = tmux::run_tmux(&["kill-pane", "-t", sidebar_pane]);
        return 0;
    }

    // Apply the responsive rule before splitting so a narrow new window never
    // flashes an unusable sidebar and immediately removes it. The auto-hidden
    // marker lets a later client resize restore it at the same fixed width.
    let main_min_width =
        tmux::display_message(window_id, &format!("#{{{}}}", tmux::SIDEBAR_MAIN_MIN_WIDTH))
            .trim()
            .parse::<u32>()
            .unwrap_or(80);
    if resolved_sidebar_width
        .is_some_and(|width| !responsive_sidebar_fits(window_width, width, main_min_width))
    {
        if create_only {
            let _ = tmux::run_tmux(&["set", "-w", "-t", window_id, tmux::SIDEBAR_AUTO_HIDDEN, "1"]);
            let _ = tmux::run_tmux(&[
                "set",
                "-w",
                "-t",
                window_id,
                tmux::SIDEBAR_AUTO_HIDDEN_PATH,
                pane_path,
            ]);
        }
        return 0;
    }

    let pane_geometry_output = tmux::run_tmux(&[
        "list-panes",
        "-t",
        window_id,
        "-F",
        "#{pane_left} #{pane_width} #{pane_id}",
    ])
    .unwrap_or_default();

    let target_pane = target_pane_for_position(&pane_geometry_output, sidebar_position)
        .unwrap_or_else(|| window_id.to_string());
    let split_flags = split_window_flags(sidebar_position);

    // Remember active pane
    let active_pane = tmux::display_message(window_id, "#{pane_id}");

    // Find our own binary path
    let self_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "tmux-agent-sidebar".to_string());

    // Create sidebar pane
    let sidebar_pane = tmux::run_tmux(&[
        "split-window",
        split_flags,
        "-l",
        &sidebar_width,
        "-t",
        &target_pane,
        "-c",
        pane_path,
        "-P",
        "-F",
        "#{pane_id}",
        &self_bin,
    ])
    .map(|s| s.trim().to_string())
    .unwrap_or_default();

    if !sidebar_pane.is_empty() {
        tmux::set_pane_option(&sidebar_pane, tmux::PANE_ROLE, "sidebar");
        clear_auto_hidden_marker(window_id);
    }

    // Restore focus
    if !active_pane.is_empty() {
        let _ = tmux::run_tmux(&["select-pane", "-t", &active_pane]);
    } else {
        let _ = tmux::run_tmux(&["select-pane", "-t", window_id, "-l"]);
    }

    0
}

/// Advisory process lock for the non-atomic tmux "list panes, then split"
/// sequence. `flock` is released by the kernel even if a hook process crashes;
/// the small persistent file is only the lock's rendezvous point.
struct SidebarCreateLock {
    file: File,
}

impl SidebarCreateLock {
    fn acquire(window_id: &str) -> Option<Self> {
        let path = sidebar_create_lock_path(window_id);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .ok()?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        (result == 0).then_some(Self { file })
    }
}

impl Drop for SidebarCreateLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn sidebar_create_lock_path(window_id: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    std::env::var("TMUX").unwrap_or_default().hash(&mut hasher);
    window_id.hash(&mut hasher);
    std::env::temp_dir().join(format!(
        "tmux-agent-sidebar-create-{:016x}.lock",
        hasher.finish()
    ))
}

pub(crate) fn cmd_toggle_all(_args: &[String]) -> i32 {
    let pane_id_role_format = pane_id_role_format();
    let has_sidebar = tmux::run_tmux(&["list-panes", "-a", "-F", &pane_id_role_format])
        .map(|output| any_sidebar_pane(&output))
        .unwrap_or(false);

    if has_sidebar {
        let all_panes =
            tmux::run_tmux(&["list-panes", "-a", "-F", &pane_id_role_format]).unwrap_or_default();
        for line in all_panes.lines() {
            let parts: Vec<&str> = line.splitn(2, '|').collect();
            if parts.len() >= 2 && parts[1] == "sidebar" {
                let _ = tmux::run_tmux(&["kill-pane", "-t", parts[0]]);
            }
        }
    } else {
        let all_windows = tmux::run_tmux(&[
            "list-panes",
            "-a",
            "-F",
            "#{window_id}|#{pane_current_path}",
        ])
        .unwrap_or_default();
        for (window_id, pane_path) in unique_window_paths(&all_windows) {
            let args = vec!["--create-only".to_string(), window_id, pane_path];
            cmd_toggle(&args);
        }
    }

    0
}

/// Restore the configured sidebar width after tmux has squeezed the layout
/// during a client resize. The configured width is reapplied in both
/// directions so tmux cannot proportionally squeeze or expand the sidebar.
pub(crate) fn cmd_maintain_width(args: &[String]) -> i32 {
    let Some(window_id) = args.first().map(String::as_str) else {
        return 0;
    };

    let window_width = tmux::display_message(window_id, "#{window_width}")
        .trim()
        .parse()
        .unwrap_or(0);
    let setting = tmux::display_message(window_id, &format!("#{{{}}}", tmux::SIDEBAR_WIDTH));
    let Some(preferred_width) = resolve_sidebar_width(&setting, window_width) else {
        return 0;
    };
    let min_width = sidebar_bound(window_id, tmux::SIDEBAR_MIN_WIDTH, 36, window_width);
    let max_width =
        sidebar_bound(window_id, tmux::SIDEBAR_MAX_WIDTH, 64, window_width).max(min_width);
    let preferred_width = preferred_width.clamp(min_width, max_width);
    let main_min_width =
        tmux::display_message(window_id, &format!("#{{{}}}", tmux::SIDEBAR_MAIN_MIN_WIDTH))
            .trim()
            .parse::<u32>()
            .unwrap_or(80);

    let pane_format = format!("#{{pane_id}}|#{{pane_width}}|#{{{}}}", tmux::PANE_ROLE);
    let panes =
        tmux::run_tmux(&["list-panes", "-t", window_id, "-F", &pane_format]).unwrap_or_default();

    let sidebar = panes.lines().find_map(|line| {
        let mut fields = line.splitn(3, '|');
        let (Some(pane_id), Some(current_width), Some(role)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return None;
        };
        (role == "sidebar").then(|| (pane_id.to_string(), current_width.to_string()))
    });

    // The remembered width is indivisible: either it and the minimum main
    // area both fit, or the sidebar stays hidden. Never squeeze the sidebar to
    // an intermediate width while the terminal is changing size.
    if !responsive_sidebar_fits(window_width, preferred_width, main_min_width) {
        if let Some((pane_id, _)) = sidebar {
            let path = tmux::display_message(&pane_id, "#{pane_current_path}");
            let _ = tmux::run_tmux(&["set", "-w", "-t", window_id, tmux::SIDEBAR_AUTO_HIDDEN, "1"]);
            if !path.is_empty() {
                let _ = tmux::run_tmux(&[
                    "set",
                    "-w",
                    "-t",
                    window_id,
                    tmux::SIDEBAR_AUTO_HIDDEN_PATH,
                    &path,
                ]);
            }
            let _ = tmux::run_tmux(&["kill-pane", "-t", &pane_id]);
        }
        return 0;
    }

    if let Some((pane_id, current_width)) = sidebar {
        if sidebar_width_changed(&current_width, preferred_width) {
            let preferred_width = preferred_width.to_string();
            let _ = tmux::run_tmux(&[
                "set",
                "-w",
                "-t",
                window_id,
                tmux::SIDEBAR_ADJUSTING_WIDTH,
                &preferred_width,
            ]);
            let _ = tmux::run_tmux(&["resize-pane", "-t", &pane_id, "-x", &preferred_width]);
        }
        return 0;
    }

    let auto_hidden =
        tmux::display_message(window_id, &format!("#{{{}}}", tmux::SIDEBAR_AUTO_HIDDEN));
    if auto_hidden == "1" {
        let path = tmux::display_message(
            window_id,
            &format!("#{{{}}}", tmux::SIDEBAR_AUTO_HIDDEN_PATH),
        );
        let toggle_args = vec![
            "--create-only".to_string(),
            window_id.to_string(),
            if path.is_empty() {
                "~".to_string()
            } else {
                path
            },
        ];
        cmd_toggle(&toggle_args);
        return cmd_maintain_width(args);
    }

    0
}

/// Persist a mouse/key-resized sidebar width at window scope. New windows keep
/// the global default, while this window retains the user's chosen width across
/// terminal resizes and responsive hide/show cycles.
pub(crate) fn cmd_remember_width(args: &[String]) -> i32 {
    let (Some(window_id), Some(pane_id)) = (args.first(), args.get(1)) else {
        return 0;
    };
    if tmux::display_message(pane_id, &format!("#{{{}}}", tmux::PANE_ROLE)) != "sidebar" {
        return 0;
    }
    let automatic_width = tmux::display_message(
        window_id,
        &format!("#{{{}}}", tmux::SIDEBAR_ADJUSTING_WIDTH),
    );
    let observed_width = tmux::display_message(pane_id, "#{pane_width}");
    if !automatic_width.is_empty() {
        let _ = tmux::run_tmux(&["set", "-wu", "-t", window_id, tmux::SIDEBAR_ADJUSTING_WIDTH]);
        if automatic_width == observed_width {
            return 0;
        }
    }
    let window_width = tmux::display_message(window_id, "#{window_width}")
        .trim()
        .parse()
        .unwrap_or(0);
    let min_width = sidebar_bound(window_id, tmux::SIDEBAR_MIN_WIDTH, 36, window_width);
    let max_width =
        sidebar_bound(window_id, tmux::SIDEBAR_MAX_WIDTH, 64, window_width).max(min_width);
    let max_percent = tmux::display_message(
        window_id,
        &format!("#{{{}}}", tmux::SIDEBAR_MAX_WIDTH_PERCENT),
    )
    .trim()
    .parse::<u32>()
    .ok()
    .filter(|percent| *percent > 0)
    .unwrap_or(30);
    let main_min_width =
        tmux::display_message(window_id, &format!("#{{{}}}", tmux::SIDEBAR_MAIN_MIN_WIDTH))
            .trim()
            .parse::<u32>()
            .unwrap_or(80);
    if let Ok(observed_width) = observed_width.parse::<u32>() {
        // Reject out-of-range observations instead of clamping and issuing a
        // second resize, which visibly made the pane jump twice during drag.
        if drag_width_range(
            window_width,
            min_width,
            max_width,
            max_percent,
            main_min_width,
        )
        .is_some_and(|range| range.contains(&observed_width))
        {
            let width = observed_width.to_string();
            let _ = tmux::run_tmux(&["set", "-w", "-t", window_id, tmux::SIDEBAR_WIDTH, &width]);
        }
    }
    0
}

fn sidebar_bound(window_id: &str, option: &str, fallback: u32, window_width: u32) -> u32 {
    resolve_sidebar_width(
        tmux::display_message(window_id, &format!("#{{{option}}}")).trim(),
        window_width,
    )
    .unwrap_or(fallback)
}

fn clear_auto_hidden_marker(window_id: &str) {
    let _ = tmux::run_tmux(&["set", "-wu", "-t", window_id, tmux::SIDEBAR_AUTO_HIDDEN]);
    let _ = tmux::run_tmux(&[
        "set",
        "-wu",
        "-t",
        window_id,
        tmux::SIDEBAR_AUTO_HIDDEN_PATH,
    ]);
}

fn any_sidebar_pane(output: &str) -> bool {
    !sidebar_panes(output).is_empty()
}

fn sidebar_panes(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let (pane_id, role) = line.split_once('|')?;
            (role == "sidebar").then(|| pane_id.to_string())
        })
        .collect()
}

fn unique_window_paths(output: &str) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    let mut windows = Vec::new();

    for line in output.lines() {
        let Some((window_id, pane_path)) = line.split_once('|') else {
            continue;
        };
        if seen.insert(window_id.to_string()) {
            windows.push((window_id.to_string(), pane_path.to_string()));
        }
    }

    windows
}

/// Which side of the window the sidebar pane is created on, driven by
/// the `@sidebar_position` tmux option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SidebarPosition {
    Left,
    Right,
}

impl SidebarPosition {
    /// Parse the raw `@sidebar_position` option value. Only an explicit
    /// (case-insensitive, whitespace-tolerant) `right` selects the right
    /// side; everything else — including unset, empty, or invalid values
    /// — falls back to the historical default of `left`, so a typo never
    /// moves the sidebar somewhere unexpected.
    fn from_setting(setting: &str) -> Self {
        if setting.trim().eq_ignore_ascii_case("right") {
            Self::Right
        } else {
            Self::Left
        }
    }
}

/// Horizontal placement of one pane, parsed from a
/// `#{pane_left} #{pane_width} #{pane_id}` formatted `list-panes` line.
#[derive(Debug, Eq, PartialEq)]
struct PaneGeometry {
    left: u32,
    width: u32,
    pane_id: String,
}

/// Parse a single `list-panes` output line into a [`PaneGeometry`].
/// Returns `None` for malformed lines so callers can simply skip them.
fn parse_pane_geometry(line: &str) -> Option<PaneGeometry> {
    let mut parts = line.split_whitespace();
    let left = parts.next()?.parse().ok()?;
    let width = parts.next()?.parse().ok()?;
    let pane_id = parts.next()?.to_string();
    Some(PaneGeometry {
        left,
        width,
        pane_id,
    })
}

/// Pick the pane the sidebar splits from: the leftmost pane for a left
/// sidebar, or the pane with the largest right edge (`left + width`) for
/// a right sidebar, so the new pane always lands at the window's outer
/// edge. Returns `None` when no line of `output` parses as geometry.
fn target_pane_for_position(output: &str, position: SidebarPosition) -> Option<String> {
    let panes = output.lines().filter_map(parse_pane_geometry);
    match position {
        SidebarPosition::Left => panes.min_by_key(|pane| pane.left),
        SidebarPosition::Right => panes.max_by_key(|pane| pane.left.saturating_add(pane.width)),
    }
    .map(|pane| pane.pane_id)
}

/// `split-window` flags for each placement: `-hfb` inserts the new pane
/// before the target (left of it), `-hf` after it (right of it). Both
/// `f` variants span the full window height.
fn split_window_flags(position: SidebarPosition) -> &'static str {
    match position {
        SidebarPosition::Left => "-hfb",
        SidebarPosition::Right => "-hf",
    }
}

/// Decide whether `cmd_auto_close` should kill the window, given the raw
/// outputs of the tmux queries it performs. Extracted as a pure function
/// so the guard logic is directly unit-testable without a running tmux
/// server.
///
/// - `list_panes_output`: `Some(stdout)` from `list-panes -F <pane role format>`,
///   or `None` if the tmux call failed.
/// - `session_windows`: parsed value of `#{session_windows}`, or `None`
///   if the tmux call failed or the value was unparseable.
/// - `session_attached`: parsed value of `#{session_attached}`, or `None`
///   if the tmux call failed or the value was unparseable.
fn should_kill_window(
    list_panes_output: Option<&str>,
    session_windows: Option<u32>,
    session_attached: Option<u32>,
) -> bool {
    // `list-panes` failed or returned nothing: the window is either gone
    // already or tmux is too busy to answer. Do NOT treat "no output"
    // as "no non-sidebar panes" — that would let us kill a live window
    // whose query happened to race with another tmux command.
    let Some(output) = list_panes_output else {
        return false;
    };
    if output.trim().is_empty() {
        return false;
    }

    let non_sidebar = output.lines().filter(|line| *line != "sidebar").count();
    if non_sidebar != 0 {
        return false;
    }

    let Some(windows) = session_windows else {
        return false;
    };

    // Last window in the session: killing it destroys the session and
    // drops every attached client. One attached client is fine — that
    // matches normal tmux `exit` behaviour on the last pane. Two or
    // more means a shared session (e.g. several terminal tabs attached
    // to `main`) where we cannot tell which clients are "wanted", so
    // preserve the sidebar instead. A missing `session_attached` errs
    // on the side of preservation.
    match windows {
        0 => false,
        1 => matches!(session_attached, Some(n) if n <= 1),
        _ => true,
    }
}

pub(crate) fn cmd_auto_close(args: &[String]) -> i32 {
    let window_id = match args.first() {
        Some(id) => id.as_str(),
        None => return 0,
    };

    let pane_role_format = format!("#{{{}}}", tmux::PANE_ROLE);
    let list_panes_output =
        tmux::run_tmux(&["list-panes", "-t", window_id, "-F", &pane_role_format]);

    let session_windows = tmux::run_tmux(&[
        "display-message",
        "-t",
        window_id,
        "-p",
        "#{session_windows}",
    ])
    .and_then(|s| s.trim().parse().ok());

    let session_attached = tmux::run_tmux(&[
        "display-message",
        "-t",
        window_id,
        "-p",
        "#{session_attached}",
    ])
    .and_then(|s| s.trim().parse().ok());

    if should_kill_window(
        list_panes_output.as_deref(),
        session_windows,
        session_attached,
    ) {
        let _ = tmux::run_tmux(&["kill-window", "-t", window_id]);
    }

    0
}

fn pane_id_role_format() -> String {
    format!("#{{pane_id}}|#{{{}}}", tmux::PANE_ROLE)
}

/// Whether an auto-create should be skipped because the window is narrower
/// than the configured minimum. The caller only invokes this with
/// `min_width > 0`. A `window_width` of 0 means the query failed / is unknown;
/// we do NOT block on that, since a false skip would silently drop the sidebar
/// on a perfectly normal window — better to create than to mysteriously not.
fn auto_create_blocked_by_width(min_width: u32, window_width: u32) -> bool {
    window_width != 0 && window_width < min_width
}

fn resolve_sidebar_width(setting: &str, window_width: u32) -> Option<u32> {
    let setting = setting.trim();
    if let Some(percent) = setting.strip_suffix('%') {
        let percent: u32 = percent.parse().ok()?;
        if percent == 0 || window_width == 0 {
            return None;
        }
        return Some((window_width.saturating_mul(percent) / 100).max(1));
    }

    setting.parse::<u32>().ok().filter(|width| *width > 0)
}

fn sidebar_width_changed(current_width: &str, desired_width: u32) -> bool {
    current_width
        .trim()
        .parse::<u32>()
        .is_ok_and(|current| current != desired_width)
}

fn responsive_sidebar_fits(window_width: u32, sidebar_width: u32, main_min_width: u32) -> bool {
    window_width
        >= sidebar_width
            .saturating_add(main_min_width)
            .saturating_add(1)
}

fn drag_width_range(
    window_width: u32,
    min_width: u32,
    max_width: u32,
    max_percent: u32,
    main_min_width: u32,
) -> Option<std::ops::RangeInclusive<u32>> {
    let percent_cap = window_width.saturating_mul(max_percent) / 100;
    let main_area_cap = window_width.saturating_sub(main_min_width.saturating_add(1));
    let effective_max = max_width.min(percent_cap).min(main_area_cap);
    (effective_max >= min_width).then_some(min_width..=effective_max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_create_width_gate() {
        // Below the minimum → skip.
        assert!(auto_create_blocked_by_width(149, 100));
        assert!(auto_create_blocked_by_width(149, 148));
        // At or above the minimum → allow.
        assert!(!auto_create_blocked_by_width(149, 149));
        assert!(!auto_create_blocked_by_width(149, 320));
        // Unknown width (query failed → 0) → do not block.
        assert!(!auto_create_blocked_by_width(149, 0));
    }

    #[test]
    fn sidebar_width_resolves_columns_and_percentages() {
        assert_eq!(resolve_sidebar_width("48", 160), Some(48));
        assert_eq!(resolve_sidebar_width("15%", 200), Some(30));
        assert_eq!(resolve_sidebar_width("1%", 40), Some(1));
        assert_eq!(resolve_sidebar_width("0%", 200), None);
        assert_eq!(resolve_sidebar_width("15%", 0), None);
        assert_eq!(resolve_sidebar_width("invalid", 200), None);
    }

    #[test]
    fn sidebar_width_is_restored_after_shrink_or_growth() {
        assert!(sidebar_width_changed("12", 48));
        assert!(!sidebar_width_changed("48", 48));
        assert!(sidebar_width_changed("60", 48));
        assert!(!sidebar_width_changed("invalid", 48));
    }

    #[test]
    fn responsive_sidebar_reserves_main_content_width_and_border() {
        assert!(!responsive_sidebar_fits(148, 48, 100));
        assert!(responsive_sidebar_fits(149, 48, 100));
        assert!(responsive_sidebar_fits(200, 48, 100));
    }

    #[test]
    fn drag_width_range_combines_absolute_percentage_and_main_area_caps() {
        assert_eq!(drag_width_range(110, 36, 64, 30, 80), None);
        assert_eq!(drag_width_range(120, 36, 64, 30, 80), Some(36..=36));
        assert_eq!(drag_width_range(140, 36, 64, 30, 80), Some(36..=42));
        assert_eq!(drag_width_range(160, 36, 64, 30, 80), Some(36..=48));
        assert_eq!(drag_width_range(200, 36, 64, 30, 80), Some(36..=60));
        assert_eq!(drag_width_range(240, 36, 64, 30, 80), Some(36..=64));
        assert_eq!(drag_width_range(320, 36, 64, 30, 80), Some(36..=64));
    }

    #[test]
    fn any_sidebar_pane_detects_sidebar_anywhere() {
        let output = "%1|pane\n%2|sidebar\n%3|pane";
        assert!(any_sidebar_pane(output));
    }

    #[test]
    fn any_sidebar_pane_returns_false_without_sidebar() {
        let output = "%1|pane\n%2|main";
        assert!(!any_sidebar_pane(output));
    }

    #[test]
    fn sidebar_panes_returns_every_duplicate_for_reconciliation() {
        let output = "%1|sidebar\n%2|main\n%3|sidebar\nmalformed";
        assert_eq!(
            sidebar_panes(output),
            vec!["%1".to_string(), "%3".to_string()]
        );
    }

    #[test]
    fn unique_window_paths_deduplicates_windows_and_keeps_spaces() {
        let output = "%1|/Users/me/My Project\n%1|/Users/me/My Project\n%2|/tmp/another project";
        assert_eq!(
            unique_window_paths(output),
            vec![
                ("%1".to_string(), "/Users/me/My Project".to_string()),
                ("%2".to_string(), "/tmp/another project".to_string()),
            ]
        );
    }

    #[test]
    fn unique_window_paths_skips_malformed_lines() {
        let output = "bad-line\n%1|/tmp";
        assert_eq!(
            unique_window_paths(output),
            vec![("%1".to_string(), "/tmp".to_string())]
        );
    }

    // ─── sidebar placement ───────────────────────────────────────────

    #[test]
    fn sidebar_position_parses_right_only() {
        assert_eq!(
            SidebarPosition::from_setting("right"),
            SidebarPosition::Right
        );
        assert_eq!(
            SidebarPosition::from_setting(" RIGHT "),
            SidebarPosition::Right
        );
        assert_eq!(SidebarPosition::from_setting("left"), SidebarPosition::Left);
        assert_eq!(SidebarPosition::from_setting(""), SidebarPosition::Left);
        assert_eq!(
            SidebarPosition::from_setting("invalid"),
            SidebarPosition::Left
        );
    }

    #[test]
    fn target_pane_for_left_position_uses_leftmost_pane() {
        let output = "40 80 %3\n0 20 %1\n20 20 %2";

        assert_eq!(
            target_pane_for_position(output, SidebarPosition::Left),
            Some("%1".to_string())
        );
    }

    #[test]
    fn target_pane_for_right_position_uses_largest_right_edge() {
        let output = "0 20 %1\n20 20 %2\n40 80 %3";

        assert_eq!(
            target_pane_for_position(output, SidebarPosition::Right),
            Some("%3".to_string())
        );
    }

    #[test]
    fn target_pane_for_position_skips_malformed_lines() {
        let output = "bad-line\n0 nope %1\n12 30 %2";

        assert_eq!(
            target_pane_for_position(output, SidebarPosition::Left),
            Some("%2".to_string())
        );
        assert_eq!(target_pane_for_position("", SidebarPosition::Right), None);
    }

    #[test]
    fn split_window_flags_match_tmux_side_semantics() {
        assert_eq!(split_window_flags(SidebarPosition::Left), "-hfb");
        assert_eq!(split_window_flags(SidebarPosition::Right), "-hf");
    }

    // ─── should_kill_window ───────────────────────────────────────────

    #[test]
    fn should_kill_window_kills_when_only_sidebar_and_other_windows_exist() {
        // Classic intended path: sidebar alone in a window, session has
        // other windows to fall back on. Attached-client count is
        // irrelevant because killing this window does not end the
        // session.
        assert!(should_kill_window(Some("sidebar"), Some(2), None));
        assert!(should_kill_window(Some("sidebar"), Some(2), Some(0)));
        assert!(should_kill_window(Some("sidebar"), Some(2), Some(5)));
    }

    #[test]
    fn should_kill_window_skips_when_non_sidebar_pane_remains() {
        // Another pane with `@pane_role` explicitly set to something
        // non-sidebar (e.g. a spawn-marked pane) keeps the window alive.
        assert!(!should_kill_window(Some("sidebar\npane"), Some(5), Some(1)));
        // `@pane_role` unset renders as an empty line — that pane is
        // a regular user pane, not a sidebar, so the window must stay.
        // The real tmux output for [sidebar pane, regular pane] is
        // "sidebar\n\n" (sidebar's role, then the regular pane's empty
        // role followed by the final record separator).
        assert!(!should_kill_window(Some("sidebar\n\n"), Some(5), Some(1)));
        assert!(!should_kill_window(Some("\nsidebar\n"), Some(5), Some(1)));
    }

    #[test]
    fn should_kill_window_skips_when_list_panes_failed() {
        // `list-panes` failure must never be treated as "window is empty" —
        // that used to let a busy-tmux race kill a live window.
        assert!(!should_kill_window(None, Some(5), Some(1)));
    }

    #[test]
    fn should_kill_window_skips_when_list_panes_empty() {
        // Whitespace-only output (e.g. window already gone) must not
        // trigger a kill either.
        assert!(!should_kill_window(Some(""), Some(5), Some(1)));
        assert!(!should_kill_window(Some("   \n"), Some(5), Some(1)));
    }

    #[test]
    fn should_kill_window_kills_last_window_when_single_client_attached() {
        // One client attached to a single-window session: destroying
        // the session only detaches the same client that just kept the
        // session alive, which matches tmux's standard `exit` behaviour
        // on the last pane — the user expects the sidebar to go with it.
        assert!(should_kill_window(Some("sidebar"), Some(1), Some(1)));
    }

    #[test]
    fn should_kill_window_kills_last_window_when_detached() {
        // No clients attached: killing the session harms no one, and
        // a stranded sidebar in a detached session is pointless anyway.
        assert!(should_kill_window(Some("sidebar"), Some(1), Some(0)));
    }

    #[test]
    fn should_kill_window_preserves_last_window_when_multiple_clients_attached() {
        // Core regression guard (0dc6e99): killing the last window of
        // a session drops every attached client. With multiple terminal
        // tabs sharing a single `main` session, that manifested as every
        // tab dying at once. Keep the sidebar stranded rather than nuke
        // the session.
        assert!(!should_kill_window(Some("sidebar"), Some(1), Some(2)));
        assert!(!should_kill_window(Some("sidebar"), Some(1), Some(7)));
    }

    #[test]
    fn should_kill_window_preserves_last_window_when_attached_query_failed() {
        // Without knowing how many clients are attached we cannot prove
        // the kill is safe. Better a lingering sidebar pane than a
        // mass-disconnect.
        assert!(!should_kill_window(Some("sidebar"), Some(1), None));
    }

    #[test]
    fn should_kill_window_skips_when_session_windows_query_failed() {
        // If we cannot prove the session has other windows, err on the
        // side of preservation. Better to leave a lingering sidebar
        // pane than to destroy a live workspace.
        assert!(!should_kill_window(Some("sidebar"), None, Some(1)));
        assert!(!should_kill_window(Some("sidebar"), Some(0), Some(1)));
    }
}
