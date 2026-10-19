//! The lock files Pi takes through `proper-lockfile`: a directory named after the locked path plus
//! `.lock`, which counts as abandoned once its modification time is older than `stale`.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use tokio::time::Instant;

const RETRY: Duration = Duration::from_millis(100);

/// A held lock, released when dropped.
#[derive(Debug)]
pub struct DirLock(PathBuf);

impl DirLock {
    /// Waits up to `wait` for the lock on `path`, taking over one older than `stale`.
    pub async fn acquire(path: PathBuf, stale: Duration, wait: Duration) -> io::Result<Self> {
        let lock = PathBuf::from(format!("{}.lock", path.display()));
        let deadline = Instant::now() + wait;

        loop {
            match std::fs::create_dir(&lock) {
                Ok(()) => return Ok(Self(lock)),
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
                Err(_) => {}
            }

            if is_stale(&lock, stale) && std::fs::remove_dir_all(&lock).is_ok() {
                continue;
            }

            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{} is held by another process", lock.display()),
                ));
            }

            tokio::time::sleep(RETRY).await;
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn is_stale(lock: &PathBuf, stale: Duration) -> bool {
    std::fs::metadata(lock)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age > stale)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("tailnet-lock-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(format!("{}.lock", path.display()));

        path
    }

    #[tokio::test]
    async fn waits_for_a_held_lock_then_releases_on_drop() {
        let path = temp("held");
        let held = DirLock::acquire(path.clone(), Duration::from_secs(20), Duration::ZERO)
            .await
            .unwrap();

        let error = DirLock::acquire(path.clone(), Duration::from_secs(20), RETRY)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        drop(held);
        DirLock::acquire(path, Duration::from_secs(20), Duration::ZERO)
            .await
            .unwrap();
    }

    // Windows opens no directory as a file to set its time.
    #[cfg(unix)]
    #[tokio::test]
    async fn takes_over_an_abandoned_lock() {
        let path = temp("stale");
        let lock = PathBuf::from(format!("{}.lock", path.display()));
        std::fs::create_dir(&lock).unwrap();
        std::fs::File::open(&lock)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();

        DirLock::acquire(path, Duration::from_secs(20), Duration::ZERO)
            .await
            .unwrap();
    }
}
