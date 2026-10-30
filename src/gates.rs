//! Use gates (P1.2): a candidate is removed only if NO live process has a
//! file open under it (real lsof) and the git worktree that contains it is
//! clean. A missing or unavailable gate fails CLOSED: the candidate is
//! skipped, never removed blindly.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::walk::{Candidate, Class};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseState {
    /// Nothing in use: the candidate stays eligible.
    Free,
    /// In use (human-readable reason): skip.
    InUse(String),
    /// Gate unavailable (missing binary/error): fail-closed → skip.
    Unavailable(String),
}

fn lsof_probe_active(args: &[&OsStr]) -> Result<bool, String> {
    let out = Command::new("lsof")
        .args(args)
        .output()
        .map_err(|e| format!("spawn lsof: {e}"))?;
    // lsof's exit code varies (1 even when it found something); parse -F cftn stdout.
    let (paths, vm_paths) = parse_lsof_cftn(&String::from_utf8_lossy(&out.stdout));
    Ok(!paths.is_empty() || !vm_paths.is_empty())
}

/// Real lsof: a live process with a file open UNDER the directory (+D,
/// recursive) or with the directory itself open as cwd (`fcwd`).
/// Passive `O_EVTONLY` / `opendir` directory descriptors (`tDIR` with `fd != cwd`)
/// from IDE/agent file watchers do not count as active use.
pub fn lsof_state(path: &Path) -> UseState {
    let p = path.as_os_str();
    let under = ["-n", "-P", "-w", "-F", "cftn", "+D"]
        .iter()
        .map(|s| OsStr::new(*s))
        .chain(std::iter::once(p));
    match lsof_probe_active(&under.collect::<Vec<_>>()) {
        Ok(true) => return UseState::InUse("process has a file open (lsof +D)".into()),
        Err(e) => return UseState::Unavailable(e),
        Ok(false) => {}
    }
    let itself = ["-n", "-P", "-w", "-F", "cftn", "--"]
        .iter()
        .map(|s| OsStr::new(*s))
        .chain(std::iter::once(p));
    match lsof_probe_active(&itself.collect::<Vec<_>>()) {
        Ok(true) => UseState::InUse("process has the directory open (lsof cwd/fd)".into()),
        Err(e) => UseState::Unavailable(e),
        Ok(false) => UseState::Free,
    }
}

/// Nearest git repo above the candidate (dir OR file — the worktree).
/// If `path` itself is a worktree checkout (contains `.git`), `path` is the repo.
pub fn nearest_repo(path: &Path) -> Option<PathBuf> {
    if path.join(".git").exists() {
        return Some(path.to_path_buf());
    }
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

pub(crate) fn is_ignored_dirty_path(path: &Path) -> bool {
    if is_artifact_path(path) {
        return true;
    }
    use crate::config::PRUNE_DIRS;
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        PRUNE_DIRS.contains(&name.as_ref())
    })
}

fn untracked_dir_has_user_files(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    for entry in rd.flatten() {
        let p = entry.path();
        if is_ignored_dirty_path(&p) {
            continue;
        }
        if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            if untracked_dir_has_user_files(&p) {
                return true;
            }
        } else {
            return true;
        }
    }
    false
}

pub(crate) fn parse_git_dirty_files(repo: &Path, stdout: &[u8]) -> Vec<PathBuf> {
    let s = String::from_utf8_lossy(stdout);
    let mut dirty = Vec::new();
    for line in s.lines() {
        let Some(p_str) = parse_dirty_path(line) else { continue };
        let rel = Path::new(p_str);
        if is_ignored_dirty_path(rel) {
            continue;
        }
        let full = repo.join(rel);
        if line.starts_with("??") && full.is_dir() && !untracked_dir_has_user_files(&full) {
            continue;
        }
        dirty.push(full);
    }
    dirty
}

