use std::path::Path;
use std::time::SystemTime;

use crate::config::Config;
use crate::fmt_util::{age_days, GIB};
use crate::seen::SeenDb;
use crate::walk::Candidate;

/// Eligible candidate with a resolved age and a regeneration hint.
/// `last_used` is the LRU key (last-seen from seen.db; mtime as fallback).
#[derive(Debug, Clone)]
pub struct PlanItem {
    pub cand: Candidate,
    pub age_days: f64,
    pub last_used: SystemTime,
    pub hint: String,
}

/// Candidate left out of the plan, with the reason (shown in the report).
#[derive(Debug, Clone)]
pub struct Rejected {
    pub path: std::path::PathBuf,
    pub bytes: u64,
    pub reason: String,
}

/// Split out the eligible ones (age ≥ the class minimum, no inner `.git`).
pub fn build(
    cands: Vec<Candidate>,
    cfg: &Config,
    override_min: Option<f64>,
) -> (Vec<PlanItem>, Vec<Rejected>) {
    build_inner(cands, cfg, override_min, None, false)
}

/// The daemon path: `build` with LRU by last-seen (seen.db, P2.1) and the
/// CHURN GUARD — a path evicted less than `CHURN_GUARD_DAYS` ago stays out of
/// the plan: whatever regenerated it is alive and will keep regenerating it
/// (measured 2026-09: Firefox cache evicted 42×/week; each round-trip is a
/// fresh FSEvents storm). The manual `clean` CLI uses plain `build` and is
/// the human escape hatch.
pub fn build_seen(
    cands: Vec<Candidate>,
    cfg: &Config,
    override_min: Option<f64>,
    seen: &SeenDb,
) -> (Vec<PlanItem>, Vec<Rejected>) {
    build_inner(cands, cfg, override_min, Some(seen), false)
}

/// Emergency daemon path (Invariant 5 emergency rule): on critical disk
/// usage (>= 95% / forced cycle), OS survival takes absolute priority over
/// churn guard: the daemon evicts eligible candidates rather than allowing
/// the disk to reach 0 bytes (ENOSPC).
pub fn build_seen_emergency(
    cands: Vec<Candidate>,
    cfg: &Config,
    override_min: Option<f64>,
    seen: &SeenDb,
    emergency: bool,
) -> (Vec<PlanItem>, Vec<Rejected>) {
    build_inner(cands, cfg, override_min, Some(seen), emergency)
}

/// A path evicted this recently is regeneration, not garbage.
pub const CHURN_GUARD_DAYS: f64 = 7.0;

fn build_inner(
    cands: Vec<Candidate>,
    cfg: &Config,
    override_min: Option<f64>,
    seen: Option<&SeenDb>,
    emergency: bool,
) -> (Vec<PlanItem>, Vec<Rejected>) {
    let now = SystemTime::now();
    let mut items = Vec::new();
    let mut rejected = Vec::new();
    for c in cands {
        if c.bytes == 0 {
            rejected.push(Rejected {
                path: c.path.clone(),
                bytes: 0,
                reason: "empty (0 B)".into(),
            });
            continue;
        }
        if c.contains_git {
            rejected.push(Rejected {
                path: c.path.clone(),
                bytes: c.bytes,
                reason: "contains .git (protected)".into(),
            });
            continue;
        }
        // an explicit per-root age override marks a temp zone: it regenerates
        // by the GB per hour and re-deleting it harms nobody — 1 day of guard
        let root_override = cfg.age_override_for(&c.path);
        let churn_days = if root_override.is_some() { 1.0 } else { CHURN_GUARD_DAYS };
        if let Some(seen) = seen {
            if !emergency {
                if let Some(removed_at) = seen.last_removed(&c) {
                    let since = age_days(now, removed_at);
                    if since < churn_days {
                        rejected.push(Rejected {
                            path: c.path.clone(),
                            bytes: c.bytes,
                            reason: format!(
                                "churn guard: evicted {} ago (< {}d) — it regenerated; leave it",
                                fmt_age_short(since),
                                churn_days
                            ),
                        });
                        continue;
                    }
                }
            }
        }
        // explicit root override lowers the class minimum (tight mode keeps
        // winning when critical: the 1h absolute floor is in min_age_days_for)
        let min = match root_override {
            Some(o) => o.min(cfg.min_age_days_for(c.class, override_min)),
            None => cfg.min_age_days_for(c.class, override_min),
        };
        let age = age_days(now, c.newest_mtime);
        if age < min {
            rejected.push(Rejected {
                path: c.path.clone(),
                bytes: c.bytes,
                reason: format!("young: {} < {} min age", fmt_age_short(age), fmt_age_short(min)),
            });
            continue;
        }
        let hint = regen_hint(&c.path);
        let last_used = match seen {
            Some(s) => s.last_used(&c),
            None => c.newest_mtime,
        };
        items.push(PlanItem { cand: c, age_days: age, last_used, hint });
    }
    (items, rejected)
}

