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

/// Real git: a candidate inside a worktree with uncommitted work → skipped.
/// A clean worktree (or not a repo) stays eligible.
pub fn git_state(path: &Path) -> UseState {
    let Some(repo) = nearest_repo(path) else {
        return UseState::Free;
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| format!("spawn git: {e}"));
    match out {
        Err(e) => UseState::Unavailable(e),
        Ok(o) => {
            let dirty = !String::from_utf8_lossy(&o.stdout).trim().is_empty();
            if dirty {
                UseState::InUse(format!("worktree has uncommitted work ({})", repo.display()))
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
}
