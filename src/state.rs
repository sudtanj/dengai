//! The persistent auto-claim state (one small JSON file, written atomically) and the claim rate limiter.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::io::Write;
use std::path::Path;

/// A claimable license: a package (`sub/<id>`, store web endpoint) or an app (`a/<id>`,
/// `CMsgClientRequestFreeLicense`). Stored as those strings; a bare number (older state files) is a sub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "IdRepr", into = "String")]
pub enum Id {
    Sub(u32),
    App(u32),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum IdRepr {
    Num(u32),
    Str(String),
}

impl TryFrom<IdRepr> for Id {
    type Error = String;
    fn try_from(r: IdRepr) -> Result<Self, String> {
        match r {
            IdRepr::Num(n) => Ok(Id::Sub(n)),
            IdRepr::Str(s) => s.parse(),
        }
    }
}

impl std::str::FromStr for Id {
    type Err = String;
    /// `sub/1`, `s/1`, `a/2` or `app/2` (ASF's `!addlicense` syntax), case-insensitive.
    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("invalid id {s:?}, use sub/<id> or a/<id>");
        let (kind, n) = s.trim().split_once('/').ok_or_else(bad)?;
        let n = n.parse().map_err(|_| bad())?;
        match kind.to_ascii_lowercase().as_str() {
            "sub" | "s" => Ok(Id::Sub(n)),
            "a" | "app" => Ok(Id::App(n)),
            _ => Err(bad()),
        }
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Id::Sub(n) => write!(f, "sub/{n}"),
            Id::App(n) => write!(f, "a/{n}"),
        }
    }
}

impl From<Id> for String {
    fn from(id: Id) -> String {
        id.to_string()
    }
}

/// A backfill list: one id per line with `#` comments and blank lines, or a JSON array of id strings.
/// Lines that don't parse are skipped.
pub fn parse_id_list(body: &str) -> Vec<Id> {
    if body.trim_start().starts_with('[') {
        let list: Vec<String> = serde_json::from_str(body).unwrap_or_default();
        return list.iter().filter_map(|s| s.parse().ok()).collect();
    }
    body.lines().filter_map(|l| l.split('#').next()?.parse().ok()).collect()
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Last PICS change number processed.
    #[serde(default)]
    pub change_number: u32,
    /// Licenses waiting to be claimed.
    #[serde(default)]
    pub queue: VecDeque<Id>,
    /// Claimed (or found owned).
    #[serde(default)]
    pub done: BTreeSet<Id>,
    /// Refused by Steam; never retried.
    #[serde(default)]
    pub rejected: BTreeSet<Id>,
    /// Unix time of the last backfill (0 = never).
    #[serde(default)]
    pub backfilled_at: u64,
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

    /// True when the license was already handled or queued.
    pub fn known(&self, id: Id) -> bool {
        self.done.contains(&id) || self.rejected.contains(&id) || self.queue.contains(&id)
    }

    /// Queues a license unless it is already known. Returns whether it was added.
    pub fn enqueue(&mut self, id: Id) -> bool {
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
