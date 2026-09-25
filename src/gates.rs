//! Use gates (P1.2): a candidate is removed only if NO live process has a
//! file open under it (real lsof) and the git worktree that contains it is
//! clean. A missing or unavailable gate fails CLOSED: the candidate is
//! skipped, never removed blindly.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseState {
    /// Nothing in use: the candidate stays eligible.
    Free,
    /// In use (human-readable reason): skip.
    InUse(String),
    /// Gate unavailable (missing binary/error): fail-closed → skip.
    Unavailable(String),
}

fn lsof_probe(args: &[&OsStr]) -> Result<bool, String> {
    let out = Command::new("lsof")
        .args(args)
        .output()
        .map_err(|e| format!("spawn lsof: {e}"))?;
    // lsof's exit code varies (1 even when it found something); the signal is stdout
    // (-F p prints one "p<pid>" line per process).
    Ok(!out.stdout.is_empty())
}

/// Real lsof: a live process with a file open UNDER the directory (+D,
/// recursive) or with the directory itself open (cwd/fd).
pub fn lsof_state(path: &Path) -> UseState {
    let p = path.as_os_str();
    let under = ["-w", "-F", "p", "+D"].iter().map(|s| OsStr::new(*s)).chain(std::iter::once(p));
    match lsof_probe(&under.collect::<Vec<_>>()) {
        Ok(true) => return UseState::InUse("process has a file open (lsof +D)".into()),
        Err(e) => return UseState::Unavailable(e),
        Ok(false) => {}
    }
    let itself = ["-w", "-F", "p", "--"].iter().map(|s| OsStr::new(*s)).chain(std::iter::once(p));
    match lsof_probe(&itself.collect::<Vec<_>>()) {
        Ok(true) => UseState::InUse("process has the directory open (lsof cwd/fd)".into()),
        Err(e) => UseState::Unavailable(e),
        Ok(false) => UseState::Free,
    }
}

/// Nearest git repo above the candidate (dir OR file — the worktree).
pub fn nearest_repo(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|a| a.join(".git").exists())
        .map(|a| a.to_path_buf())
}

pub(crate) fn parse_dirty_path(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.len() < 3 {
        return None;
    }
    let path_part = trimmed[2..].trim();
    let actual_path = if let Some((_, new)) = path_part.split_once(" -> ") {
        new.trim()
    } else {
        path_part
    };
    Some(actual_path.trim_matches('"'))
}

pub(crate) fn is_artifact_path(path: &Path) -> bool {
    use crate::config::DEFAULT_ARTIFACT_NAMES;
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        DEFAULT_ARTIFACT_NAMES.contains(&name.as_ref())
    })
}

pub(crate) fn parse_git_dirty_files(repo: &Path, stdout: &[u8]) -> Vec<PathBuf> {
    let s = String::from_utf8_lossy(stdout);
    let mut dirty = Vec::new();
    for line in s.lines() {
        let Some(p_str) = parse_dirty_path(line) else { continue };
        let rel = Path::new(p_str);
        if is_artifact_path(rel) {
            continue;
        }
        dirty.push(repo.join(rel));
    }
    dirty
}

pub(crate) fn is_project_dirty(repo: &Path, candidate_path: &Path, dirty_files: &[PathBuf]) -> bool {
    if dirty_files.is_empty() {
        return false;
    }
    let project = candidate_path.parent().unwrap_or(candidate_path);
    if project == repo {
        return true;
    }
    dirty_files.iter().any(|df| {
        df.starts_with(project) || df.parent() == Some(repo)
    })
}

/// Real git: a candidate inside a worktree with uncommitted work → skipped.
/// A clean worktree (or not a repo) stays eligible.
pub fn git_state(path: &Path) -> UseState {
    let Some(repo) = nearest_repo(path) else {
        return UseState::Free;
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["status", "--porcelain", "-uall"])
        .output()
        .map_err(|e| format!("spawn git: {e}"));
    match out {
        Err(e) => UseState::Unavailable(e),
        Ok(o) => {
            let dirty = parse_git_dirty_files(&repo, &o.stdout);
            if is_project_dirty(&repo, path, &dirty) {
                let project = path.parent().unwrap_or(path);
                UseState::InUse(format!("worktree has uncommitted work ({})", project.display()))
            } else {
                UseState::Free
            }
        }
    }
}

