//! The persistent auto-claim state (one small JSON file, written atomically) and the claim rate limiter.

use crate::purchase::Claim;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
    /// Account of the saved refresh token when it was signed in through the setup page.
    #[serde(default)]
    pub username: String,
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
    /// Failed attempts per queued license (keyed by its `sub/<id>` / `a/<id>` string).
    #[serde(default)]
    pub retries: BTreeMap<String, u8>,
    /// The claim rate limiter, so a restart doesn't forget a back-off.
    #[serde(default)]
    pub bucket: TokenBucket,
}

/// Failed attempts after which a license is rejected.
pub const MAX_RETRIES: u8 = 5;

impl State {
    /// Loads the state, or an empty one when the file doesn't exist. Drops duplicate queue entries
    /// and ones already claimed or rejected, which older versions could leave behind.
    pub fn load(path: &Path) -> Result<State, String> {
        match std::fs::read(path) {
            Ok(b) => {
                let mut s: State = serde_json::from_slice(&b).map_err(|e| format!("{}: {e}", path.display()))?;
                let (done, rejected, mut seen) = (&s.done, &s.rejected, BTreeSet::new());
                s.queue.retain(|id| !done.contains(id) && !rejected.contains(id) && seen.insert(*id));
                Ok(s)
            }
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

    /// Applies the outcome of claiming `id` (the queue front): done, rejected, or back of the queue
    /// until `MAX_RETRIES` failures. Returns true when a retry limit moved it to `rejected`.
    pub fn settle(&mut self, id: Id, claim: &Claim) -> bool {
        let key = id.to_string();
        match claim {
            Claim::RateLimited => return false,
            Claim::Ok => {
                self.done.insert(id);
            }
            Claim::Rejected(_) => {
                self.rejected.insert(id);
            }
            Claim::Retry(_) => {
                let failures = self.retries.get(&key).copied().unwrap_or(0) + 1;
                if failures < MAX_RETRIES {
                    self.retries.insert(key, failures);
                    self.queue.retain(|q| *q != id);
                    self.queue.push_back(id);
                    return false;
                }
                self.rejected.insert(id);
            }
        }
        self.queue.retain(|q| *q != id);
        self.retries.remove(&key);
        matches!(claim, Claim::Retry(_))
    }
}

/// Token bucket: `per_hour` claims, refilled continuously, with a hard back-off window that doubles
/// on each rate limit until a claim succeeds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenBucket {
    /// Set from the config by `configure`.
    #[serde(skip)]
    capacity: f64,
    #[serde(default)]
    tokens: f64,
    #[serde(default)]
    last: u64,
    #[serde(default)]
    blocked_until: u64,
    /// Consecutive rate limits.
    #[serde(default)]
    strikes: u32,
}

/// The first back-off; it doubles per strike up to `BACKOFF_DOUBLINGS` times (1 h, 2 h, 4 h, 8 h).
const BACKOFF_BASE_SECS: u64 = 3600;
const BACKOFF_DOUBLINGS: u32 = 3;

impl TokenBucket {
    pub fn new(per_hour: u32, now: u64) -> Self {
        let mut bucket = TokenBucket::default();
        bucket.configure(per_hour, now);
        bucket
    }

    /// Sets the size from the config. A bucket that was never used starts full.
    pub fn configure(&mut self, per_hour: u32, now: u64) {
        self.capacity = per_hour.max(1) as f64;
        if self.last == 0 {
            self.tokens = self.capacity;
            self.last = now;
        }
        self.tokens = self.tokens.min(self.capacity);
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

    /// Empties the bucket and blocks it for 1 h, doubling on each consecutive call up to 8 h.
    pub fn back_off(&mut self, now: u64) {
        let secs = BACKOFF_BASE_SECS << self.strikes.min(BACKOFF_DOUBLINGS);
        self.strikes += 1;
        self.tokens = 0.0;
        self.last = now + secs;
        self.blocked_until = now + secs;
    }

    /// Forgets earlier rate limits after a successful claim.
    pub fn reset_backoff(&mut self) {
        self.strikes = 0;
    }

    /// Unix time until which the bucket is blocked (0 when not blocked).
    pub fn blocked_until(&self) -> u64 {
        self.blocked_until
    }
}
