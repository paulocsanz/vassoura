//! seen.db (P2.1): a persistent per-candidate record of the last activity
//! the daemon "saw". Each poll cycle compares the current mtime with the
//! previous cycle's: it changed → there was activity → `last_seen` moves to
//! now; it did not change → `last_seen` stays in the past (a cold candidate,
//! first in LRU order). A candidate with no record falls back to mtime. The
//! record (and the timestamp of the last eviction, for the rate-limit)
//! survives a restart.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::walk::Candidate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenEntry {
    /// Last cycle in which the candidate showed activity (mtime changed).
    pub last_seen: SystemTime,
    /// Fingerprint of the previous cycle (newest mtime seen).
    pub last_mtime: SystemTime,
    /// When the daemon last evicted this path (churn-guard input). `None`
    /// for old records written before the field existed.
    pub last_removed: Option<SystemTime>,
}

/// One completed destructive cycle: when it ended and the free space right
/// after it. The daemon's churn breaker compares these samples across
/// cycles: if free space after eviction keeps coming back flat, what the
/// daemon deletes is being regenerated and eviction must stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EvictionSample {
    /// End of the cycle (nanos since the epoch).
    pub at: u64,
    /// Free bytes measured right after the cycle.
    pub free_after: u64,
}

/// How many eviction samples are kept (a small ring; the breaker looks at
/// the last few cycles only).
pub const HISTORY_CAP: usize = 8;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeenDb {
    /// Last eviction run by the daemon (rate-limit between cycles).
    pub last_eviction: Option<SystemTime>,
    /// path → record.
    pub entries: HashMap<String, SeenEntry>,
    /// Recent destructive cycles (churn-breaker input), bounded ring.
    pub eviction_history: Vec<EvictionSample>,
    /// Eviction suspended until this instant (churn breaker tripped).
    pub backoff_until: Option<SystemTime>,
    /// Consecutive breaker trips (backoff escalation: 1h, 2h, 4h … 24h cap).
    pub backoff_count: u32,
}