fn skip_of(state: UseState, gate: &str) -> Option<String> {
    match state {
        UseState::Free => None,
        UseState::InUse(why) => Some(format!("{gate}: {why}")),
        UseState::Unavailable(e) => Some(format!("{gate} unavailable (fail-closed): {e}")),
    }
}

/// Production gate used by `clean --apply` and the daemon:
/// `Some(reason)` = skip (no removal, no ledger).
pub fn in_use(path: &Path) -> Option<String> {
    skip_of(lsof_state(path), "in use")
        .or_else(|| skip_of(git_state(path), "in use"))
}

/// Gates for a whole plan: git status runs once per repo (in parallel),
/// lsof runs once for the machine. A command that does not return is killed
/// and fails closed for that repo or that snapshot only.
pub struct GateSession {
    git: std::collections::HashMap<PathBuf, Result<Vec<PathBuf>, String>>,
    open_paths: Vec<PathBuf>,
    lsof_ok: bool,
}

pub fn prepare(paths: &[&Path]) -> GateSession {
    use std::collections::HashSet;
    let repos: HashSet<PathBuf> = paths.iter().filter_map(|p| nearest_repo(p)).collect();
    let git = git_many(repos);
    let (lsof_ok, open_paths) = lsof_snapshot();
    GateSession { git, open_paths, lsof_ok }
}

impl GateSession {
    pub fn check(&self, path: &Path) -> Option<String> {
        if !self.lsof_ok {
            match lsof_cwd_timeout(path) {
                Ok(true) => return Some("in use: process has the directory open (lsof cwd/fd)".into()),
                Ok(false) => {}
                Err(e) => return Some(format!("in use unavailable (fail-closed): {e}")),
            }
        } else if open_under(&self.open_paths, path) {
            return Some("in use: process has a file open (lsof)".into());
        }
        if let Some(repo) = nearest_repo(path) {
            if let Some(res) = self.git.get(&repo) {
                match res {
                    Err(e) => return Some(format!("in use unavailable (fail-closed): {e}")),
                    Ok(dirty) => {
                        if is_project_dirty(&repo, path, dirty) {
                            let project = path.parent().unwrap_or(path);
                            return Some(format!("in use: worktree has uncommitted work ({})", project.display()));
                        }
                    }
                }
            }
        }
        None
    }
}

fn git_many(repos: std::collections::HashSet<PathBuf>) -> std::collections::HashMap<PathBuf, Result<Vec<PathBuf>, String>> {
    let repos: Vec<PathBuf> = repos.into_iter().collect();
    let mut slots = vec![Ok(Vec::new()); repos.len()];
    // Bound concurrency to 4 threads to prevent I/O thrashing on large repos
    for (chunk_repos, chunk_slots) in repos.chunks(4).zip(slots.chunks_mut(4)) {
        std::thread::scope(|scope| {
            for (repo, slot) in chunk_repos.iter().zip(chunk_slots.iter_mut()) {
                scope.spawn(|| {
                    *slot = git_dirty_files_timeout(repo);
                });
            }
        });
    }
    repos.into_iter().zip(slots).collect()
}

fn git_dirty_files_timeout(repo: &Path) -> Result<Vec<PathBuf>, String> {
    let out = crate::procutil::output_with_timeout(
        Command::new("git").arg("-C").arg(repo).args(["status", "--porcelain", "-uall"]),
        std::time::Duration::from_secs(60),
    )?;
    Ok(parse_git_dirty_files(repo, &out.stdout))
}

/// One `lsof` for every open file. Killed if it does not finish: the cycle
/// then falls back to a per-directory cwd check, still with a timeout.
fn lsof_snapshot() -> (bool, Vec<PathBuf>) {
    let out = crate::procutil::output_with_timeout(
        Command::new("lsof").args(["-n", "-P", "-F", "n"]),
        std::time::Duration::from_secs(30),
    );
    let Ok(out) = out else {
        return (false, Vec::new());
    };
    let mut paths = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // -F n lines are "n" + the path. Skip the field marker.
        let Some(rest) = line.strip_prefix('n') else { continue };
        if rest.starts_with('/') {
            paths.push(PathBuf::from(rest));
        }
    }
    (true, paths)
}

fn lsof_cwd_timeout(path: &Path) -> Result<bool, String> {
    let out = crate::procutil::output_with_timeout(
        Command::new("lsof").args(["-w", "-F", "p", "--"]).arg(path),
        std::time::Duration::from_secs(3),
    )?;
    Ok(!out.stdout.is_empty())
}