/// Compact age for humans: "42m", "3h", "5d" — sub-day minimums were
/// unreadable as fractions of a day ("0.04d").
pub fn fmt_age_short(days: f64) -> String {
    if days < 1.0 / 24.0 {
        format!("{:.0}m", (days * 1440.0).max(1.0))
    } else if days < 1.0 {
        format!("{:.0}h", days * 24.0)
    } else {
        format!("{days:.0}d")
    }
}

pub struct Packed {
    /// Eviction order: oldest first (LRU by last write).
    pub items: Vec<PlanItem>,
    pub target_bytes: u64,
    pub need_bytes: u64,
    pub planned_bytes: u64,
    pub reached: bool,
}

/// Below this the item is dust: it does not take a plan slot while a large
/// candidate remains and the byte target has not been met. Without this the
/// item cap fills with `__pycache__` of tens of KiB (the oldest) and the
/// GiB-sized `target`/`node_modules` stay out — seen on 2026-09-23:
/// 400 items, 5.2 GiB in the plan, 55 MiB actually freed, disk still full.
const SLOT_FLOOR: u64 = 64 * 1024 * 1024;

/// Pack eligible items in LRU order (least recently used first) until the
/// free-space target, the `top` item cap, or the per-cycle byte cap.
/// Items ≥ `SLOT_FLOOR` go in first; dust only takes a slot afterwards.
pub fn pack(items: Vec<PlanItem>, free: u64, until_free_gib: f64, top: usize) -> Packed {
    pack_capped(items, free, until_free_gib, top, None)
}

/// `pack` with a per-cycle byte bound (daemon: bounded hysteresis).
pub fn pack_capped(
    mut items: Vec<PlanItem>,
    free: u64,
    until_free_gib: f64,
    top: usize,
    cap_bytes: Option<u64>,
) -> Packed {
    let by_lru = |a: &PlanItem, b: &PlanItem| {
        a.last_used
            .cmp(&b.last_used)
            .then(b.cand.bytes.cmp(&a.cand.bytes))
    };
    items.sort_by(by_lru);
    let (mut big, mut dust) = (Vec::new(), Vec::new());
    for it in items {
        if it.cand.bytes >= SLOT_FLOOR {
            big.push(it);
        } else {
            dust.push(it);
        }
    }
    let target = (until_free_gib * GIB as f64) as u64;
    let need = target.saturating_sub(free);
    let mut planned = 0u64;
    let mut out = Vec::new();
    for it in big.into_iter().chain(dust) {
        if planned >= need || out.len() >= top {
            break;
        }
        // per-cycle bound: an item that blows the cap stays out of the cycle; the
        // only exception is the first item (progress guarantee — without it a
        // candidate larger than the cap would never leave).
        if let Some(cap) = cap_bytes {
            if !out.is_empty() && planned.saturating_add(it.cand.bytes) > cap {
                continue;
            }
        }
        planned += it.cand.bytes;
        out.push(it);
    }
    let reached = planned >= need;
    Packed { items: out, target_bytes: target, need_bytes: need, planned_bytes: planned, reached }
}

