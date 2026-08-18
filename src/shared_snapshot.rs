//! Shared tmux/process snapshot daemon.
//!
//! Every sidebar is a separate TUI process, but the expensive source data is
//! global to a tmux server. The first client starts a small daemon; subsequent
//! clients read its cached snapshot over a per-user Unix socket. If the daemon
//! cannot be reached, callers simply fall back to the legacy local query path.

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::port::PaneProcessSnapshot;
use crate::process::ProcessSnapshot;
use crate::state::AppState;
use crate::tmux::{self, SessionInfo};

const PROTOCOL_VERSION: u8 = 2;
/// Bump when daemon cache semantics change without a wire-format change.
const DAEMON_CACHE_REVISION: u8 = 1;
const REQUEST_SNAPSHOT: u8 = 1;
const REQUEST_INVALIDATE: u8 = 2;
// Codex has no reliable lifecycle hooks before its first prompt or when its
// TUI exits. Keep these aligned with the visible sidebar's foreground refresh
// cadence so process-only session starts/stops cannot sit in the shared cache
// for tens of seconds.
const TMUX_FALLBACK_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const PROCESS_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const PORT_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const DAEMON_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SnapshotResponse {
    pub(crate) version: u8,
    pub(crate) generation: u64,
    pub(crate) unchanged: bool,
    pub(crate) sessions: Vec<SessionInfo>,
    pub(crate) pane_processes: Option<PaneProcessSnapshot>,
    pub(crate) sidebar_visibility: HashMap<String, SidebarVisibility>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct SidebarVisibility {
    pub(crate) pane_active: bool,
    pub(crate) window_active: bool,
    pub(crate) session_attached: bool,
}

struct SnapshotCache {
    sessions: Vec<SessionInfo>,
    processes: Option<ProcessSnapshot>,
    last_tmux_refresh: Option<Instant>,
    last_process_refresh: Option<Instant>,
    pane_processes: Option<PaneProcessSnapshot>,
    last_port_refresh: Option<Instant>,
    sidebar_visibility: HashMap<String, SidebarVisibility>,
    generation: u64,
    dirty: bool,
}

impl SnapshotCache {
    fn new() -> Self {
        Self {
            sessions: Vec::new(),
            processes: None,
            last_tmux_refresh: None,
            last_process_refresh: None,
            pane_processes: None,
            // Do not put a full-system lsof scan on the first client request;
            // ports are optional decoration and can populate after warm-up.
            last_port_refresh: Some(Instant::now()),
            sidebar_visibility: HashMap::new(),
            generation: 0,
            dirty: true,
        }
    }

    fn invalidate(&mut self) {
        self.dirty = true;
    }

    fn snapshot(&mut self, client_generation: u64) -> SnapshotResponse {
        let fallback_due = self
            .last_tmux_refresh
            .is_none_or(|last| last.elapsed() >= TMUX_FALLBACK_REFRESH_INTERVAL);
        if self.dirty || fallback_due {
            self.refresh();
        }
        let unchanged = client_generation == self.generation;
        SnapshotResponse {
            version: PROTOCOL_VERSION,
            generation: self.generation,
            unchanged,
            sessions: if unchanged {
                Vec::new()
            } else {
                self.sessions.clone()
            },
            pane_processes: (!unchanged).then(|| self.pane_processes.clone()).flatten(),
            sidebar_visibility: self.sidebar_visibility.clone(),
        }
    }

    fn refresh(&mut self) {
        let process_due = self
            .last_process_refresh
            .is_none_or(|last| last.elapsed() >= PROCESS_REFRESH_INTERVAL);
        if process_due {
            let (mut sessions, mut processes) = tmux::query_sessions_with_process_snapshot();
            crate::state::sweep_dead_bg_shells(&mut sessions, &mut processes);
            self.sessions = sessions;
            if processes.is_some() {
                self.processes = processes;
            }
            self.last_process_refresh = Some(Instant::now());
        } else if let Some(processes) = self.processes.as_ref() {
            self.sessions = tmux::query_sessions_with_cached_process_snapshot(processes);
        } else {
            self.sessions = tmux::query_sessions_without_process_snapshot();
        }
        let port_due = self
            .last_port_refresh
            .is_none_or(|last| last.elapsed() >= PORT_REFRESH_INTERVAL);
        if port_due {
            if let Some(scanned) =
                crate::port::scan_session_process_snapshot(&self.sessions, self.processes.as_ref())
            {
                for session in &self.sessions {
                    for window in &session.windows {
                        for pane in &window.panes {
                            if !scanned.live_agent_panes.contains(&pane.pane_id) {
                                AppState::clear_dead_agent_metadata(&pane.pane_id);
                            }
                        }
                    }
                }
                self.sessions = AppState::filter_sessions_to_live_agent_panes(
                    std::mem::take(&mut self.sessions),
                    &scanned.live_agent_panes,
                );
                self.pane_processes = Some(scanned);
            }
            self.last_port_refresh = Some(Instant::now());
        }
        self.last_tmux_refresh = Some(Instant::now());
        self.sidebar_visibility = query_sidebar_visibility();
        self.generation = self.generation.wrapping_add(1);
        self.dirty = false;
    }
}

fn query_sidebar_visibility() -> HashMap<String, SidebarVisibility> {
    let Some(output) = tmux::run_tmux(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id}|#{pane_active}|#{window_active}|#{session_attached}|#{@pane_role}",
    ]) else {
        return HashMap::new();
    };
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('|');
            let pane_id = fields.next()?;
            let pane_active = fields.next()? == "1";
            let window_active = fields.next()? == "1";
            let session_attached = fields.next()? == "1";
            (fields.next()? == "sidebar").then(|| {
                (
                    pane_id.to_string(),
                    SidebarVisibility {
                        pane_active,
                        window_active,
                        session_attached,
                    },
                )
            })
        })
        .collect()
}