fn to_nanos(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

fn from_nanos(n: u64) -> SystemTime {
    UNIX_EPOCH + std::time::Duration::from_nanos(n)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SeenEntryJson {
    last_seen: u64,
    last_mtime: u64,
    #[serde(default)]
    last_removed: Option<u64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SeenDbJson {
    version: u32,
    last_eviction: Option<u64>,
    entries: HashMap<String, SeenEntryJson>,
    #[serde(default)]
    eviction_history: Vec<EvictionSample>,
    #[serde(default)]
    backoff_until: Option<u64>,
    #[serde(default)]
    backoff_count: u32,
}

impl SeenDb {
    pub fn load(path: &Path) -> SeenDb {
        let Ok(raw) = fs::read_to_string(path) else {
            return SeenDb::default();
        };
        let Ok(j) = serde_json::from_str::<SeenDbJson>(&raw) else {
            return SeenDb::default(); // corrupt → start cold again (mtime fallback)
        };
        SeenDb {
            last_eviction: j.last_eviction.map(from_nanos),
            entries: j
                .entries
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        SeenEntry {
                            last_seen: from_nanos(v.last_seen),
                            last_mtime: from_nanos(v.last_mtime),
                            last_removed: v.last_removed.map(from_nanos),
                        },
                    )
                })
                .collect(),
            eviction_history: j.eviction_history,
            backoff_until: j.backoff_until.map(from_nanos),
            backoff_count: j.backoff_count,
        }
    }

    pub fn persist(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let j = SeenDbJson {
            version: 1,
            last_eviction: self.last_eviction.map(to_nanos),
            entries: self
                .entries
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        SeenEntryJson {
                            last_seen: to_nanos(v.last_seen),
                            last_mtime: to_nanos(v.last_mtime),
                            last_removed: v.last_removed.map(to_nanos),
                        },
                    )
                })
                .collect(),
            eviction_history: self.eviction_history.clone(),
            backoff_until: self.backoff_until.map(to_nanos),
            backoff_count: self.backoff_count,
        };
        // atomic write: temp + rename in the same directory
        let tmp = path.with_extension("db.tmp");
        fs::write(&tmp, serde_json::to_string(&j).expect("seen.db serializes"))?;
        fs::rename(&tmp, path)
    }

    /// Update the record with the candidates seen this cycle: mtime changed
    /// since the previous cycle → activity (last_seen = now); a new candidate
    /// → last_seen is born at its own mtime (conservative fallback).
    pub fn refresh(&mut self, cands: &[Candidate], now: SystemTime) {
        for c in cands {
            let key = c.path.display().to_string();
            let e = self.entries.get_mut(&key);
            match e {
                Some(e) if e.last_mtime == c.newest_mtime => {} // cold: keep last_seen
                Some(e) => {
                    e.last_seen = now;
                    e.last_mtime = c.newest_mtime;
                }
                None => {
                    self.entries.insert(
                        key,
                        SeenEntry {
                            last_seen: c.newest_mtime,
                            last_mtime: c.newest_mtime,
                            last_removed: None,
                        },
                    );
                }
            }
        }
    }

    /// LRU sort key for the candidate: last-seen if there is a record,
    /// mtime as fallback.
    pub fn last_used(&self, cand: &Candidate) -> SystemTime {
        self.entries
            .get(&cand.path.display().to_string())
            .map(|e| e.last_seen)
            .unwrap_or(cand.newest_mtime)
    }

    pub fn mark_eviction(&mut self, now: SystemTime) {
        self.last_eviction = Some(now);
    }

    /// Mark a path as actively in-use (e.g. uncommitted worktree or open file)
    /// so it moves to the back of the LRU queue and doesn't block clean candidates.
    pub fn mark_active(&mut self, path: &Path, now: SystemTime) {
        let key = path.display().to_string();
        if let Some(e) = self.entries.get_mut(&key) {
            e.last_seen = now;
        } else {
            self.entries.insert(key, SeenEntry { last_seen: now, last_mtime: now, last_removed: None });
        }
    }

    /// When the daemon last evicted this exact path (churn-guard input).
    pub fn last_removed(&self, cand: &Candidate) -> Option<SystemTime> {
        self.entries.get(&cand.path.display().to_string()).and_then(|e| e.last_removed)
    }

    /// Record an eviction of this exact path (churn-guard input: the daemon
    /// must not come back for what it just harvested until it had time to
    /// prove itself cold — 7 days — not minutes).
    pub fn mark_removed(&mut self, path: &Path, now: SystemTime) {
        let key = path.display().to_string();
        let e = self
            .entries
            .entry(key)
            .or_insert(SeenEntry { last_seen: now, last_mtime: now, last_removed: None });
        e.last_removed = Some(now);
    }

    /// One completed destructive CYCLE (not per path — a cycle that removes
    /// 200 items is still one sample) with the free space it actually bought.
    pub fn record_eviction_sample(&mut self, at: SystemTime, free_after: u64) {
        self.eviction_history.push(EvictionSample { at: to_nanos(at), free_after });
        if self.eviction_history.len() > HISTORY_CAP {
            let cut = self.eviction_history.len() - HISTORY_CAP;
            self.eviction_history.drain(0..cut);
        }
    }

    /// Remaining backoff, if eviction is currently suspended. A pause of
    /// zero length is expiry, not backoff.
    pub fn in_backoff(&self, now: SystemTime) -> Option<std::time::Duration> {
        self.backoff_until
            .and_then(|until| until.duration_since(now).ok())
            .filter(|remaining| !remaining.is_zero())
    }

    /// Trip the breaker: suspend eviction, doubling the pause on each
    /// consecutive trip (1h, 2h, 4h … capped). Returns the pause taken.
    pub fn trip_backoff(
        &mut self,
        now: SystemTime,
        base: std::time::Duration,
        cap: std::time::Duration,
    ) -> std::time::Duration {
        self.backoff_count = self.backoff_count.saturating_add(1);
        let shift = (self.backoff_count - 1).min(5);
        let pause = base.saturating_mul(1u32 << shift).min(cap);
        self.backoff_until = Some(now + pause);
        pause
    }

    /// A destructive cycle made real progress: end the escalation (keep the
    /// history — it is the current evidence for the next window).
    pub fn reset_backoff_escalation(&mut self) {
        self.backoff_count = 0;
    }

    /// A destructive cycle made real progress (or the disk recovered):
    /// eviction resumes and the escalation restarts from the base pause.
    pub fn clear_backoff(&mut self) {
        if self.backoff_until.is_some() || self.backoff_count > 0 || !self.eviction_history.is_empty()
        {
            self.backoff_until = None;
            self.backoff_count = 0;
            self.eviction_history.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::Class;
    use std::time::Duration;

    fn cand(path: &str, age_secs: u64) -> Candidate {
        Candidate {
            path: path.into(),
            class: Class::Artifact,
            bytes: 10,
            newest_mtime: SystemTime::now() - Duration::from_secs(age_secs),
            root_mtime: SystemTime::now() - Duration::from_secs(age_secs),
            contains_git: false,
            entries: 1,
        }
    }

    #[test]
    fn round_trip_survives_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("seen.db");
        let mut db = SeenDb::default();
        let now = SystemTime::now();
        let c = cand("/x/nm", 100);
        db.refresh(std::slice::from_ref(&c), now);
        db.mark_eviction(now);
        db.persist(&p).unwrap();

        let back = SeenDb::load(&p);
        assert_eq!(back.last_eviction, Some(now), "nanos preserved across the round-trip");
        assert_eq!(back.entries.len(), 1);
        let e = &back.entries["/x/nm"];
        assert_eq!(e.last_seen, c.newest_mtime, "a new one is born at mtime");
        assert_eq!(e.last_mtime, c.newest_mtime);
    }

    #[test]
    fn refresh_marks_activity_and_keeps_cold_cold() {
        let mut db = SeenDb::default();
        let cold = cand("/a/nm", 500_000);
        let hot = cand("/b/nm", 500_000);
        let cycle1 = SystemTime::now() - Duration::from_secs(3600);
        db.refresh(&[cold.clone(), hot.clone()], cycle1);
        // born at their own mtime (conservative fallback)
        assert_eq!(db.last_used(&cold), cold.newest_mtime);
        assert_eq!(db.last_used(&hot), hot.newest_mtime);

        // one cycle later: /b was written (new mtime), /a was not
        let later = SystemTime::now();
        let hot2 = Candidate { newest_mtime: later - Duration::from_secs(5), ..hot.clone() };
        db.refresh(&[cold.clone(), hot2.clone()], later);

        assert_eq!(db.last_used(&cold), cold.newest_mtime, "cold keeps seen");
        assert_eq!(db.last_used(&hot2), later, "activity advances seen");
    }

    #[test]
    fn unknown_candidate_falls_back_to_mtime() {
        let db = SeenDb::default();
        let c = cand("/nunca/visto", 999);
        assert_eq!(db.last_used(&c), c.newest_mtime);
    }

    #[test]
    fn corrupt_db_starts_cold() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("seen.db");
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(SeenDb::load(&p), SeenDb::default());
    }

    #[test]
    fn seen_wins_over_mtime_and_unseen_falls_back() {
        let mut db = SeenDb::default();
        let old = SystemTime::now() - Duration::from_secs(90 * 86400);

        // `restored`: OLD mtime (90d, extracted from an archive), but the daemon
        // saw it change NOW (the file arrived yesterday, mtimes came in old):
        // the recent seen protects it — the case where seen ≠ mtime.
        let mut restored = cand("/restored", 0);
        restored.newest_mtime = old;
        let cycle1 = SystemTime::now() - Duration::from_secs(86400);
        db.refresh(std::slice::from_ref(&restored), cycle1);
        let mut changed = restored.clone();
        changed.newest_mtime = SystemTime::now() - Duration::from_secs(86300);
        let cycle2 = SystemTime::now();
        db.refresh(std::slice::from_ref(&changed), cycle2);
        db.refresh(std::slice::from_ref(&restored), cycle2); // mtime voltou a 90d

        // `virgem`: mtime 90d, never seen → mtime fallback
        let virgem = cand("/virgem", 90 * 86400);

        let mut keyed = [
            ("/restored", db.last_used(&restored)),
            ("/virgem", db.last_used(&virgem)),
        ];
        keyed.sort_by_key(|(_, k)| *k);
        assert_eq!(keyed[0].0, "/virgem", "never-seen (mtime 90d) leaves first");
        assert!(keyed[1].0 == "/restored" && db.last_used(&restored) > old);
    }

    #[test]
    fn mark_active_advances_last_seen() {
        let mut db = SeenDb::default();
        let c = cand("/in/use", 1000);
        let now = SystemTime::now();
        db.mark_active(Path::new("/in/use"), now);
        assert_eq!(db.last_used(&c), now);
    }

    #[test]
    fn churn_guard_state_round_trips_and_old_db_parses() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("seen.db");
        let mut db = SeenDb::default();
        let now = SystemTime::now();
        db.mark_removed(Path::new("/x/nm"), now);
        db.record_eviction_sample(now, 12 * 1024 * 1024 * 1024);
        db.persist(&p).unwrap();
        let back = SeenDb::load(&p);
        assert_eq!(back.last_removed(&cand("/x/nm", 50)), Some(now));
        assert_eq!(back.eviction_history.len(), 1);

        // seen.db written by a pre-churn-guard version: no new keys at all.
        let old = tmp.path().join("old.db");
        std::fs::write(
            &old,
            r#"{"version":1,"last_eviction":100,"entries":{"/a":{"last_seen":5,"last_mtime":5}}}"#,
        )
        .unwrap();
        let legacy = SeenDb::load(&old);
        assert_eq!(legacy.entries.len(), 1);
        assert_eq!(legacy.last_removed(&cand("/a", 5)), None);
        assert!(legacy.eviction_history.is_empty());
        assert_eq!(legacy.backoff_until, None);
    }

    #[test]
    fn history_is_a_bounded_ring() {
        let mut db = SeenDb::default();
        let t0 = SystemTime::now();
        for i in 0..(HISTORY_CAP + 4) {
            db.record_eviction_sample(t0, i as u64);
        }
        assert_eq!(db.eviction_history.len(), HISTORY_CAP);
        assert_eq!(db.eviction_history.last().unwrap().free_after, (HISTORY_CAP + 3) as u64);
    }

    #[test]
    fn backoff_escalates_doubles_and_caps() {
        let mut db = SeenDb::default();
        let now = SystemTime::now();
        let base = std::time::Duration::from_secs(3600);
        let cap = std::time::Duration::from_secs(24 * 3600);
        assert_eq!(db.trip_backoff(now, base, cap), std::time::Duration::from_secs(3600));
        assert_eq!(db.trip_backoff(now, base, cap), std::time::Duration::from_secs(2 * 3600));
        assert_eq!(db.trip_backoff(now, base, cap), std::time::Duration::from_secs(4 * 3600));
        assert_eq!(db.in_backoff(now), Some(std::time::Duration::from_secs(4 * 3600)));
        for _ in 0..10 {
            db.trip_backoff(now, base, cap);
        }
        assert_eq!(db.backoff_until, Some(now + cap), "pause caps at 24h, no overflow");
        assert!(db.in_backoff(now + cap).is_none(), "backoff expires");
        db.clear_backoff();
        assert_eq!((db.backoff_until, db.backoff_count, db.eviction_history.len()), (None, 0, 0));
    }
}