pub(crate) fn is_project_dirty(repo: &Path, candidate_path: &Path, dirty_files: &[PathBuf]) -> bool {
    if dirty_files.is_empty() {
        return false;
    }
    let project = if candidate_path == repo {
        repo
    } else {
        candidate_path.parent().unwrap_or(candidate_path)
    };
    if project == repo {
        return true;
    }
    dirty_files.iter().any(|df| df.starts_with(project))
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
        .args(["status", "--porcelain", "-unormal"])
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
fn is_vm_hypervisor(cmd: &str) -> bool {
    cmd.starts_with("com.apple.Virtualization.") || cmd.starts_with("qemu-") || cmd == "virtiofsd"
}

fn is_transient_search_or_indexer(cmd: &str) -> bool {
    let base = cmd.rsplit('/').next().unwrap_or(cmd);
    matches!(
        base,
        "grep" | "egrep" | "fgrep" | "rg" | "ripgrep" | "ag" | "ack" | "find" | "bfs" |
        "fd" | "fdfind" | "du" | "ncdu" | "dust" | "tree" | "ls" | "exa" | "eza" |
        "mdfind" | "mds" | "mdworker" | "mdworker_shared" | "fseventsd" | "watchman" |
        "file" | "stat" | "cat" | "head" | "tail" | "wc" | "less" | "more" |
        "strings" | "nm" | "objdump" | "otool"
    )
}

/// Passive directory descriptors (`tDIR` with `fd != "cwd"` and `fd != "rtd"`):
/// IDE / agent file watchers (`claude`, `Cursor`, `Code`, Chokidar `kqueue`) open
/// thousands of `O_EVTONLY` / `O_RDONLY` directory handles (`f*000..*999`, `tDIR`)
/// across `node_modules/`, `.turbo/`, and `target/`. Unlike `fcwd` (active working
/// directory) or `tREG`/`ftxt` (open files, compiler locks, mapped binaries), passive
/// `DIR` fds cannot write or execute anything and cleanly close on `NOTE_DELETE`.
pub(crate) fn is_passive_dir_fd(fd: &str, ftype: &str) -> bool {
    ftype == "DIR" && fd != "cwd" && fd != "rtd"
}

pub(crate) fn parse_lsof_cftn(stdout: &str) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut paths = Vec::new();
    let mut vm_paths = Vec::new();
    let mut is_vm = false;
    let mut is_transient = false;
    let mut fd = "";
    let mut ftype = "";
    for line in stdout.lines() {
        if let Some(cmd) = line.strip_prefix('c') {
            let cmd_name = cmd.trim();
            is_vm = is_vm_hypervisor(cmd_name);
            is_transient = is_transient_search_or_indexer(cmd_name);
            fd = "";
            ftype = "";
            continue;
        }
        if let Some(f) = line.strip_prefix('f') {
            fd = f.trim();
            ftype = "";
            continue;
        }
        if let Some(t) = line.strip_prefix('t') {
            ftype = t.trim();
            continue;
        }
        let Some(rest) = line.strip_prefix('n') else { continue };
        if is_transient || is_passive_dir_fd(fd, ftype) {
            continue;
        }
        if rest.starts_with('/') {
            let p = PathBuf::from(rest);
            if is_vm {
                vm_paths.push(p);
            } else {
                paths.push(p);
            }
        }
    }
    (paths, vm_paths)
}

pub struct GateSession {
    git: std::collections::HashMap<PathBuf, Result<Vec<PathBuf>, String>>,
    open_paths: Vec<PathBuf>,
    vm_open_paths: Vec<PathBuf>,
    lsof_err: Option<String>,
}

pub fn prepare(paths: &[&Path]) -> GateSession {
    use std::collections::HashSet;
    let repos: HashSet<PathBuf> = paths.iter().filter_map(|p| nearest_repo(p)).collect();
    let git = git_many(repos);
    let ((open_paths, vm_open_paths), lsof_err) = match lsof_snapshot() {
        Ok(res) => (res, None),
        Err(e) => match lsof_batch_cwd_timeout(paths) {
            Ok(batch_paths) => ((batch_paths, Vec::new()), None),
            Err(batch_err) => ((Vec::new(), Vec::new()), Some(format!("{e}; batch probe: {batch_err}"))),
        },
    };
    GateSession { git, open_paths, vm_open_paths, lsof_err }
}

impl GateSession {
    /// Populate git status for any additional candidate paths introduced after `prepare`
    /// (e.g. Cargo `target/` sub-artifact salvage candidates).
    pub fn ensure_repos(&mut self, paths: &[&Path]) {
        use std::collections::HashSet;
        let missing: HashSet<PathBuf> = paths
            .iter()
            .filter_map(|p| nearest_repo(p))
            .filter(|r| !self.git.contains_key(r))
            .collect();
        if !missing.is_empty() {
            self.git.extend(git_many(missing));
        }
    }