struct DaemonGuard {
    socket: PathBuf,
    lock: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.lock);
    }
}

fn server_key() -> u64 {
    let mut hasher = DefaultHasher::new();
    unsafe { libc::geteuid() }.hash(&mut hasher);
    // A newly installed binary must not remain attached to a daemon running
    // an older wire/schema or cache implementation. Include the protocol in
    // the socket identity because local rebuilds can change daemon behavior
    // without changing the package version. The old daemon becomes idle and
    // exits while the new binary gets its own socket.
    crate::VERSION.hash(&mut hasher);
    PROTOCOL_VERSION.hash(&mut hasher);
    DAEMON_CACHE_REVISION.hash(&mut hasher);
    std::env::var("TMUX")
        .unwrap_or_default()
        .split(',')
        .next()
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

fn runtime_path(extension: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "tmux-agent-sidebar-{:016x}.{extension}",
        server_key()
    ))
}

fn daemon_is_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

fn acquire_lock() -> io::Result<Option<(std::fs::File, DaemonGuard)>> {
    let socket = runtime_path("sock");
    let lock = runtime_path("lock");
    for _ in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                return Ok(Some((file, DaemonGuard { socket, lock })));
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let live_owner = fs::read_to_string(&lock)
                    .ok()
                    .and_then(|text| text.trim().parse::<i32>().ok())
                    .is_some_and(daemon_is_alive);
                if live_owner {
                    return Ok(None);
                }
                let _ = fs::remove_file(&lock);
            }
            Err(err) => return Err(err),
        }
    }
    Ok(None)
}

/// Run the singleton snapshot daemon. Invoked by the hidden `daemon` CLI
/// subcommand; normal users never need to start it manually.
pub fn run_daemon() -> i32 {
    match run_daemon_inner() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("tmux-agent-sidebar daemon: {err}");
            1
        }
    }
}

fn run_daemon_inner() -> io::Result<()> {
    let Some((_lock_file, guard)) = acquire_lock()? else {
        return Ok(());
    };
    let _ = fs::remove_file(&guard.socket);
    let listener = UnixListener::bind(&guard.socket)?;
    fs::set_permissions(&guard.socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;

    let mut cache = SnapshotCache::new();
    let mut last_request = Instant::now();
    loop {
        let mut poll_fd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, 1_000) };
        if ready == 0 {
            if last_request.elapsed() >= DAEMON_IDLE_TIMEOUT {
                return Ok(());
            }
            continue;
        }
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                last_request = Instant::now();
                let mut request = [0_u8; 10];
                if stream.read_exact(&mut request).is_ok() && request[0] == PROTOCOL_VERSION {
                    match request[1] {
                        REQUEST_SNAPSHOT => {
                            let generation = u64::from_be_bytes(request[2..10].try_into().unwrap());
                            let response = cache.snapshot(generation);
                            let _ = serde_json::to_writer(&mut stream, &response);
                            let _ = stream.flush();
                        }
                        REQUEST_INVALIDATE => cache.invalidate(),
                        _ => {}
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                continue;
            }
            Err(err) => return Err(err),
        }
    }
}

