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

// ---------------------------------------------------------------- churn breaker
// Incident 2026-09: `until_free_gib` was unreachable (needed 77 GiB, ~26 GiB
// evictable — all of it regenerating), so the daemon sat in EVICT forever:
// delete, apps/agents rebuild, delete again, one destructive cycle per
// minute (ledger: 5401 removals; Firefox cache 42×/week). The FSEvents storm
// took fseventsd to 84 GiB RSS and the machine thrashed. The breaker makes
// "evicting is not working" a first-class conclusion instead of a loop.

// ---------------------------------------------------------------- scan-health breaker
// Incident 2026-10-01: the machine entered IO collapse (disk 97%, swap full,
// agents building in parallel). The system killed every scan worker; `run()`
// returned Ok with ZERO candidates and the daemon printed
// "0 candidates · IDLE" while the disk burned — without the ability to
// measure it cleaned nothing and said nothing, and the in-process fallback
// wedged in the kernel for hours. The breaker makes "cannot measure" a
// first-class state: degrade to the shallow PROBE scan (seconds instead of
// an hour), say so loudly, and retry the full scan with backoff.

/// Consecutive catastrophic scans before degrading to probe mode.
pub const SCAN_DEATHS_TO_DEGRADE: u32 = 1;
/// Probe-mode cycles before the first full-scan retry; doubles each further
/// failure, capped (300s cadence → up to ~80 min between attempts).
pub const SCAN_RETRY_BASE_CYCLES: u32 = 2;
pub const SCAN_RETRY_CAP_CYCLES: u32 = 16;

/// Which scan the next cycle should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    Full,
    Probe,
}

#[derive(Debug, Default, Clone)]
pub struct ScanHealthState {
    pub probe_mode: bool,
    /// Consecutive catastrophic full scans (zero candidates + dead workers).
    pub deaths_streak: u32,
    /// Cycles spent in probe mode since the last full-scan attempt.
    pub probe_cycles: u32,
}

pub struct ScanVerdict {
    pub state: ScanHealthState,
    pub mode: ScanMode,
    /// Set on the transition into probe mode: the daemon notifies (this is
    /// the "it says so" part — silence was the 2026-10-01 failure).
    pub degraded_msg: Option<String>,
}

/// Pure transition: given the breaker state and this cycle's scan health,
/// decide the next cycle's scan mode.
pub fn next_scan_mode(
    state: ScanHealthState,
    health: &crate::scan_pool::ScanHealth,
    scanned: usize,
) -> ScanVerdict {
    let mut state = state;
    let catastrophic = scanned == 0
        && (health.worker_deaths + health.silence_kills + health.spawn_failures > 0
            || health.in_process_fallback);

    if !state.probe_mode {
        if catastrophic {
            state.deaths_streak += 1;
            if state.deaths_streak >= SCAN_DEATHS_TO_DEGRADE {
                state.probe_mode = true;
                state.probe_cycles = 0;
                return ScanVerdict {
                    state,
                    mode: ScanMode::Probe,
                    degraded_msg: Some(format!(
                        "scan workers died ({} this cycle) — shallow probe mode until IO pressure drops",
                        health.worker_deaths + health.silence_kills + health.spawn_failures
                    )),
                };
            }
        } else {
            state.deaths_streak = 0;
        }
        return ScanVerdict { state, mode: ScanMode::Full, degraded_msg: None };
    }

    // probe mode: full scan is retried with exponential cycle backoff
    state.probe_cycles += 1;
    let retry_after = SCAN_RETRY_BASE_CYCLES
        .saturating_pow(state.deaths_streak.min(4))
        .min(SCAN_RETRY_CAP_CYCLES);
    if state.probe_cycles >= retry_after {
        state.probe_cycles = 0;
        state.probe_mode = false;
        ScanVerdict { state, mode: ScanMode::Full, degraded_msg: None }
    } else {
        ScanVerdict { state, mode: ScanMode::Probe, degraded_msg: None }
    }
}

