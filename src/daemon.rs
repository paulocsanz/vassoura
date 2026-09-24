//! Daemon (P1.1): a poll loop with a watermark and HYSTERESIS.
//! Free space at or above the low mark (or between the marks) → nothing
//! happens; free space below the low mark → LRU eviction (oldest first)
//! bounded per cycle (byte bound + item bound) until free space reaches the
//! high mark or the bound runs out; a rate-limit between destructive cycles.
//! Every removal writes a ledger line (the single point is `clean::apply`).

use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::clean;
use crate::config::Config;
use crate::fmt_util::{human, GIB};
use crate::plan;
use crate::seen::SeenDb;
use crate::statusfs::{self, StatusReport};
use crate::walk;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleAction {
    /// Nothing to do: free ≥ low watermark (or already ≥ high).
    Idle,
    /// Evict LRU until `need_bytes` or the cycle bound runs out.
    Evict { need_bytes: u64, cap_bytes: u64, cap_items: usize },
}

/// Pure hysteresis decision for the cycle.
pub fn decide_cycle(free: u64, low_bytes: u64, high_bytes: u64, cap_bytes: u64, cap_items: usize) -> CycleAction {
    if free >= low_bytes {
        return CycleAction::Idle;
    }
    let need = high_bytes.saturating_sub(free);
    if need == 0 {
        // inverted marks or already at the high target: nothing to do
        return CycleAction::Idle;
    }
    CycleAction::Evict { need_bytes: need, cap_bytes, cap_items }
}

/// Rate-limit: an eviction less than `rate_limit` ago → hold this cycle.
pub fn rate_limited(last_eviction: Option<SystemTime>, now: SystemTime, rate_limit: Duration) -> bool {
    match last_eviction {
        None => false,
        Some(t) => now.duration_since(t).map(|d| d < rate_limit).unwrap_or(false),
    }
}

#[derive(Debug, Default)]
pub struct CycleReport {
    pub free_before: u64,
    pub free_after: u64,
    pub scanned: usize,
    pub eligible_bytes: u64,
    pub action: Option<CycleAction>,
    pub rate_limited: bool,
    pub removed: usize,
    pub freed: u64,
    pub skipped: Vec<(std::path::PathBuf, String)>,
}

/// One daemon cycle: scan → seen.db → **re-stat** → statusfs → decision →
/// (bounded eviction with the use gates) → ledger.
///
/// The statfs that decides is the one AFTER the scan. Inventorying the working
/// set takes tens of minutes and free space moves during that interval;
/// deciding from the stat at the start leaves the cycle IDLE with the disk
/// already below the mark (measured: 45.9 → 20.8 GiB, 110 GiB eligible, IDLE).
pub fn run_cycle(cfg: &Config) -> CycleReport {
    run_cycle_sampling_opts(cfg, || disk_of(cfg), false)
}

/// Run cycle with forced eviction (bypasses rate-limiting, e.g. when woken
/// by fast probe on critical disk usage >= 95%).
pub fn run_cycle_forced(cfg: &Config) -> CycleReport {
    run_cycle_sampling_opts(cfg, || disk_of(cfg), true)
}

/// `run_cycle` with an injectable disk sampler (tests: free space drops
/// between the start of the scan and the decision).
pub fn run_cycle_sampling(cfg: &Config, sample_disk: impl FnMut() -> crate::disk::Disk) -> CycleReport {
    run_cycle_sampling_opts(cfg, sample_disk, false)
}

pub fn run_cycle_sampling_opts(
    cfg: &Config,
    mut sample_disk: impl FnMut() -> crate::disk::Disk,
    force: bool,
) -> CycleReport {
    let mut rep = CycleReport::default();
    let d = sample_disk();
    rep.free_before = d.free;

    let low_now = (cfg.low_watermark_gib * GIB as f64) as u64;
    let deadline = if d.free < low_now || d.used_percent() >= 90.0 {
        Some(Duration::from_secs(45))
    } else {
        Some(Duration::from_secs(120))
    };
    let cands = walk::scan_with(cfg, None, true, deadline);
    rep.scanned = cands.len();

    // seen.db (P2.1): LRU by last-seen, persisted every cycle
    let mut seen = SeenDb::load(&crate::config::expand(&cfg.seen_db));
    seen.refresh(&cands, SystemTime::now());

    // re-stat: the decision, statusfs and tight min-age use the disk now,
    // not before the scan
    let d_now = sample_disk();
    let low = (cfg.low_watermark_gib * GIB as f64) as u64;
    let high = (cfg.until_free_gib * GIB as f64) as u64;

    let min_override = if d_now.free < low {
        Some(cfg.min_age_days_tight)
    } else {
        None
    };

    let (items, _rejected) = plan::build_seen(cands, cfg, min_override, &seen);
    rep.eligible_bytes = items.iter().map(|i| i.cand.bytes).sum();

    // status fs (P2.2): the same values as `status --json`
    let report: StatusReport =
        statusfs::build_report(d_now, cfg, rep.eligible_bytes, items.len());
    if let Err(e) = statusfs::write_status_dir(&crate::config::expand(&cfg.status_dir), &report) {
        eprintln!("# warning: statusfs {}: {e}", cfg.status_dir.display());
    }

    let cap_bytes = (cfg.daemon.max_bytes_per_cycle_gib * GIB as f64) as u64;
    let action = decide_cycle(d_now.free, low, high, cap_bytes, cfg.daemon.max_items_per_cycle);
    rep.action = Some(action.clone());
    if let CycleAction::Evict { need_bytes, cap_bytes, cap_items } = action {
        let rl = Duration::from_secs(cfg.daemon.rate_limit_secs);
        if !force && rate_limited(seen.last_eviction, SystemTime::now(), rl) {
            rep.rate_limited = true;
        } else if need_bytes > 0 {
            let packed = plan::pack_capped(items, d_now.free, cfg.until_free_gib, cap_items, Some(cap_bytes));
            let out = clean::apply(&packed.items, &crate::config::expand(&cfg.ledger));
            rep.removed = out.removed;
            rep.freed = out.freed;
            if out.removed > 0 {
                seen.mark_eviction(SystemTime::now());
                if cfg.daemon.notify {
                    let removed: Vec<&Path> = packed
                        .items
                        .iter()
                        .map(|i| i.cand.path.as_path())
                        .filter(|p| !out.skipped.iter().any(|(sp, _)| sp == p))
                        .collect();
                    notify(
                        "vassoura",
                        &evicted_body(&removed, out.freed),
                        Some(&crate::config::expand(&cfg.ledger)),
                    );
                }
            }
            for (p, why) in &out.skipped {
                if why.starts_with("in use") {
                    seen.mark_active(p, SystemTime::now());
                }
            }
            rep.skipped = out.skipped;
        }
    }

    if let Err(e) = seen.persist(&crate::config::expand(&cfg.seen_db)) {
        eprintln!("# warning: seen.db {}: {e}", cfg.seen_db.display());
    }

    rep.free_after = sample_disk().free;
    rep
}