fn request_once(generation: u64) -> Option<SnapshotResponse> {
    let mut stream = UnixStream::connect(runtime_path("sock")).ok()?;
    stream.set_read_timeout(Some(CLIENT_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT)).ok()?;
    let mut request = [0_u8; 10];
    request[0] = PROTOCOL_VERSION;
    request[1] = REQUEST_SNAPSHOT;
    request[2..].copy_from_slice(&generation.to_be_bytes());
    stream.write_all(&request).ok()?;
    let response: SnapshotResponse = serde_json::from_reader(stream).ok()?;
    (response.version == PROTOCOL_VERSION).then_some(response)
}

fn spawn_daemon() -> Option<()> {
    let executable = std::env::current_exe().ok()?;
    Command::new(executable)
        .arg("daemon")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    Some(())
}

/// Read the shared snapshot, starting the singleton daemon on first use.
/// Returns `None` on any failure so the TUI can use its local fallback.
pub(crate) fn query_sessions(generation: u64) -> Option<SnapshotResponse> {
    if let Some(response) = request_once(generation) {
        return Some(response);
    }
    spawn_daemon()?;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(25));
        if let Some(response) = request_once(generation) {
            return Some(response);
        }
    }
    None
}

struct AsyncClient {
    request_tx: mpsc::SyncSender<u64>,
    response_rx: Mutex<mpsc::Receiver<SnapshotResponse>>,
}

static ASYNC_CLIENT: OnceLock<AsyncClient> = OnceLock::new();

/// Return the latest completed snapshot while a worker performs all daemon
/// and tmux I/O. At most one request is queued, so a slow daemon cannot build
/// an unbounded backlog behind the UI.
pub(crate) fn query_sessions_async(generation: u64) -> Option<SnapshotResponse> {
    let client = ASYNC_CLIENT.get_or_init(|| {
        let (request_tx, request_rx) = mpsc::sync_channel::<u64>(1);
        let (response_tx, response_rx) = mpsc::sync_channel::<SnapshotResponse>(1);
        std::thread::spawn(move || {
            while let Ok(generation) = request_rx.recv() {
                if let Some(response) = query_sessions(generation) {
                    let _ = response_tx.try_send(response);
                }
            }
        });
        AsyncClient {
            request_tx,
            response_rx: Mutex::new(response_rx),
        }
    });

    let response = client
        .response_rx
        .lock()
        .ok()
        .and_then(|rx| rx.try_iter().last());
    let _ = client.request_tx.try_send(generation);
    response
}

/// Mark the daemon's global snapshot dirty after an agent or tmux event.
/// This is deliberately fire-and-forget: hooks must never wait for daemon
/// startup or make agent execution depend on sidebar availability.
pub fn invalidate() {
    let Ok(mut stream) = UnixStream::connect(runtime_path("sock")) else {
        return;
    };
    let mut request = [0_u8; 10];
    request[0] = PROTOCOL_VERSION;
    request[1] = REQUEST_INVALIDATE;
    let _ = stream.write_all(&request);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_round_trips() {
        let response = SnapshotResponse {
            version: PROTOCOL_VERSION,
            generation: 7,
            unchanged: false,
            sessions: vec![],
            pane_processes: None,
            sidebar_visibility: HashMap::new(),
        };
        let encoded = serde_json::to_vec(&response).unwrap();
        let decoded: SnapshotResponse = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.version, PROTOCOL_VERSION);
        assert_eq!(decoded.generation, 7);
        assert!(decoded.sessions.is_empty());
    }

    #[test]
    fn runtime_paths_are_server_specific_and_share_a_stem() {
        let socket = runtime_path("sock");
        let lock = runtime_path("lock");
        assert_eq!(socket.with_extension("lock"), lock);
        assert!(
            socket
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("sidebar")
        );
    }

    #[test]
    fn process_lifecycle_cache_matches_foreground_refresh_cadence() {
        assert_eq!(TMUX_FALLBACK_REFRESH_INTERVAL, Duration::from_secs(2));
        assert_eq!(PROCESS_REFRESH_INTERVAL, Duration::from_secs(2));
    }
}