fn open_under(opens: &[PathBuf], dir: &Path) -> bool {
    if opens.iter().any(|p| p == dir || p.starts_with(dir)) {
        return true;
    }
    // lsof returns the resolved path (`/private/tmp` for `/tmp`,
    // `/private/var/folders` for `/var/folders`). Compare both forms if canon diverges.
    if let Ok(canon) = std::fs::canonicalize(dir) {
        if canon != dir {
            return opens.iter().any(|p| p == &canon || p.starts_with(&canon));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsof_free_on_quiet_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f"), b"x").unwrap();
        assert_eq!(lsof_state(tmp.path()), UseState::Free);
    }

    #[test]
    fn lsof_catches_open_file_below() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("dep")).unwrap();
        let f = tmp.path().join("dep/a.bin");
        std::fs::write(&f, b"x").unwrap();
        let mut hold = std::process::Command::new("python3")
            .arg("-c")
            .arg(format!(
                "f=open({:?},'rb'); import time; time.sleep(8)",
                f.display().to_string()
            ))
            .spawn()
            .expect("python3 to hold a file open");
        std::thread::sleep(std::time::Duration::from_millis(800));
        let st = lsof_state(tmp.path());
        let _ = hold.kill();
        let _ = hold.wait();
        assert!(matches!(st, UseState::InUse(_)), "expected InUse, got {st:?}");
    }

    #[test]
    fn git_gate_non_repo_is_free_dirty_is_in_use_clean_is_free() {
        let tmp = tempfile::tempdir().unwrap();
        let cand = tmp.path().join("node_modules");
        std::fs::create_dir_all(&cand).unwrap();
        assert_eq!(git_state(&cand), UseState::Free, "no repo → eligible");

        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(tmp.path().join("package.json"), b"{}").unwrap();
        assert!(
            matches!(git_state(&cand), UseState::InUse(_)),
            "uncommitted → skipped"
        );
        git(&["add", "."]);
        git(&["commit", "-qm", "x"]);
        assert_eq!(git_state(&cand), UseState::Free, "clean → eligible");
    }

    #[test]
    fn unavailable_gate_fails_closed_in_combined() {
        // The production fail-closed path is the composition in `in_use`; here the
        // Unavailable → skipped mapping is checked directly.
        let why = skip_of(UseState::Unavailable("boom".into()), "in use").unwrap();
        assert!(why.contains("fail-closed"), "{why}");
        assert!(skip_of(UseState::Free, "in use").is_none());
    }

    #[test]
    fn git_gate_ignores_modifications_inside_artifact_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let cand = tmp.path().join("target");
        std::fs::create_dir_all(&cand).unwrap();

        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(tmp.path().join("Cargo.toml"), b"[package]").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        // A tracked or untracked file modified inside `target` or `__pycache__`
        // should NOT mark the project dirty.
        let pyc = tmp.path().join("scripts/__pycache__/mod.pyc");
        std::fs::create_dir_all(pyc.parent().unwrap()).unwrap();
        std::fs::write(&pyc, b"compiled").unwrap();
        assert_eq!(git_state(&cand), UseState::Free, "artifact changes are ignored");
    }

    #[test]
    fn git_gate_monorepo_dirty_subproject_does_not_block_clean_subproject() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let pkg_a = repo.join("packages/pkg_a");
        let pkg_b = repo.join("packages/pkg_b");
        std::fs::create_dir_all(pkg_a.join("target")).unwrap();
        std::fs::create_dir_all(pkg_b.join("target")).unwrap();

        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("Cargo.toml"), b"[workspace]").unwrap();
        std::fs::write(pkg_a.join("Cargo.toml"), b"[package]").unwrap();
        std::fs::write(pkg_b.join("Cargo.toml"), b"[package]").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        // Dirty work in pkg_a:
        std::fs::write(pkg_a.join("wip.rs"), b"// uncommitted").unwrap();

        let cand_a = pkg_a.join("target");
        let cand_b = pkg_b.join("target");

        let session = prepare(&[&cand_a, &cand_b]);
        assert!(session.check(&cand_a).is_some(), "pkg_a target should be skipped because pkg_a is dirty");
        assert!(session.check(&cand_b).is_none(), "pkg_b target should be clean because pkg_b has no uncommitted work");
    }
}