/// Disk of the first allowlist root (fallback: /).
pub fn disk_of(cfg: &Config) -> crate::disk::Disk {
    let p = crate::config::expand(
        cfg.artifact_roots.first().map(|r| r.as_path()).unwrap_or_else(|| Path::new("/")),
    );
    crate::disk::Disk::snapshot(&p).unwrap_or_else(|| crate::disk::Disk::snapshot(Path::new("/")).expect("statfs of /"))
}

/// Best-effort macOS notification (silent failure: a courtesy, not a gate).
/// If `open` is given and `terminal-notifier` exists, the CLICK opens that
/// path (the ledger); plain osascript cannot bind an action to the click
/// (macOS activates the app that posted it — Script Editor), so the fallback
/// puts the path in the message body.
pub fn notify(title: &str, body: &str, open: Option<&Path>) {
    if let Some(target) = open {
        let url = format!("file://{}", target.display());
        let via_tn = Command::new("terminal-notifier")
            .args(["-title", title, "-message", body, "-open", &url])
            .status();
        if via_tn.is_ok() {
            return;
        }
    }
    let body = match open {
        Some(p) => format!("{body} — details: {}", p.display()),
        None => body.to_string(),
    };
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(format!(
            "display notification {} with title {}",
            quote_os(&body),
            quote_os(title)
        ))
        .status();
}

/// Eviction notification body: WHAT left (up to 3 paths + a count)
/// and how much was freed.
pub fn evicted_body(paths: &[&Path], freed: u64) -> String {
    let shown: Vec<String> = paths.iter().take(3).map(|p| p.display().to_string()).collect();
    let extra = paths.len().saturating_sub(shown.len());
    let list = match extra {
        0 => shown.join(", "),
        n => format!("{} (+{n})", shown.join(", ")),
    };
    format!("removed: {list} · {} freed", human(freed))
}

/// Escaped double quotes for AppleScript (our strings are simple).
fn quote_os(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = GIB;

    #[test]
    fn idle_when_free_at_or_above_low() {
        assert_eq!(decide_cycle(50 * G, 40 * G, 100 * G, 20 * G, 30), CycleAction::Idle);
        assert_eq!(decide_cycle(40 * G, 40 * G, 100 * G, 20 * G, 30), CycleAction::Idle);
        assert_eq!(decide_cycle(150 * G, 40 * G, 100 * G, 20 * G, 30), CycleAction::Idle);
    }

    #[test]
    fn evicts_below_low_until_high() {
        assert_eq!(
            decide_cycle(30 * G, 40 * G, 100 * G, 20 * G, 30),
            CycleAction::Evict { need_bytes: 70 * G, cap_bytes: 20 * G, cap_items: 30 }
        );
    }

    #[test]
    fn inverted_or_satisfied_marks_are_idle() {
        // free < low but already ≥ high (inverted marks): removes nothing
        assert_eq!(decide_cycle(50 * G, 60 * G, 40 * G, 20 * G, 30), CycleAction::Idle);
    }

    #[test]
    fn rate_limit_holds_recent_cycles_only() {
        let now = SystemTime::now();
        let rl = Duration::from_secs(900);
        assert!(!rate_limited(None, now, rl));
        assert!(rate_limited(Some(now), now, rl));
        assert!(rate_limited(Some(now - Duration::from_secs(600)), now, rl));
        assert!(!rate_limited(Some(now - Duration::from_secs(901)), now, rl));
    }

    #[test]
    fn evicted_body_names_what_left() {
        let a = Path::new("/p/a/node_modules");
        let b = Path::new("/p/b/node_modules");
        let c = Path::new("/p/c/target");
        let body = evicted_body(&[a, b], 4096);
        assert!(body.contains("/p/a/node_modules"), "{body}");
        assert!(body.contains("/p/b/node_modules"), "{body}");
        assert!(body.contains("freed"), "{body}");

        let many = evicted_body(&[a, b, c, Path::new("/p/d/dist")], 4096);
        assert!(many.contains("(+1)"), "more than 3 summarizes the count: {many}");
    }

    #[test]
    fn quote_os_escapes() {
        assert_eq!(quote_os("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }
}