    /// True when Cargo is actively holding `.cargo-lock` or `.cargo-build-lock` open
    /// anywhere inside `target_dir` (meaning an active `cargo` command is running).
    pub fn has_cargo_lock_open(&self, target_dir: &Path) -> bool {
        let is_lock = |p: &Path| {
            matches!(
                p.file_name().and_then(|n| n.to_str()),
                Some(".cargo-lock" | ".cargo-build-lock")
            )
        };
        if self.open_paths.iter().any(|p| p.starts_with(target_dir) && is_lock(p)) {
            return true;
        }
        if let Ok(canon) = std::fs::canonicalize(target_dir) {
            if canon != target_dir
                && self.open_paths.iter().any(|p| p.starts_with(&canon) && is_lock(p))
            {
                return true;
            }
        }
        false
    }

    pub fn check(&self, path: &Path) -> Option<String> {
        self.check_candidate_policy(path, Class::Artifact, false)
    }

    pub fn check_candidate(&self, cand: &Candidate, emergency: bool) -> Option<String> {
        self.check_candidate_policy(&cand.path, cand.class, emergency)
    }

    pub fn check_candidate_policy(&self, path: &Path, class: Class, emergency: bool) -> Option<String> {
        if open_under(&self.open_paths, path) {
            return Some("in use: process has a file open (lsof)".into());
        }
        if !emergency && open_under(&self.vm_open_paths, path) {
            return Some("in use: VM hypervisor has a file open (virtiofs)".into());
        }
        // In an emergency (disk >= 95% full or forced cycle), OS survival
        // takes priority over incremental build caches: pure regenerable artifacts
        // (target, node_modules, .venv, etc.) and app caches whose files are not held open
        // by any process can be evicted even if the containing repo has uncommitted edits,
        // or if system-wide lsof timed out under extreme pressure.
        // Class::Worktree checkouts NEVER bypass this: a dirty worktree checkout contains
        // uncommitted user source code and must never be deleted!
        if emergency && class != Class::Worktree {
            return None;
        }
        if let Some(err) = &self.lsof_err {
            return Some(format!("in use unavailable (fail-closed): {err}"));
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
    // Bound concurrency to 2 threads to prevent I/O thrashing on large repos
    for (chunk_repos, chunk_slots) in repos.chunks(2).zip(slots.chunks_mut(2)) {
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
        Command::new("git").arg("-C").arg(repo).args(["status", "--porcelain", "-unormal"]),
        std::time::Duration::from_secs(90),
    )?;
    Ok(parse_git_dirty_files(repo, &out.stdout))
}

/// One `lsof` for every open file. Killed if it does not finish: the cycle
/// then falls back to a single batch check on the candidate paths.
fn lsof_snapshot() -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let out = crate::procutil::output_with_timeout(
        Command::new("lsof").args(["-n", "-P", "-w", "-F", "cftn"]),
        std::time::Duration::from_secs(45),
    )?;
    Ok(parse_lsof_cftn(&String::from_utf8_lossy(&out.stdout)))
}

fn lsof_batch_cwd_timeout(paths: &[&Path]) -> Result<Vec<PathBuf>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut opens = Vec::new();
    for chunk in paths.chunks(100) {
        let mut cmd = Command::new("lsof");
        cmd.args(["-n", "-P", "-w", "-F", "cftn", "--"]);
        for p in chunk {
            cmd.arg(p);
        }
        let out = crate::procutil::output_with_timeout(&mut cmd, std::time::Duration::from_secs(15))?;
        let (chunk_paths, _) = parse_lsof_cftn(&String::from_utf8_lossy(&out.stdout));
        opens.extend(chunk_paths);
    }
    Ok(opens)
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
        git(&["config", "commit.gpgsign", "false"]);
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
        git(&["config", "commit.gpgsign", "false"]);
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
        git(&["config", "commit.gpgsign", "false"]);
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

        // Untracked file in the repo root (e.g. notes.txt or .cleanup-plan.md) must NOT
        // block an independent subproject.
        std::fs::write(repo.join("notes.txt"), b"scratchpad").unwrap();
        let session2 = prepare(&[&cand_b]);
        assert!(session2.check(&cand_b).is_none(), "pkg_b target stays clean even with untracked root notes");

