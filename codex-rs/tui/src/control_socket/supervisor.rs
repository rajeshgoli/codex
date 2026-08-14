use super::ControlState;
use super::MAX_CONNECTION_WORKERS;
use super::handle_connection;
use crate::app_event_sender::AppEventSender;
use crate::session_log;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
#[cfg(unix)]
use std::os::unix::net::UnixStream;

const PATH_CHECK_INTERVAL: Duration = Duration::from_millis(50);
const REBIND_MAX_DELAY: Duration = Duration::from_secs(1);

pub(crate) struct ControlSocketHandle {
    shutdown: Arc<AtomicBool>,
    join_handle: Option<JoinHandle<()>>,
}

impl ControlSocketHandle {
    pub(crate) fn start(
        socket_path: PathBuf,
        app_event_tx: AppEventSender,
    ) -> std::io::Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = socket_path;
            let _ = app_event_tx;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "--control-socket is currently supported on Unix only",
            ))
        }

        #[cfg(unix)]
        {
            validate_socket_path(&socket_path)?;
            let generation = bind_initial_listener(&socket_path)?;
            let identity = generation.identity;
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_for_thread = Arc::clone(&shutdown);
            let socket_path_for_thread = socket_path.clone();

            let join_handle = match std::thread::Builder::new()
                .name("codex-control-socket".to_string())
                .spawn(move || {
                    supervise_listener(
                        socket_path_for_thread,
                        generation,
                        app_event_tx,
                        shutdown_for_thread,
                    );
                }) {
                Ok(handle) => handle,
                Err(err) => {
                    remove_socket_file_if_owned(&socket_path, identity)?;
                    return Err(err);
                }
            };

            Ok(Self {
                shutdown,
                join_handle: Some(join_handle),
            })
        }
    }

    pub(crate) fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(join_handle) = self.join_handle.take()
            && let Err(err) = join_handle.join()
        {
            tracing::debug!("control socket supervisor join failed: {err:?}");
        }
    }
}

impl Drop for ControlSocketHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
struct ListenerGeneration {
    listener: UnixListener,
    identity: SocketIdentity,
    epoch: String,
}

#[cfg(unix)]
enum ListenerExit {
    Shutdown,
    PathMissing,
    PathReplaced,
    AcceptFailed(std::io::Error),
}

#[cfg(unix)]
fn validate_socket_path(socket_path: &Path) -> std::io::Result<()> {
    if !socket_path.is_absolute() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "control socket path must be absolute",
        ));
    }
    let parent = socket_path.parent().ok_or_else(|| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            "control socket path must include a parent directory",
        )
    })?;
    fs::create_dir_all(parent)
}

#[cfg(unix)]
fn bind_initial_listener(path: &Path) -> std::io::Result<ListenerGeneration> {
    remove_existing_socket_if_safe(path)?;
    bind_listener(path)
}

#[cfg(unix)]
fn bind_listener(path: &Path) -> std::io::Result<ListenerGeneration> {
    let listener = UnixListener::bind(path)?;
    let identity = socket_identity(path)?;
    if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .and_then(|()| listener.set_nonblocking(true))
    {
        if let Err(cleanup_err) = remove_socket_file_if_owned(path, identity) {
            tracing::debug!("control socket setup cleanup failed: {cleanup_err}");
        }
        return Err(err);
    }
    Ok(ListenerGeneration {
        listener,
        identity,
        epoch: uuid::Uuid::new_v4().to_string(),
    })
}

#[cfg(unix)]
fn supervise_listener(
    socket_path: PathBuf,
    mut generation: ListenerGeneration,
    app_event_tx: AppEventSender,
    shutdown: Arc<AtomicBool>,
) {
    let active_workers = Arc::new(AtomicUsize::new(0));
    let mut generation_number = 1_u64;

    loop {
        log_lifecycle(
            if generation_number == 1 {
                "control_socket_started"
            } else {
                "control_socket_restarted"
            },
            &socket_path,
            generation_number,
            &generation.epoch,
            None,
        );
        let state = Arc::new(ControlState::new(
            app_event_tx.clone(),
            generation.epoch.clone(),
        ));
        let exit = run_listener_generation(
            &generation.listener,
            generation.identity,
            &socket_path,
            state,
            Arc::clone(&shutdown),
            Arc::clone(&active_workers),
        );

        if let Err(err) = remove_socket_file_if_owned(&socket_path, generation.identity) {
            tracing::warn!(
                "control socket ownership-safe cleanup failed for {}: {err}",
                socket_path.display()
            );
        }
        if matches!(exit, ListenerExit::Shutdown) {
            log_lifecycle(
                "control_socket_stopped",
                &socket_path,
                generation_number,
                &generation.epoch,
                None,
            );
            return;
        }

        let reason = match exit {
            ListenerExit::PathMissing => "socket path disappeared".to_string(),
            ListenerExit::PathReplaced => "socket path ownership changed".to_string(),
            ListenerExit::AcceptFailed(err) => format!("listener accept failed: {err}"),
            ListenerExit::Shutdown => unreachable!(),
        };
        tracing::warn!(
            "control socket degraded at {}: {reason}; rebinding",
            socket_path.display()
        );
        log_lifecycle(
            "control_socket_degraded",
            &socket_path,
            generation_number,
            &generation.epoch,
            Some(&reason),
        );

        let mut delay = PATH_CHECK_INTERVAL;
        generation = loop {
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            match bind_listener(&socket_path) {
                Ok(listener) => break listener,
                Err(err) => {
                    tracing::debug!(
                        "control socket rebind pending for {}: {err}",
                        socket_path.display()
                    );
                    std::thread::sleep(delay);
                    delay = std::cmp::min(delay.saturating_mul(2), REBIND_MAX_DELAY);
                }
            }
        };
        generation_number += 1;
    }
}