fn has(dir: &Path, name: &str) -> bool {
    dir.join(name).exists()
}

/// Hint for how to bring the directory back — recorded in the ledger.
pub fn regen_hint(path: &Path) -> String {
    let parent = path.parent().unwrap_or(path);
    let in_lake = path.ancestors().any(|a| a.file_name().is_some_and(|n| n == ".lake"));
    if in_lake {
        return "lake build".into();
    }
    if path.file_name().is_some_and(|n| n == ".terraform") {
        return "terraform init".into();
    }
    if has(parent, "package.json") && has(parent, "pnpm-lock.yaml") {
        return "pnpm install".into();
    }
    if has(parent, "package.json") && has(parent, "package-lock.json") {
        return "npm ci".into();
    }
    if has(parent, "yarn.lock") {
        return "yarn install".into();
    }
    if has(parent, "bun.lockb") || has(parent, "bun.lock") {
        return "bun install".into();
    }
    if has(parent, "package.json") {
        return "npm install".into();
    }
    if has(parent, "Cargo.toml") {
        return "cargo build".into();
    }
    if has(parent, "Podfile") {
        return "pod install".into();
    }
    if has(parent, "build.gradle") || has(parent, "build.gradle.kts") || has(parent, "settings.gradle") {
        return "gradle build".into();
    }
    if has(parent, "pyproject.toml") || has(parent, "requirements.txt") {
        return "recreate the venv + pip install".into();
    }
    if has(parent, "go.mod") {
        return "go mod download && go build".into();
    }
    if has(parent, "mix.exs") {
        return "mix deps.get".into();
    }
    if has(parent, "Gemfile") {
        return "bundle install".into();
    }
    "rebuild the project (regenerable)".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::Class;
    use std::time::Duration;

    fn cand(path: &str, bytes: u64, age_d: u64) -> Candidate {
        Candidate {
            path: path.into(),
            class: Class::Artifact,
            bytes,
            newest_mtime: SystemTime::now() - Duration::from_secs(age_d * 86400),
            root_mtime: SystemTime::now() - Duration::from_secs(age_d * 86400),
            contains_git: false,
            entries: 1,
        }
    }

    fn item(path: &str, bytes: u64, age_d: u64) -> PlanItem {
        PlanItem {
            cand: cand(path, bytes, age_d),
            age_days: age_d as f64,
            last_used: SystemTime::now() - Duration::from_secs(age_d * 86400),
            hint: String::new(),
        }
    }

    #[test]
    fn pack_evicts_oldest_first_until_need() {
        let items = vec![
            item("/x/novo", 10 * GIB, 20),
            item("/x/velho1", 30 * GIB, 100),
            item("/x/velho2", 30 * GIB, 60),
        ];
        let free = 10 * GIB;
        let p = pack(items, free, 50.0, 100);
        assert_eq!(p.need_bytes, 40 * GIB);
        assert!(p.reached);
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].cand.path, Path::new("/x/velho1"));
        assert_eq!(p.items[1].cand.path, Path::new("/x/velho2"));
    }

    #[test]
    fn pack_respects_top_and_reports_unreached() {
        let items = vec![item("/x/a", 5 * GIB, 100), item("/x/b", 5 * GIB, 90), item("/x/c", 5 * GIB, 80)];
        let p = pack(items, 0, 100.0, 2);
        assert!(!p.reached, "top cut off before the target");
        assert_eq!(p.items.len(), 2);
    }

    #[test]
    fn pack_respects_byte_cap_per_cycle() {
        let items = vec![
            item("/x/velho1", 10 * GIB, 100),
            item("/x/velho2", 10 * GIB, 90),
            item("/x/velho3", 10 * GIB, 80),
        ];
        // unreachable target (need 100 GiB), cap 25 GiB per cycle: 2 items
        // (20 ≤ 25); the 3rd would blow it → left for the next cycle.
        let p = pack_capped(items, 0, 100.0, 100, Some(25 * GIB));
        assert!(!p.reached, "cap runs out before the target");
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.planned_bytes, 20 * GIB);

        // cap smaller than any item: only the LRU one (progress), never two.
        let items2 = vec![item("/x/velho1", 10 * GIB, 100), item("/x/velho2", 10 * GIB, 90)];
        let p2 = pack_capped(items2, 0, 100.0, 100, Some(5 * GIB));
        assert_eq!(p2.items.len(), 1);
        assert_eq!(p2.planned_bytes, 10 * GIB);
    }

    #[test]
    fn pack_fills_slots_with_large_items_before_ancient_dust() {
        // ancient dust must not spend the item cap while a GiB-sized
        // target (newer, already above the minimum age) exists.
        let mut items: Vec<PlanItem> = (0..10)
            .map(|i| item(&format!("/dust/{i}"), 20 * 1024, 400))
            .collect();
        items.push(item("/big/target", 8 * GIB, 20));
        let p = pack(items, 2 * GIB, 40.0, 5);
        assert_eq!(p.items[0].cand.path, Path::new("/big/target"));
        assert!(p.planned_bytes >= 8 * GIB);
    }

    #[test]
    fn pack_orders_by_last_used_not_mtime() {
        // old by mtime, but seen recently → leaves later
        let mut a = item("/x/mtime-velho-seen-novo", 5 * GIB, 300);
        a.last_used = SystemTime::now() - Duration::from_secs(3600);
        let b = item("/x/seen-velho", 5 * GIB, 10);
        let p = pack(vec![a, b], 0, 10.0, 10);
        assert_eq!(p.items[0].cand.path, Path::new("/x/seen-velho"));
    }

    #[test]
    fn hint_by_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("pnpm-lock.yaml"), "").unwrap();
        let nm = tmp.path().join("node_modules");
        std::fs::create_dir_all(&nm).unwrap();
        assert_eq!(regen_hint(&nm), "pnpm install");

        let rs = tempfile::tempdir().unwrap();
        std::fs::write(rs.path().join("Cargo.toml"), "").unwrap();
        std::fs::create_dir_all(rs.path().join("target")).unwrap();
        assert_eq!(regen_hint(&rs.path().join("target")), "cargo build");
    }

    #[test]
    fn age_override_lowers_min_age_and_churn_for_temp_roots() {
        use crate::seen::SeenDb;
        use std::time::Duration;
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("seen.db");
        let cfg = Config {
            age_overrides: [("/tmp/whale".to_string(), 1u64)].into_iter().collect(),
            ..Config::default()
        };
        // longest prefix wins; the 1h absolute floor still applies
        assert_eq!(cfg.age_override_for(Path::new("/tmp/whale/target")), Some(1.0));
        assert_eq!(cfg.age_override_for(Path::new("/tmp/other")), None);
        assert_eq!(
            cfg.age_override_for(Path::new("/tmp/whale/fresh")),
            Some(1.0)
        );

        // a 2-day-old candidate removed 2 days ago: without the override the
        // 7d churn guard blocks it; with the 1d temp guard it is eligible
        let mut seen = SeenDb::load(&db);
        let c_old = cand("/tmp/whale/target", GIB, 2);
        let c_plain = cand("/home/x/plain/target", GIB, 2);
        seen.mark_removed(Path::new("/tmp/whale/target"), SystemTime::now() - Duration::from_secs(2 * 86400));
        seen.mark_removed(Path::new("/home/x/plain/target"), SystemTime::now() - Duration::from_secs(2 * 86400));
        let (items, rejected) = build_seen(
            vec![c_old, c_plain],
            &cfg,
            None,
            &seen,
        );
        assert!(items.iter().any(|i| i.cand.path == Path::new("/tmp/whale/target")),
            "temp override: 2d guard < override → eligible; rejected: {rejected:?}");
        assert!(rejected.iter().any(|r| r.path == Path::new("/home/x/plain/target")
            && r.reason.contains("churn guard")),
            "plain root keeps the 7d guard; {rejected:?}");
    }
}