/// Consecutive destructive cycles the breaker looks at.
pub const CHURN_WINDOW: usize = 3;
/// Net free-space gain below which the window counts as "no progress".
pub const CHURN_MIN_PROGRESS_BYTES: u64 = GIB;
/// Minimum plunge in free space across the churn window that indicates an external
/// write flood rather than regeneration churn.
pub const CHURN_EXTERNAL_WRITE_FLOOD_BYTES: u64 = 5 * GIB;
/// First backoff pause when the breaker trips; doubles on each consecutive
/// trip, capped at `CHURN_BACKOFF_CAP`.
pub const CHURN_BACKOFF_BASE: Duration = Duration::from_secs(60 * 60);
pub const CHURN_BACKOFF_CAP: Duration = Duration::from_secs(24 * 60 * 60);

/// Disk usage percentage where fast probe wakes the daemon (95%).
pub const FAST_PROBE_USED_PERCENT: f64 = 95.0;
/// Extreme emergency disk usage percentage where OS survival takes absolute priority (98%).
pub const EMERGENCY_USED_PERCENT: f64 = 98.0;

/// Trip when the last `CHURN_WINDOW` destructive cycles bought less than
/// `CHURN_MIN_PROGRESS_BYTES` of durable free space: what the daemon deletes
/// is coming back between cycles.
///
/// A steep plunge caused by external writes (first - last is large) is a write
/// flood, NOT regeneration churn — evictions must not be suspended while disk is falling.
pub fn churn_detected(history: &[crate::seen::EvictionSample]) -> bool {
    if history.len() < CHURN_WINDOW {
        return false;
    }
    let tail = &history[history.len() - CHURN_WINDOW..];
    let first = tail[0].free_after;
    let last = tail[tail.len() - 1].free_after;

    // If free space plunged significantly across the window, external processes
    // are writing heavily. This is an external write flood, not regeneration churn.
    if first.saturating_sub(last) >= CHURN_EXTERNAL_WRITE_FLOOD_BYTES {
        return false;
    }

    last.saturating_sub(first) < CHURN_MIN_PROGRESS_BYTES
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
    /// The churn breaker is holding this cycle (and, if it just tripped,
    /// every cycle for the next backoff pause): evictions are buying no
    /// durable free space.
    pub backoff: bool,
    pub removed: usize,
    pub freed: u64,
    pub skipped: Vec<(std::path::PathBuf, String)>,
    /// Scan ran in shallow probe mode (scan-health breaker active).
    pub degraded: bool,
    /// Set on the transition into degraded mode: the caller notifies.
    pub notify_msg: Option<String>,
    /// What the scan pool reported about its own health this cycle.
    pub health: crate::scan_pool::ScanHealth,
}

/// One daemon cycle: scan → seen.db → **re-stat** → statusfs → decision →
/// (bounded eviction with the use gates) → ledger.
///
/// The statfs that decides is the one AFTER the scan. Inventorying the working
/// set takes tens of minutes and free space moves during that interval;
/// deciding from the stat at the start leaves the cycle IDLE with the disk
/// already below the mark (measured: 45.9 → 20.8 GiB, 110 GiB eligible, IDLE).
pub fn run_cycle(cfg: &Config) -> CycleReport {
    run_cycle_sampling_opts(cfg, || disk_of(cfg), false, ScanMode::Full)
}

/// Shallow-probe cycle: seconds of IO instead of a full walk. Used by the
/// scan-health breaker while the machine cannot survive a full scan.
pub fn run_cycle_probe(cfg: &Config) -> CycleReport {
    run_cycle_sampling_opts(cfg, || disk_of(cfg), false, ScanMode::Probe)
}

/// Run cycle with forced eviction (skips the rate-limit and churn backoff,
/// e.g. when woken by fast probe on critical disk usage >= 95%).
/// On critical disk, OS survival takes absolute priority over churn backoff.
pub fn run_cycle_forced(cfg: &Config) -> CycleReport {
    run_cycle_sampling_opts(cfg, || disk_of(cfg), true, ScanMode::Full)
}