#[cfg(unix)]
fn run_listener_generation(
    listener: &UnixListener,
    identity: SocketIdentity,
    socket_path: &Path,
    state: Arc<ControlState>,
    shutdown: Arc<AtomicBool>,
    active_workers: Arc<AtomicUsize>,
) -> ListenerExit {
    let mut consecutive_accept_errors = 0_u8;
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                consecutive_accept_errors = 0;
                if active_workers.load(Ordering::Acquire) >= MAX_CONNECTION_WORKERS {
                    tracing::warn!(
                        "control socket worker limit reached ({}); dropping connection",
                        MAX_CONNECTION_WORKERS
                    );
                    drop(stream);
                    continue;
                }
                active_workers.fetch_add(1, Ordering::AcqRel);
                let connection_state = Arc::clone(&state);
                let connection_shutdown = Arc::clone(&shutdown);
                let connection_workers = Arc::clone(&active_workers);
                if let Err(err) = std::thread::Builder::new()
                    .name("codex-control-conn".to_string())
                    .spawn(move || {
                        if let Err(err) =
                            handle_connection(stream, connection_state, connection_shutdown)
                        {
                            tracing::warn!("control socket connection error: {err}");
                        }
                        connection_workers.fetch_sub(1, Ordering::AcqRel);
                    })
                {
                    active_workers.fetch_sub(1, Ordering::AcqRel);
                    tracing::warn!("failed to spawn control socket worker: {err}");
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                match socket_identity(socket_path) {
                    Ok(current) if current == identity => {}
                    Ok(_) => return ListenerExit::PathReplaced,
                    Err(err) if err.kind() == ErrorKind::NotFound => {
                        return ListenerExit::PathMissing;
                    }
                    Err(err) => return ListenerExit::AcceptFailed(err),
                }
                std::thread::sleep(PATH_CHECK_INTERVAL);
            }
            Err(err) => {
                consecutive_accept_errors = consecutive_accept_errors.saturating_add(1);
                if consecutive_accept_errors >= 3 {
                    return ListenerExit::AcceptFailed(err);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    ListenerExit::Shutdown
}

#[cfg(unix)]
fn socket_identity(path: &Path) -> std::io::Result<SocketIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("control socket path is not a socket: {}", path.display()),
        ));
    }
    Ok(SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(unix)]
fn remove_existing_socket_if_safe(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if !metadata.file_type().is_socket() {
        return Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            format!(
                "refusing to overwrite existing non-socket path: {}",
                path.display()
            ),
        ));
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            format!("control socket is already active at {}", path.display()),
        )),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound
            ) =>
        {
            fs::remove_file(path)
        }
        Err(err) => Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            format!(
                "control socket path exists and could not be verified as stale ({}): {err}",
                path.display()
            ),
        )),
    }
}

#[cfg(unix)]
fn remove_socket_file_if_owned(path: &Path, owned: SocketIdentity) -> std::io::Result<()> {
    match socket_identity(path) {
        Ok(current) if current == owned => fs::remove_file(path),
        Ok(_) => Ok(()),
        Err(err) if matches!(err.kind(), ErrorKind::NotFound | ErrorKind::InvalidData) => Ok(()),
        Err(err) => Err(err),
    }
}

fn log_lifecycle(
    event_type: &str,
    socket_path: &Path,
    generation: u64,
    epoch: &str,
    reason: Option<&str>,
) {
    session_log::log_control_socket_lifecycle(
        event_type,
        json!({
            "path": socket_path,
            "generation": generation,
            "epoch": epoch,
            "reason": reason,
        }),
    );
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
