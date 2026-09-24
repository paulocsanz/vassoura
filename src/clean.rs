use std::fs;
use std::path::Path;

use crate::ledger::{self, Line};
use crate::plan::PlanItem;

#[derive(Debug, Default)]
pub struct Outcome {
    pub freed: u64,
    pub removed: usize,
    pub skipped: Vec<(std::path::PathBuf, String)>,
}

/// Run the plan with last-instant gates (production: lsof + git,
/// fail-closed — P1.2):
/// 1. the candidate is not in use (file open by a live process /
///    worktree with uncommitted work) — unavailable gates skip;
/// 2. the path still exists and is still a directory (not a symlink);
/// 3. the root mtime is exactly the scan's — if something wrote inside
///    after the scan, eviction is aborted for that item;
/// 4. every successful removal is born with a ledger line.
pub fn apply(items: &[PlanItem], ledger_path: &Path) -> Outcome {
    // One lsof snapshot and one git status per repo, each with a kill
    // timeout. Per-candidate lsof/+D and git status serialized the cycle
    // behind a single wedged command.
    let paths: Vec<&Path> = items.iter().map(|i| i.cand.path.as_path()).collect();
    let gates = crate::gates::prepare(&paths);
    apply_with(items, ledger_path, &|p| gates.check(p))
}

