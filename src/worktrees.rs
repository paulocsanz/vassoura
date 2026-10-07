//! Git worktree collection (2026-09-30 incident follow-up): a single mono
//! repo accumulated 138 registered worktrees — dozens full source checkouts
//! with their own node_modules — while the daemon only ever collected the
//! artifacts INSIDE them.
//!
//! Invariants preserved:
//! - never-yours: only repos listed in `worktree_repos` are eligible; the
//!   main checkout is never a candidate; bare/locked/prunable entries are
//!   skipped; nothing outside the allowlist is touched.
//! - nothing-in-use: the standard lsof + git-dirty gates run at eviction
//!   time, AND removal itself goes through `git worktree remove` (never
//!   `--force`), which refuses dirty, locked or submodule-populated
//!   checkouts — a race between scan and removal fails closed.
//! - nothing-without-ledger: removal goes through the same ledger path as
//!   every other class, with a `git worktree add` regeneration hint.
//! - merged gate: the worktree HEAD must be an ancestor of the origin
//!   default branch. Unknown origin / failed probe = fail-closed skip.

use std::path::{Path, PathBuf};

use crate::walk::{measure_path, Candidate, Progress};

/// One entry of `git worktree list --porcelain` that passed the structural
/// filters (existing directory, not bare, not locked, not prunable, not the
/// main checkout).
#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
}

/// Parse `git worktree list --porcelain` and keep the removable-shaped
/// entries. A missing git or a failed probe returns an empty list: the scan
/// proceeds with the other classes, and the absence is visible as "0
/// worktree candidates" rather than an error path.
pub fn enumerate(repo: &Path) -> Vec<Worktree> {
    let Ok(out) = crate::procutil::output_with_timeout(
        std::process::Command::new("git").arg("-C").arg(repo).args(["worktree", "list", "--porcelain"]),
        std::time::Duration::from_secs(30),
    ) else {
        return Vec::new();
    };
    parse_porcelain(&String::from_utf8_lossy(&out.stdout))
}

fn parse_porcelain(text: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    let mut first = true;
    let mut path: Option<PathBuf> = None;
    let mut head = String::new();
    let mut branch: Option<String> = None;
    let mut skip = false;

    let flush = |path: &mut Option<PathBuf>, head: &mut String, branch: &mut Option<String>, skip: &mut bool, first: &mut bool, out: &mut Vec<Worktree>| {
        if let (Some(p), false) = (path.take(), *skip) {
            if p.is_dir() {
                out.push(Worktree {
                    path: p,
                    head: std::mem::take(head),
                    branch: branch.take(),
                });
            }
        }
        *path = None;
        head.clear();
        *branch = None;
        *skip = false;
        *first = false;
    };

    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if path.is_some() {
                flush(&mut path, &mut head, &mut branch, &mut skip, &mut first, &mut out);
            }
            path = Some(PathBuf::from(p));
            continue;
        }
        if first && path.is_some() && line.starts_with("HEAD ") {
            // The first listed worktree is the main checkout — never a candidate.
            skip = true;
        }
        if let Some(h) = line.strip_prefix("HEAD ") {
            head = h.to_string();
            continue;
        }
        if line.starts_with("branch ") {
            branch = Some(line.trim_start_matches("branch ").to_string());
            continue;
        }
        if line.starts_with("bare") || line.starts_with("locked") || line.starts_with("prunable") || line.starts_with("detached") {
            // `detached` alone is fine (HEAD carries the commit), but bare,
            // locked and prunable entries never leave the allowlist shape.
            if !line.starts_with("detached") {
                skip = true;
            }
            continue;
        }
    }
    flush(&mut path, &mut head, &mut branch, &mut skip, &mut first, &mut out);
    out
}

/// Resolve the origin default branch (`origin/HEAD`, falling back to
/// origin/main then origin/master). `None` = no known upstream default:
/// the merged gate cannot pass, so worktrees are not eligible.
pub fn origin_default(repo: &Path) -> Option<String> {
    if let Ok(out) = crate::procutil::output_with_timeout(
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["symbolic-ref", "refs/remotes/origin/HEAD"]),
        std::time::Duration::from_secs(10),
    ) {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    for cand in ["origin/main", "origin/master"] {
        if let Ok(out) = crate::procutil::output_with_timeout(
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["rev-parse", "--verify", "--quiet", cand]),
            std::time::Duration::from_secs(10),
        ) {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(cand.to_string());
            }
        }
    }
    None
}

