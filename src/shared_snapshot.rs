//! Shared tmux/process snapshot daemon.
//!
//! Every sidebar is a separate TUI process, but the expensive source data is
//! global to a tmux server. The first client starts a small daemon; subsequent
//! clients read its cached snapshot over a per-user Unix socket. If the daemon
//! cannot be reached, callers simply fall back to the legacy local query path.

use std::collections::hash_map::DefaultHasher;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::process::ProcessSnapshot;
use crate::tmux::{self, SessionInfo};

const PROTOCOL_VERSION: u8 = 1;
const TMUX_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const PROCESS_REFRESH_INTERVAL: Duration = Duration::from_secs(15);
const DAEMON_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotResponse {
    version: u8,
    sessions: Vec<SessionInfo>,
}

struct SnapshotCache {
    sessions: Vec<SessionInfo>,
    processes: Option<ProcessSnapshot>,
    last_tmux_refresh: Option<Instant>,
    last_process_refresh: Option<Instant>,
}

impl SnapshotCache {
    fn new() -> Self {
        Self {
            sessions: Vec::new(),
            processes: None,
            last_tmux_refresh: None,
            last_process_refresh: None,
        }
    }

    fn sessions(&mut self) -> Vec<SessionInfo> {
        if self
            .last_tmux_refresh
            .is_some_and(|last| last.elapsed() < TMUX_REFRESH_INTERVAL)
        {
            return self.sessions.clone();
        }

        let process_due = self
            .last_process_refresh
            .is_none_or(|last| last.elapsed() >= PROCESS_REFRESH_INTERVAL);
        if process_due {
            let (sessions, processes) = tmux::query_sessions_with_process_snapshot();
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
        self.last_tmux_refresh = Some(Instant::now());
        self.sessions.clone()
    }
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
    // an older wire/schema implementation. The old daemon becomes idle and
    // exits while the new version gets its own socket.
    crate::VERSION.hash(&mut hasher);
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
        match listener.accept() {
            Ok((mut stream, _)) => {
                last_request = Instant::now();
                let mut request = [0_u8; 1];
                if stream.read_exact(&mut request).is_ok() && request[0] == PROTOCOL_VERSION {
                    let response = SnapshotResponse {
                        version: PROTOCOL_VERSION,
                        sessions: cache.sessions(),
                    };
                    let _ = serde_json::to_writer(&mut stream, &response);
                    let _ = stream.flush();
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if last_request.elapsed() >= DAEMON_IDLE_TIMEOUT {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(err) => return Err(err),
        }
    }
}

fn request_once() -> Option<Vec<SessionInfo>> {
    let mut stream = UnixStream::connect(runtime_path("sock")).ok()?;
    stream.set_read_timeout(Some(CLIENT_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT)).ok()?;
    stream.write_all(&[PROTOCOL_VERSION]).ok()?;
    let response: SnapshotResponse = serde_json::from_reader(stream).ok()?;
    (response.version == PROTOCOL_VERSION).then_some(response.sessions)
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
pub fn query_sessions() -> Option<Vec<SessionInfo>> {
    if let Some(sessions) = request_once() {
        return Some(sessions);
    }
    spawn_daemon()?;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(25));
        if let Some(sessions) = request_once() {
            return Some(sessions);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_round_trips() {
        let response = SnapshotResponse {
            version: PROTOCOL_VERSION,
            sessions: vec![],
        };
        let encoded = serde_json::to_vec(&response).unwrap();
        let decoded: SnapshotResponse = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.version, PROTOCOL_VERSION);
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
}
