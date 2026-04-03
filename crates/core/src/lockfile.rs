use std::{
    fs::OpenOptions,
    io::Write as _,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::{CoreError, Result};

pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn acquire_lock(path: &Path, timeout: Duration) -> Result<LockGuard> {
    let start = Instant::now();

    loop {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut f) => {
                let _ = writeln!(f, "pid={}", std::process::id());
                return Ok(LockGuard {
                    path: path.to_path_buf(),
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if start.elapsed() >= timeout {
                    return Err(CoreError::LockTimeout {
                        path: path.to_path_buf(),
                        timeout,
                    });
                }

                std::thread::sleep(Duration::from_millis(50));
            }
            Err(source) => return Err(CoreError::Io { source }),
        }
    }
}

