use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use tracing::trace;

/// An exclusive, process-lifetime lock on a file, used to ensure only one
/// spotify-dl instance runs at a time.
///
/// Dropping the lock releases it.
pub struct InstanceLock {
    // fd-lock's write guard borrows the `RwLock` it was created from, so the
    // `RwLock` is leaked into a `'static` reference here. The guard keeps it
    // alive for as long as this `InstanceLock` exists, and dropping the guard
    // releases the underlying file lock.
    _guard: fd_lock::RwLockWriteGuard<'static, File>,
}

impl InstanceLock {
    /// Acquire an exclusive lock on the file at `lock_path`, creating it (and
    /// its parent directories) if necessary.
    ///
    /// The current pid is written into the lock file for easier debugging of
    /// stale locks.
    ///
    /// # Errors
    /// Returns an error if another instance already holds the lock file, or if
    /// the lock file cannot be opened or written.
    pub fn acquire(lock_path: PathBuf) -> Result<Self> {
        if let Some(parent) = lock_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create lock file directory {}", parent.display())
                })?;
            }
        }

        // Best effort: report which pid holds the lock, if the file says so.
        let holder_pid = fs::read_to_string(&lock_path)
            .ok()
            .and_then(|content| content.trim().parse::<u32>().ok());

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;

        let rw_lock = Box::leak(Box::new(fd_lock::RwLock::new(file)));

        let mut guard = match rw_lock.try_write() {
            Ok(guard) => guard,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                let holder = holder_pid
                    .map(|pid| format!(", held by pid {}", pid))
                    .unwrap_or_default();
                return Err(anyhow!(
                    "another spotify-dl instance is already running (lock file: {}{})",
                    lock_path.display(),
                    holder
                ));
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to lock lock file {}", lock_path.display()));
            }
        };

        guard
            .set_len(0)
            .with_context(|| format!("failed to truncate lock file {}", lock_path.display()))?;
        write!(guard, "{}", std::process::id())
            .with_context(|| format!("failed to write pid to lock file {}", lock_path.display()))?;
        guard
            .flush()
            .with_context(|| format!("failed to flush lock file {}", lock_path.display()))?;

        trace!("acquired instance lock at {}", lock_path.display());

        Ok(Self { _guard: guard })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::InstanceLock;

    fn lock_path(dir: &Path) -> PathBuf {
        dir.join("spotify-dl.lock")
    }

    /// fd-lock uses `flock(2)` with `LOCK_NB` on unix. `flock` locks belong to
    /// the open file description, so two independent handles on the same file
    /// conflict even within a single process; a second `acquire` is therefore
    /// correctly refused here.
    #[test]
    fn second_concurrent_acquire_is_refused() {
        let dir = tempdir().unwrap();
        let path = lock_path(dir.path());

        let first = InstanceLock::acquire(path.clone()).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents.trim(),
            std::process::id().to_string(),
            "lock file should contain the holder's pid"
        );

        let second = InstanceLock::acquire(path);
        let err = second
            .err()
            .expect("second acquire on the same lock file should fail");
        assert!(
            err.to_string().contains("already running"),
            "unexpected error message: {}",
            err
        );

        drop(first);
    }

    #[test]
    fn lock_is_released_on_drop() {
        let dir = tempdir().unwrap();
        let path = lock_path(dir.path());

        let first = InstanceLock::acquire(path.clone()).unwrap();
        drop(first);

        let second = InstanceLock::acquire(path).unwrap();
        drop(second);
    }
}
