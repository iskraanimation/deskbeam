//! One process per data directory.
//!
//! Two engines on one data directory would run the same iroh identity twice and write the
//! same files. The app and the CLI both take this lock before starting an engine.

use std::{
    fs::{File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
};

/// Held while this process owns the data directory. Dropping it releases the lock.
#[derive(Debug)]
pub struct DataDirLock {
    _file: File,
    path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another DeskBeam is already using {0}")]
    InUse(PathBuf),
    #[error("could not lock {0}: {1}")]
    Io(PathBuf, std::io::Error),
}

impl DataDirLock {
    pub const FILE_NAME: &'static str = "deskbeam.lock";

    /// Takes the lock on `data_dir`, creating the directory if needed.
    pub fn acquire(data_dir: &Path) -> Result<Self, LockError> {
        let io = |e| LockError::Io(data_dir.to_path_buf(), e);
        std::fs::create_dir_all(data_dir).map_err(io)?;
        let path = data_dir.join(Self::FILE_NAME);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(io)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file, path }),
            Err(TryLockError::WouldBlock) => Err(LockError::InUse(data_dir.to_path_buf())),
            Err(TryLockError::Error(e)) => Err(io(e)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_is_refused_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let first = DataDirLock::acquire(dir.path()).unwrap();
        assert!(matches!(
            DataDirLock::acquire(dir.path()),
            Err(LockError::InUse(_))
        ));
        drop(first);
        DataDirLock::acquire(dir.path()).unwrap();
    }
}