/// `run_cycle` with an injectable disk sampler (tests: free space drops
/// between the start of the scan and the decision).
pub fn run_cycle_sampling(cfg: &Config, sample_disk: impl FnMut() -> crate::disk::Disk) -> CycleReport {
    run_cycle_sampling_opts(cfg, sample_disk, false, ScanMode::Full)
}

pub fn run_cycle_sampling_opts(
    cfg: &Config,
    mut sample_disk: impl FnMut() -> crate::disk::Disk,
    force: bool,
    mode: ScanMode,
) -> CycleReport {
    let mut rep = CycleReport::default();
    let d = sample_disk();
    rep.free_before = d.free;

    let is_emergency = force || d.used_percent() >= EMERGENCY_USED_PERCENT;
    let low_now = (cfg.low_watermark_gib * GIB as f64) as u64;
    let deadline = if d.free < low_now || d.used_percent() >= 90.0 {
        Some(Duration::from_secs(45))
    } else {
        Some(Duration::from_secs(120))
    };
    let cands = match mode {
        ScanMode::Probe => {
            rep.degraded = true;
            walk::scan_probe(cfg, None)
        }
        ScanMode::Full => {
            let (mut c, health) = walk::scan_with_health(cfg, None, true, deadline);
            // a full scan that lost its workers measured nothing: degraded
            // even in full mode, so the report never lies about coverage
            let lost = health.worker_deaths + health.silence_kills + health.spawn_failures + health.deadline_stopped;
            let truncated = health.jobs_uncompleted > 0 || health.deadline_stopped > 0;
            if (c.is_empty() && (lost > 0 || health.in_process_fallback)) || truncated {
                rep.degraded = true;
            }
            if is_emergency || truncated {
                for pc in walk::scan_probe(cfg, None) {
                    if !c.iter().any(|existing| existing.path == pc.path) {
                        c.push(pc);
                    }
                }
            }
            rep.health = health;
            c
        }
    };
    rep.scanned = cands.len();

    // seen.db (P2.1): LRU by last-seen, persisted every cycle
    let mut seen = SeenDb::load(&crate::config::expand(&cfg.seen_db));
    seen.refresh(&cands, SystemTime::now());

    // re-stat: the decision, statusfs and tight min-age use the disk now,
    // not before the scan
    let d_now = sample_disk();
    let low = (cfg.low_watermark_gib * GIB as f64) as u64;
    let high = (cfg.until_free_gib * GIB as f64) as u64;

    // The disk left the critical band (space was freed for real): whatever
    // churn evidence we had belongs to another regime.
    if d_now.free >= low {
        seen.clear_backoff();
    }

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
    let now = SystemTime::now();
    let mut free_after_sampled = false;
    let now_emergency = force || d_now.used_percent() >= EMERGENCY_USED_PERCENT;
    if let CycleAction::Evict { need_bytes, cap_bytes, cap_items } = action {
        if seen.in_backoff(now).is_some() && !now_emergency {
            // Churn breaker: recent destructive cycles bought no durable free
            // space — deleting more only feeds the regeneration loop.
            rep.backoff = true;
        } else {
            if now_emergency && seen.in_backoff(now).is_some() {
                // Emergency override: on critical disk (>= 95% full), OS survival
                // takes absolute priority over churn backoff.
                seen.clear_backoff();
            }
            let rl = Duration::from_secs(cfg.daemon.rate_limit_secs);
            if !force && rate_limited(seen.last_eviction, now, rl) {
                rep.rate_limited = true;
            } else if need_bytes > 0 {
                let candidate_paths: Vec<&Path> = items.iter().map(|i| i.cand.path.as_path()).collect();
                let gates = crate::gates::prepare(&candidate_paths);

                let mut free_items = Vec::new();
                for item in items {
                    if let Some(why) = gates.check(&item.cand.path) {
                        seen.mark_active(&item.cand.path, SystemTime::now());
                        rep.skipped.push((item.cand.path.clone(), why));
                    } else {
                        free_items.push(item);
                    }
                }

                let packed = plan::pack_capped(free_items, d_now.free, cfg.until_free_gib, cap_items, Some(cap_bytes));
                let out = clean::apply_with(&packed.items, &crate::config::expand(&cfg.ledger), &|p| gates.check(p));
                rep.removed = out.removed;
                rep.freed = out.freed;
                rep.free_after = sample_disk().free;
                free_after_sampled = true;
                if out.removed > 0 {
                    seen.mark_eviction(now);
                    let removed: Vec<&Path> = packed
                        .items
                        .iter()
                        .map(|i| i.cand.path.as_path())
                        .filter(|p| !out.skipped.iter().any(|(sp, _)| sp == p))
                        .collect();
                    for p in &removed {
                        seen.mark_removed(p, now);
                    }
                    // one sample per destructive CYCLE: its actual outcome
                    seen.record_eviction_sample(now, rep.free_after);
                    if churn_detected(&seen.eviction_history) {
                        let pause = seen.trip_backoff(now, CHURN_BACKOFF_BASE, CHURN_BACKOFF_CAP);
                        rep.backoff = true;
                        if cfg.daemon.notify {
                            notify(
                                "vassoura — evictions suspended",
                                &churn_body(pause, cfg),
                                Some(&crate::config::expand(&cfg.ledger)),
                            );
                        }
                    } else {
                        // this cycle bought durable space: end the escalation
                        seen.reset_backoff_escalation();
                        if cfg.daemon.notify {
                            notify(
                                "vassoura",
                                &evicted_body(&removed, out.freed),
                                Some(&crate::config::expand(&cfg.ledger)),
                            );
                        }
                    }
                }
                for (p, why) in &out.skipped {
                    if why.starts_with("in use") {
                        seen.mark_active(p, SystemTime::now());
                    }
                }
                rep.skipped.extend(out.skipped);
            }
        }
    }

    if let Err(e) = seen.persist(&crate::config::expand(&cfg.seen_db)) {
        eprintln!("# warning: seen.db {}: {e}", cfg.seen_db.display());
    }

    if !free_after_sampled {
        rep.free_after = sample_disk().free;
    }
    rep
}