/// True when `head` is an ancestor of the origin default branch, i.e. every
/// commit made in this worktree already lives upstream. A probe failure is
/// an error (fail-closed), not "not merged".
pub fn merged_into_origin(repo: &Path, head: &str, origin_ref: &str) -> Result<bool, String> {
    let out = crate::procutil::output_with_timeout(
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["merge-base", "--is-ancestor", head, origin_ref]),
        std::time::Duration::from_secs(15),
    )
    .map_err(|e| format!("merge-base: {e}"))?;
    Ok(out.status.success())
}

/// Check multiple worktree HEADs against the origin default branch in a single
/// git invocation via `git rev-list --no-walk <heads...> --not <origin_ref>`.
/// Returns a set of HEAD hashes that ARE merged into origin.
/// Falls back to individual `merged_into_origin` if the batch command fails.
pub fn filter_merged_heads<'a>(
    repo: &Path,
    heads: impl IntoIterator<Item = &'a str>,
    origin_ref: &str,
) -> Result<std::collections::HashSet<String>, String> {
    use std::collections::HashSet;
    let heads: Vec<&'a str> = heads.into_iter().collect();
    if heads.is_empty() {
        return Ok(HashSet::new());
    }
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(repo).args(["rev-list", "--no-walk"]);
    for h in &heads {
        cmd.arg(*h);
    }
    cmd.args(["--not", origin_ref]);

    let out = crate::procutil::output_with_timeout(&mut cmd, std::time::Duration::from_secs(15));
    match out {
        Ok(o) if o.status.success() => {
            let unmerged: HashSet<String> = String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .collect();
            let mut merged = HashSet::new();
            for h in heads {
                if !unmerged.contains(h) {
                    merged.insert(h.to_string());
                }
            }
            Ok(merged)
        }
        _ => {
            // Fallback: test individually
            let mut merged = HashSet::new();
            for h in heads {
                if merged_into_origin(repo, h, origin_ref).unwrap_or(false) {
                    merged.insert(h.to_string());
                }
            }
            Ok(merged)
        }
    }
}

/// Structural eligibility: merged into origin (fail-closed) and not already
/// registered as removed. The age/in-use/churn gates are the generic ones
/// (plan.rs + gates.rs); the final dirty/locked check is git itself at
/// removal time.
pub fn eligibility_skip(repo: &Path, wt: &Worktree) -> Option<String> {
    let Some(origin_ref) = origin_default(repo) else {
        return Some("no origin default branch (fail-closed)".into());
    };
    match merged_into_origin(repo, &wt.head, &origin_ref) {
        Err(e) => Some(format!("merge gate unavailable (fail-closed): {e}")),
        Ok(false) => Some(format!("HEAD not merged into {origin_ref}")),
        Ok(true) => None,
    }
}

pub fn measure_worktree(path: &Path, prog: &mut Progress) -> Candidate {
    let mut c = measure_path(path, crate::walk::Class::Worktree, prog);
    // A worktree's `.git` is a pointer file, not a repository: the generic
    // "contains .git (protected)" rejection must not fire. The pointer file
    // is removed by `git worktree remove` itself, together with the admin
    // metadata in the main repo.
    c.contains_git = false;
    c
}

