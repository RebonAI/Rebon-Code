use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub const UPDATE_LOCK_FILE_NAME: &str = "update.lock";

#[derive(Debug)]
pub struct UpdateLock {
    path: PathBuf,
    _file: File,
}

#[derive(Debug)]
pub enum UpdateLockError {
    Contended { path: PathBuf },
    Io { path: PathBuf, source: io::Error },
}

impl std::fmt::Display for UpdateLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contended { path } => write!(
                f,
                "another update process already holds lock {}",
                path.display()
            ),
            Self::Io { path, source } => {
                write!(
                    f,
                    "failed to acquire update lock {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for UpdateLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Contended { .. } => None,
        }
    }
}

impl UpdateLock {
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, UpdateLockError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| UpdateLockError::Io {
                path: path.clone(),
                source,
            })?;
        }

        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => Ok(Self { path, _file: file }),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                Err(UpdateLockError::Contended { path })
            }
            Err(source) => Err(UpdateLockError::Io { path, source }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn update_lock_path(config_home: &Path) -> PathBuf {
    config_home.join(UPDATE_LOCK_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_acquire_contend_release_reacquire() {
        let dir = tempfile::Builder::new()
            .prefix("rebon-update-lock-test-")
            .tempdir()
            .expect("temp dir");
        let path = update_lock_path(dir.path());

        let first = UpdateLock::acquire(&path).expect("first lock succeeds");
        assert!(path.exists());
        let second = UpdateLock::acquire(&path).expect_err("second lock contends");
        assert!(matches!(second, UpdateLockError::Contended { .. }));

        drop(first);
        let third = UpdateLock::acquire(&path).expect("lock reacquires after drop");
        drop(third);
        assert!(!path.exists());
    }
}