/// Notification body when the breaker trips: say WHY eviction stopped and
/// what to do about it — an unreachable target is a human problem.
pub fn churn_body(pause: Duration, cfg: &Config) -> String {
    format!(
        "last {} eviction cycles bought no durable free space (regeneration churn) — \
         evictions suspended for {}h. The {} GiB free target may be unreachable: \
         lower until_free_gib or free space manually.",
        CHURN_WINDOW,
        pause.as_secs() / 3600,
        cfg.until_free_gib
    )
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

    fn sample(free_after: u64) -> crate::seen::EvictionSample {
        crate::seen::EvictionSample { at: 0, free_after }
    }

    #[test]
    fn churn_breaker_trips_on_flat_free_space_only() {
        // fewer cycles than the window: no verdict
        assert!(!churn_detected(&[]));
        assert!(!churn_detected(&[sample(20 * G), sample(20 * G)]));
        // the incident: free after eviction goes nowhere across the window
        assert!(churn_detected(&[sample(21 * G), sample(26 * G), sample(21 * G), sample(22 * G)]));
        // oscillation inside the window also counts as no progress
        assert!(churn_detected(&[sample(20 * G), sample(20 * G), sample(20 * G)]));
        // real progress: each cycle durably ahead of the last
        assert!(!churn_detected(&[sample(20 * G), sample(25 * G), sample(30 * G)]));
    }

    #[test]
    fn churn_breaker_does_not_trip_on_external_write_flood() {
        // External process is writing tens of GiB: free space is falling (40G -> 14G -> 12G).
        // This is an external write flood, NOT regeneration churn. The breaker must NOT trip!
        assert!(!churn_detected(&[sample(40 * G), sample(14 * G), sample(12 * G)]));
    }

    #[test]
    fn churn_body_names_the_problem_and_the_knob() {
        let cfg = Config::default();
        let body = churn_body(Duration::from_secs(4 * 3600), &cfg);
        assert!(body.contains("suspended for 4h"), "{body}");
        assert!(body.contains("unreachable"), "{body}");
        assert!(body.contains("until_free_gib"), "{body}");
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

    fn dead_health(n: usize) -> crate::scan_pool::ScanHealth {
        crate::scan_pool::ScanHealth { worker_deaths: n, ..Default::default() }
    }

    #[test]
    fn scan_breaker_degrades_after_catastrophic_scan_and_notifies() {
        let v = next_scan_mode(ScanHealthState::default(), &dead_health(4), 0);
        assert!(v.state.probe_mode, "zero candidates + dead workers → probe");
        assert_eq!(v.mode, ScanMode::Probe);
        let msg = v.degraded_msg.expect("transition says so");
        assert!(msg.contains("probe"), "{msg}");
    }

    #[test]
    fn scan_breaker_stays_full_when_workers_survived() {
        let v = next_scan_mode(ScanHealthState::default(), &dead_health(1), 52);
        assert!(!v.state.probe_mode, "candidates were collected: not catastrophic");
        assert_eq!(v.mode, ScanMode::Full);
        assert!(v.degraded_msg.is_none());
        // clean cycle resets the streak
        let after_bad = ScanHealthState { deaths_streak: 1, ..Default::default() };
        let v2 = next_scan_mode(after_bad, &crate::scan_pool::ScanHealth::default(), 52);
        assert_eq!(v2.state.deaths_streak, 0);
        assert_eq!(v2.mode, ScanMode::Full);
    }

    #[test]
    fn probe_mode_retries_full_with_backoff() {
        let degraded = ScanHealthState { probe_mode: true, deaths_streak: 1, probe_cycles: 0 };
        // retry_after = 2^deaths_streak = 2: ONE probe cycle, then a full retry
        let v1 = next_scan_mode(degraded.clone(), &crate::scan_pool::ScanHealth::default(), 3);
        assert_eq!(v1.mode, ScanMode::Probe);
        assert_eq!(v1.state.probe_cycles, 1);
        let v2 = next_scan_mode(v1.state.clone(), &crate::scan_pool::ScanHealth::default(), 3);
        assert_eq!(v2.mode, ScanMode::Full, "backoff window (2) elapsed → full retry");
        assert_eq!(v2.state.probe_cycles, 0);
        assert!(!v2.state.probe_mode, "probe flag cleared on full retry");
    }

    #[test]
    fn repeated_failures_escalate_the_backoff() {
        let mut state = ScanHealthState::default();
        // first failure → probe (streak 1 → retry every 2)
        state = next_scan_mode(state, &dead_health(3), 0).state;
        assert!(state.probe_mode);
        // one probe cycle holds…
        state = next_scan_mode(state, &Default::default(), 3).state;
        assert_eq!(state.probe_cycles, 1);
        // …then the full retry fires
        let retry = next_scan_mode(state, &Default::default(), 3);
        assert_eq!(retry.mode, ScanMode::Full);
        assert!(!retry.state.probe_mode);
        // …and fails catastrophically again → probe, streak 2 → retry every 4
        let v = next_scan_mode(retry.state, &dead_health(3), 0);
        assert_eq!(v.mode, ScanMode::Probe);
        assert_eq!(v.state.deaths_streak, 2);
        // with retry_after=4 the next THREE probe cycles hold, the 4th retries
        let mut s = v.state;
        let mut verdict = next_scan_mode(s, &Default::default(), 3);
        s = verdict.state;
        verdict = next_scan_mode(s, &Default::default(), 3);
        s = verdict.state;
        assert_eq!(verdict.mode, ScanMode::Probe, "backoff escalated: 2 → 4");
        verdict = next_scan_mode(s, &Default::default(), 3);
        s = verdict.state;
        assert_eq!(verdict.mode, ScanMode::Probe, "still holding at cycle 3 of 4");
        verdict = next_scan_mode(s, &Default::default(), 3);
        s = verdict.state;
        assert_eq!(verdict.mode, ScanMode::Full, "4th probe cycle retries full");
        assert!(!s.probe_mode, "probe flag cleared on full retry");
    }
}
