use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

fn socket_path() -> PathBuf {
    std::env::temp_dir().join("webtorapp.sock")
}

/// Tries to become the one true instance. Returns the listener to accept
/// future launches' pings on if this process should proceed as normal, or
/// `None` if another instance is already running - the attempted connection
/// above already served as the "please show yourself" notification, so the
/// caller should just exit immediately without doing anything else.
pub fn acquire() -> Option<UnixListener> {
    let path = socket_path();
    match UnixListener::bind(&path) {
        Ok(listener) => Some(listener),
        Err(_) => {
            if UnixStream::connect(&path).is_ok() {
                None
            } else {
                // Bind failed but nothing answers a connection either - a
                // stale socket file left behind by a process that didn't
                // exit cleanly. Safe to reclaim.
                let _ = std::fs::remove_file(&path);
                UnixListener::bind(&path).ok()
            }
        }
    }
}
