//! The persistent auto-claim state (one small JSON file, written atomically) and the claim rate limiter.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Last PICS change number processed.
    #[serde(default)]
    pub change_number: u32,
    /// Package ids waiting to be claimed.
    #[serde(default)]
    pub queue: VecDeque<u32>,
    /// Claimed (or found owned).
    #[serde(default)]
    pub done: BTreeSet<u32>,
    /// Refused by Steam; never retried.
    #[serde(default)]
    pub rejected: BTreeSet<u32>,
}

impl State {
    /// Loads the state, or an empty one when the file doesn't exist.
    pub fn load(path: &Path) -> Result<State, String> {
        match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Writes to `<path>.tmp`, syncs, then renames over `path`.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    }

    /// True when the package was already handled or queued.
    pub fn known(&self, id: u32) -> bool {
        self.done.contains(&id) || self.rejected.contains(&id) || self.queue.contains(&id)
    }

    /// Queues a package unless it is already known. Returns whether it was added.
    pub fn enqueue(&mut self, id: u32) -> bool {
        let new = !self.known(id);
        if new {
            self.queue.push_back(id);
        }
        new
    }
}

/// Token bucket: `per_hour` claims, refilled continuously, with a hard back-off window.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    last: u64,
    blocked_until: u64,
}

impl TokenBucket {
    pub fn new(per_hour: u32, now: u64) -> Self {
        let capacity = per_hour.max(1) as f64;
        TokenBucket { capacity, tokens: capacity, last: now, blocked_until: 0 }
    }

    /// Takes one token if available.
    pub fn try_take(&mut self, now: u64) -> bool {
        if now < self.blocked_until {
            return false;
        }
        let elapsed = now.saturating_sub(self.last) as f64;
        self.tokens = (self.tokens + elapsed * self.capacity / 3600.0).min(self.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Empties the bucket and blocks it for `secs`.
    pub fn back_off(&mut self, now: u64, secs: u64) {
        self.tokens = 0.0;
        self.last = now + secs;
        self.blocked_until = now + secs;
    }

    /// Unix time until which the bucket is blocked (0 when not blocked).
    pub fn blocked_until(&self) -> u64 {
        self.blocked_until
    }
}