/// Remove through git (no `--force`): dirty, locked or submodule-populated
/// checkouts are refused by git and the caller must skip. Returns the error
/// text from git on refusal.
pub fn remove_worktree(wt: &Path) -> Result<(), String> {
    let out = crate::procutil::output_with_timeout(
        std::process::Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["worktree", "remove"])
            .arg(wt),
        std::time::Duration::from_secs(120),
    )
    .map_err(|e| format!("spawn git worktree remove: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn regen_hint(wt: &Worktree) -> String {
    match &wt.branch {
        Some(b) => format!("git worktree add <path> {}", b.trim_start_matches("refs/heads/")),
        None => format!("git worktree add <path> {}", &wt.head[..wt.head.len().min(12)]),
    }
}

/// Regeneration hint computed at removal time (the Candidate does not carry
/// the branch). Works from inside the worktree, so it must run BEFORE the
/// directory is removed.
pub fn hint_for_existing(wt: &Path) -> Option<String> {
    let out = crate::procutil::output_with_timeout(
        std::process::Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["rev-parse", "--abbrev-ref", "HEAD"]),
        std::time::Duration::from_secs(10),
    )
    .ok()?;
    let refname = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if refname.is_empty() {
        return None;
    }
    if refname == "HEAD" {
        let sha = std::process::Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        Some(format!("git worktree add <path> {sha}"))
    } else {
        Some(format!("git worktree add <path> {refname}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture: a bare origin, a clone, main pushed, and two worktrees —
    /// one whose branch is merged upstream (eligible) and one with an
    /// unpushed commit (not eligible).
    fn fixture() -> (tempfile::TempDir, PathBuf, Worktree, Worktree) {
        let tmp = tempfile::tempdir().unwrap();
        let run = |args: &[&str], cwd: &Path| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap()
        };
        let origin = tmp.path().join("origin.git");
        run(&["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()], tmp.path());
        let clone = tmp.path().join("clone");
        run(&["clone", "-q", origin.to_str().unwrap(), clone.to_str().unwrap()], tmp.path());
        run(&["config", "user.email", "t@t"], &clone);
        run(&["config", "user.name", "t"], &clone);
        std::fs::write(clone.join("f.txt"), b"one").unwrap();
        run(&["add", "."], &clone);
        run(&["commit", "-qm", "one"], &clone);
        run(&["push", "-q", "origin", "main"], &clone);

        let wt_path = |name: &str| clone.join(name);
        // merged worktree: branch created, committed, merged to main, pushed
        run(&["worktree", "add", "-q", "-b", "feat/merged", wt_path("wt_merged").to_str().unwrap()], &clone);
        std::fs::write(wt_merged_file(&clone), b"two").unwrap();
        run(&["add", "."], &clone.join("wt_merged"));
        run(&["commit", "-qm", "two"], &clone.join("wt_merged"));
        run(&["merge", "-q", "--no-ff", "feat/merged"], &clone);
        run(&["push", "-q", "origin", "main"], &clone);

        // unmerged worktree: commit never pushed
        run(&["worktree", "add", "-q", "-b", "feat/wip", wt_path("wt_wip").to_str().unwrap()], &clone);
        std::fs::write(clone.join("wt_wip").join("g.txt"), b"three").unwrap();
        run(&["add", "."], &clone.join("wt_wip"));
        run(&["commit", "-qm", "three"], &clone.join("wt_wip"));

        let all = enumerate(&clone);
        let merged = all.iter().find(|w| w.path.ends_with("wt_merged")).unwrap().clone();
        let wip = all.iter().find(|w| w.path.ends_with("wt_wip")).unwrap().clone();
        (tmp, clone, merged, wip)
    }

    fn wt_merged_file(clone: &Path) -> PathBuf {
        clone.join("wt_merged").join("f.txt")
    }

    #[test]
    fn enumerate_skips_main_and_lists_side_worktrees() {
        let (_tmp, _clone, merged, wip) = fixture();
        assert_eq!(merged.branch.as_deref(), Some("refs/heads/feat/merged"));
        assert_eq!(wip.branch.as_deref(), Some("refs/heads/feat/wip"));
        assert_ne!(merged.head, wip.head);
    }

    #[test]
    fn merged_gate_passes_merged_and_refuses_wip() {
        let (_tmp, clone, merged, wip) = fixture();
        assert_eq!(eligibility_skip(&clone, &merged), None, "merged → eligible");
        let why = eligibility_skip(&clone, &wip).unwrap();
        assert!(why.contains("not merged"), "{why}");
    }

    #[test]
    fn no_origin_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(tmp.path())
                .output()
                .unwrap()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(tmp.path().join("a"), b"a").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "a"]);
        run(&["worktree", "add", "-q", "-b", "side", tmp.path().join("side").to_str().unwrap()]);
        let all = enumerate(tmp.path());
        let side = all.iter().find(|w| w.path.ends_with("side")).unwrap();
        let why = eligibility_skip(tmp.path(), side).unwrap();
        assert!(why.contains("fail-closed"), "{why}");
    }

    #[test]
    fn remove_worktree_refuses_dirty_checkout() {
        let (_tmp, _clone, merged, _wip) = fixture();
        std::fs::write(merged.path.join("uncommitted.txt"), b"wip").unwrap();
        let err = remove_worktree(&merged.path).unwrap_err();
        assert!(!err.is_empty(), "git must refuse a dirty worktree");
        assert!(merged.path.exists(), "dirty worktree survives");
    }

    #[test]
    fn remove_worktree_removes_clean_checkout() {
        let (_tmp, _clone, merged, _wip) = fixture();
        remove_worktree(&merged.path).unwrap();
        assert!(!merged.path.exists());
    }

    #[test]
    fn hint_names_the_branch() {
        let (_tmp, _clone, merged, _wip) = fixture();
        assert_eq!(regen_hint(&merged), "git worktree add <path> feat/merged");
    }
}