/// `apply` with an injectable in-use predicate (deterministic tests).
pub fn apply_with(
    items: &[PlanItem],
    ledger_path: &Path,
    in_use: &dyn Fn(&Path) -> Option<String>,
) -> Outcome {
    let mut out = Outcome::default();
    for it in items {
        let path = &it.cand.path;
        if let Some(why) = in_use(path) {
            out.skipped.push((path.clone(), why));
            continue;
        }
        let Ok(md) = fs::symlink_metadata(path) else {
            out.skipped.push((path.clone(), "gone since the scan".into()));
            continue;
        };
        if md.is_symlink() || !md.is_dir() {
            out.skipped.push((path.clone(), "no longer a regular directory".into()));
            continue;
        }
        if md.modified().ok() != Some(it.cand.root_mtime) {
            out.skipped.push((path.clone(), "changed since the scan (kept)".into()));
            continue;
        }
        match fs::remove_dir_all(path) {
            Ok(()) => {
                let line = Line {
                    ts: jiff::Timestamp::now().as_second(),
                    iso: crate::fmt_util::now_iso(),
                    action: "rm",
                    class: it.cand.class.label().to_string(),
                    path: path.display().to_string(),
                    bytes: it.cand.bytes,
                    age_days: it.age_days,
                    hint: it.hint.clone(),
                };
                if let Err(e) = ledger::append(ledger_path, &line) {
                    // a removal without a ledger violates the "nothing-without-ledger" refusal:
                    // the directory is already gone; we record the error on stderr.
                    eprintln!("# LEDGER ERROR {}: {e} (item already removed: {})", ledger_path.display(), path.display());
                }
                out.freed += it.cand.bytes;
                out.removed += 1;
            }
            Err(e) => {
                out.skipped.push((path.clone(), format!("rm failed: {e}")));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::{Candidate, Class};
    use std::time::{Duration, SystemTime};

    fn item(path: &std::path::Path, bytes: u64, age_d: u64, newest: SystemTime) -> PlanItem {
        PlanItem {
            cand: Candidate {
                path: path.to_path_buf(),
                class: Class::Artifact,
                bytes,
                newest_mtime: newest,
                root_mtime: newest,
                contains_git: false,
                entries: 1,
            },
            age_days: age_d as f64,
            last_used: newest,
            hint: "pnpm install".into(),
        }
    }

    #[test]
    fn apply_removes_ledgers_and_guards_fresh_touch() {
        let tmp = tempfile::tempdir().unwrap();
        let old_mtime = SystemTime::now() - Duration::from_secs(30 * 86400);
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        for d in [&a, &b] {
            fs::create_dir_all(d).unwrap();
            fs::write(d.join("f.bin"), vec![0u8; 128]).unwrap();
        }
        // simulate ages
        filetime::set_file_times(&a, old_mtime.into(), old_mtime.into()).unwrap();
        filetime::set_file_times(&b, old_mtime.into(), old_mtime.into()).unwrap();

        let ledger = tmp.path().join("ledger.jsonl");
        // item `b` was touched after the scan (mtime diverged)
        let stale_b = SystemTime::now() - Duration::from_secs(60);
        let items = vec![
            item(&a, 200, 30, old_mtime),
            item(&b, 200, 30, stale_b),
        ];
        let noop = |_p: &std::path::Path| None;
        let out = apply_with(&items, &ledger, &noop);

        assert_eq!(out.removed, 1);
        assert!(out.skipped.iter().any(|(p, _)| p == &b), "b must be protected");
        assert!(!a.exists());
        assert!(b.exists(), "b must not be removed");
        let txt = fs::read_to_string(&ledger).unwrap();
        let lines: Vec<&str> = txt.lines().collect();
        assert_eq!(lines.len(), 1);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["path"], a.display().to_string());
        assert_eq!(v["hint"], "pnpm install");
    }

    #[test]
    fn apply_skips_in_use_without_removal_or_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("f.bin"), vec![0u8; 128]).unwrap();
        let old = SystemTime::now() - Duration::from_secs(30 * 86400);
        filetime::set_file_times(&a, old.into(), old.into()).unwrap();
        let it = item(&a, 128, 30, old);
        let ledger = tmp.path().join("ledger.jsonl");

        let gate = |p: &std::path::Path| {
            (p == &a).then(|| "in use: process has a file open".to_string())
        };
        let out = apply_with(std::slice::from_ref(&it), &ledger, &gate);

        assert_eq!(out.removed, 0);
        assert_eq!(out.skipped.len(), 1);
        assert!(out.skipped[0].1.contains("in use"), "{:?}", out.skipped[0]);
        assert!(a.exists(), "in use is not removed");
        assert!(!ledger.exists(), "no removal means no ledger line");
    }

    #[test]
    fn inner_file_newer_than_the_root_is_still_removed() {
        // The scan stores the newest mtime in the tree (age) and the root mtime
        // (re-stat). An inner file newer than the root is not "changed
        // since the scan" — nothing was written between the scan and removal.
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        let nm = proj.join("node_modules");
        fs::create_dir_all(nm.join("dep")).unwrap();
        fs::write(proj.join("package.json"), "{}").unwrap();
        fs::write(nm.join("dep/a.js"), "x").unwrap();
        let old = SystemTime::now() - Duration::from_secs(40 * 86400);
        let newer = SystemTime::now() - Duration::from_secs(20 * 86400);
        filetime::set_file_times(&nm, old.into(), old.into()).unwrap();
        filetime::set_file_times(nm.join("dep"), newer.into(), newer.into()).unwrap();
        filetime::set_file_times(nm.join("dep/a.js"), newer.into(), newer.into()).unwrap();

        let cfg = crate::config::Config {
            artifact_roots: vec![tmp.path().to_path_buf()],
            artifact_names: vec!["node_modules".into()],
            min_age_days_artifacts: 14,
            app_cache_roots: vec![],
            ..crate::config::Config::default()
        };
        let cands = crate::walk::scan(&cfg, None, true);
        assert_eq!(cands.len(), 1);
        assert!(cands[0].newest_mtime > cands[0].root_mtime, "inner file is newer than the root");
        let (items, _) = crate::plan::build(cands, &cfg, None);
        assert_eq!(items.len(), 1);
        let ledger = tmp.path().join("ledger.jsonl");
        let out = apply_with(&items, &ledger, &|_| None);
        assert_eq!(out.removed, 1, "root re-stat must not refuse: {:?}", out.skipped);
        assert!(!nm.exists());
        assert!(ledger.exists());
    }
}