        // Scratch files in .claude or .pr-worktrees must NOT block clean subprojects.
        let claude_scratch = repo.join(".claude/scratch/test.py");
        std::fs::create_dir_all(claude_scratch.parent().unwrap()).unwrap();
        std::fs::write(&claude_scratch, b"test").unwrap();
        let session3 = prepare(&[&cand_b]);
        assert!(session3.check(&cand_b).is_none(), "pkg_b target stays clean with files in PRUNE_DIRS");
    }

    #[test]
    fn git_gate_worktree_candidate_checks_own_worktree_status() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("main_repo");
        let wt = tmp.path().join("side_wt");
        std::fs::create_dir_all(&repo).unwrap();

        let git_repo = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
        };
        git_repo(&["init", "-q"]);
        git_repo(&["config", "user.email", "t@t"]);
        git_repo(&["config", "user.name", "t"]);
        git_repo(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), b"hi").unwrap();
        git_repo(&["add", "."]);
        git_repo(&["commit", "-qm", "init"]);
        git_repo(&["worktree", "add", "-q", wt.to_str().unwrap()]);

        // Main repo has an untracked file, but worktree `wt` is clean
        std::fs::write(repo.join("dirty_in_main.txt"), b"dirty").unwrap();

        let session = prepare(&[&wt]);
        assert!(session.check(&wt).is_none(), "clean worktree candidate is free even if main repo has untracked files");

        // Now dirty work occurs inside the worktree
        std::fs::write(wt.join("uncommitted_in_wt.txt"), b"dirty in wt").unwrap();
        let session_dirty = prepare(&[&wt]);
        assert!(session_dirty.check(&wt).is_some(), "dirty worktree candidate is in-use/skipped");
    }

    #[test]
    fn git_gate_emergency_bypasses_dirty_repo_for_artifacts_but_protects_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let target = repo.join("target");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&wt).unwrap();

        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("Cargo.toml"), b"[package]").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        git(&["worktree", "add", "-q", wt.to_str().unwrap()]);

        // Uncommitted changes in repo and worktree
        std::fs::write(repo.join("src.rs"), b"// dirty").unwrap();
        std::fs::write(wt.join("wt.rs"), b"// dirty wt").unwrap();

        let art_cand = Candidate {
            path: target.clone(),
            class: Class::Artifact,
            bytes: 1024,
            newest_mtime: std::time::SystemTime::now(),
            root_mtime: std::time::SystemTime::now(),
            contains_git: false,
            entries: 10,
        };
        let wt_cand = Candidate {
            path: wt.clone(),
            class: Class::Worktree,
            bytes: 2048,
            newest_mtime: std::time::SystemTime::now(),
            root_mtime: std::time::SystemTime::now(),
            contains_git: false,
            entries: 5,
        };

        let session = prepare(&[&target, &wt]);

        // In normal non-emergency, both are skipped due to uncommitted work
        assert!(session.check_candidate(&art_cand, false).is_some(), "artifact skipped in normal mode when repo is dirty");
        assert!(session.check_candidate(&wt_cand, false).is_some(), "worktree skipped in normal mode when wt is dirty");

        // In emergency mode, artifact is ALLOWED (bypass dirty repo), but worktree is STILL REFUSED!
        assert!(session.check_candidate(&art_cand, true).is_none(), "artifact allowed in emergency even if repo is dirty");
        assert!(session.check_candidate(&wt_cand, true).is_some(), "worktree NEVER removed in emergency if dirty");
    }

    #[test]
    fn vm_hypervisor_paths_skipped_in_normal_mode_but_bypassed_in_emergency() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&target).unwrap();

        let cand = Candidate {
            path: target.clone(),
            class: Class::Artifact,
            bytes: 1024 * 1024,
            newest_mtime: std::time::SystemTime::now(),
            root_mtime: std::time::SystemTime::now(),
            contains_git: false,
            entries: 10,
        };

        let session = GateSession {
            git: std::collections::HashMap::new(),
            open_paths: Vec::new(),
            vm_open_paths: vec![target.join("debug/libfoo.rlib")],
            lsof_err: None,
        };

        // Normal mode: skipped as held by VM virtiofs
        let normal_skip = session.check_candidate(&cand, false);
        assert!(normal_skip.is_some(), "normal mode must skip VM held files");
        assert!(normal_skip.unwrap().contains("VM hypervisor"));

        // Emergency mode: bypassed so host disk does not fill to 0 bytes
        let emergency_skip = session.check_candidate(&cand, true);
        assert!(emergency_skip.is_none(), "emergency mode must bypass VM held files");
    }

    #[test]
    fn transient_search_and_indexers_are_recognized() {
        assert!(is_transient_search_or_indexer("grep"));
        assert!(is_transient_search_or_indexer("/usr/bin/grep"));
        assert!(is_transient_search_or_indexer("rg"));
        assert!(is_transient_search_or_indexer("find"));
        assert!(is_transient_search_or_indexer("bfs"));
        assert!(is_transient_search_or_indexer("du"));
        assert!(is_transient_search_or_indexer("mdworker"));
        assert!(!is_transient_search_or_indexer("rustc"));
        assert!(!is_transient_search_or_indexer("cargo"));
        assert!(!is_transient_search_or_indexer("node"));
    }

    #[test]
    fn parse_lsof_ignores_passive_dir_watchers_but_keeps_cwd_and_open_files() {
        // Real macOS lsof -F cftn shape from 2026-10-30 incident:
        // - `claude` holds thousands of `f*000..*999` + `tDIR` kqueue watches on node_modules & target
        // - `zsh` holds `fcwd` + `tDIR` inside an active shell directory
        // - `caixote-api` holds `ftxt` + `tREG` on target/debug/caixote-api
        // - `cargo` holds `f8` + `tREG` on target/debug/.cargo-lock
        let sample = "\
cclaude\n\
f*000\n\
tDIR\n\
n/Users/paulo/software/bosque/node_modules/.pnpm/next\n\
f*001\n\
tDIR\n\
n/Users/paulo/software/substrato/target/debug/build\n\
czsh\n\
fcwd\n\
tDIR\n\
n/Users/paulo/software/active_shell/target\n\
ccaixote-api\n\
ftxt\n\
tREG\n\
n/Users/paulo/software/caixote/target/debug/caixote-api\n\
ccargo\n\
f8\n\
tREG\n\
n/Users/paulo/software/forja/engine/target/debug/.cargo-lock\n\
ccom.apple.Virtualization.VirtualMachine\n\
f42\n\
tREG\n\
n/Users/paulo/software/vm_guest/target/debug/libfoo.rlib\n";

        let (paths, vm_paths) = parse_lsof_cftn(sample);
        assert!(
            !paths.iter().any(|p| p.starts_with("/Users/paulo/software/bosque")),
            "passive claude kqueue DIR watches must not block eviction: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.starts_with("/Users/paulo/software/substrato")),
            "passive DIR watches on target subdirs must not block eviction: {paths:?}"
        );
        assert!(
            paths.contains(&PathBuf::from("/Users/paulo/software/active_shell/target")),
            "process cwd (fcwd + tDIR) MUST block eviction: {paths:?}"
        );
        assert!(
            paths.contains(&PathBuf::from("/Users/paulo/software/caixote/target/debug/caixote-api")),
            "running binary (ftxt + tREG) MUST be recorded: {paths:?}"
        );
        assert!(
            paths.contains(&PathBuf::from("/Users/paulo/software/forja/engine/target/debug/.cargo-lock")),
            "cargo lock (tREG) MUST be recorded: {paths:?}"
        );
        assert_eq!(
            vm_paths,
            vec![PathBuf::from("/Users/paulo/software/vm_guest/target/debug/libfoo.rlib")]
        );
    }

    #[test]
    fn cargo_lock_detection_distinguishes_running_daemon_from_active_cargo_build() {
        let daemon_only = GateSession {
            git: std::collections::HashMap::new(),
            open_paths: vec![PathBuf::from("/p/caixote/target/debug/caixote-api")],
            vm_open_paths: Vec::new(),
            lsof_err: None,
        };
        assert!(
            !daemon_only.has_cargo_lock_open(Path::new("/p/caixote/target")),
            "running binary without .cargo-lock allows intermediate deps/incremental salvage"
        );

        let active_cargo = GateSession {
            git: std::collections::HashMap::new(),
            open_paths: vec![
                PathBuf::from("/p/caixote/target/debug/caixote-api"),
                PathBuf::from("/p/caixote/target/debug/.cargo-lock"),
            ],
            vm_open_paths: Vec::new(),
            lsof_err: None,
        };
        assert!(
            active_cargo.has_cargo_lock_open(Path::new("/p/caixote/target")),
            "active .cargo-lock forbids any salvage inside target/"
        );
    }
}
