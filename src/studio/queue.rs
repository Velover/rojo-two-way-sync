//! Takes turns running Luau in Studio with other tools on this machine.
//!
//! Studio runs MCP work on its main thread, so calls that overlap time out or freeze it. Tools that
//! share Studio agree on a lock: a folder in the OS temp directory, `roblox-studio-queue/exec/lock`,
//! made with one `mkdir` (which only one process can win), with a short gap after the last holder
//! released it. This is the same lock as Orchid's `Tools/StudioQueue` and the hooks around an
//! assistant's own `execute_luau` calls, so a push waits for them and they for it.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::bail;

/// Time between one holder's release and the next holder's start.
const GAP: Duration = Duration::from_millis(250);
/// A holder that never released (crashed, killed) loses the lock after this.
const STALE: Duration = Duration::from_secs(180);
const MAX_WAIT: Duration = Duration::from_secs(540);
const POLL: Duration = Duration::from_millis(200);

fn dir() -> PathBuf {
    std::env::temp_dir()
        .join("roblox-studio-queue")
        .join("exec")
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// The lock while held; released when dropped.
pub struct ExecTurn {
    lock: PathBuf,
    last: PathBuf,
}

impl ExecTurn {
    /// Waits for the lock and the gap after its last holder.
    pub fn take(owner: &str) -> anyhow::Result<Self> {
        let dir = dir();
        fs_err::create_dir_all(&dir)?;
        let lock = dir.join("lock");
        let last = dir.join("last");
        let started = SystemTime::now();
        loop {
            if std::fs::create_dir(&lock).is_ok() {
                break;
            }
            let age = std::fs::metadata(&lock)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok());
            if age.is_some_and(|age| age > STALE) {
                let _ = std::fs::remove_dir_all(&lock);
                continue;
            }
            if started.elapsed().unwrap_or_default() > MAX_WAIT {
                bail!(
                    "Studio stayed busy with other work for {}s (the lock in {})",
                    MAX_WAIT.as_secs(),
                    dir.display()
                );
            }
            thread::sleep(POLL);
        }
        let _ = std::fs::write(
            lock.join("owner"),
            format!(
                "{owner} {}",
                humantime::format_rfc3339_seconds(SystemTime::now())
            ),
        );

        let released: u128 = std::fs::read_to_string(&last)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0);
        let wait = (released + GAP.as_millis()).saturating_sub(now_ms());
        if wait > 0 {
            thread::sleep(Duration::from_millis(wait as u64));
        }
        Ok(ExecTurn { lock, last })
    }
}

impl Drop for ExecTurn {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.last, now_ms().to_string());
        let _ = std::fs::remove_dir_all(&self.lock);
    }
}
