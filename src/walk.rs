use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::{expand, Config, PRUNE_DIRS};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Artifact,
    AppCache,
}

impl Class {
    pub fn label(self) -> &'static str {
        match self {
            Class::Artifact => "artifact",
            Class::AppCache => "app-cache",
        }
    }
}

/// A cataloged regenerable directory: how much it occupies, when it was last
/// written (newest mtime found in the tree), and whether it contains `.git`
/// (contains → protected, never evicted).
#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub class: Class,
    pub bytes: u64,
    /// Newest mtime of any entry in the tree (age / LRU fallback).
    pub newest_mtime: SystemTime,
    /// Mtime of the candidate root at scan time. This is what `clean`
    /// re-stats: comparing with `newest_mtime` marks "changed" on almost every
    /// directory, because an inner file is newer than the root.
    pub root_mtime: SystemTime,
    pub contains_git: bool,
    pub entries: u64,
}

pub struct Progress {
    entries: u64,
    bytes: u64,
    quiet: bool,
}

impl Progress {
    pub fn new(quiet: bool) -> Self {
        Self { entries: 0, bytes: 0, quiet }
    }
    fn tick(&mut self, add: u64) {
        self.entries += 1;
        self.bytes += add;
        if !self.quiet && self.entries.is_multiple_of(200_000) {
            eprint!("\r  … {} entries, {}", self.entries, crate::fmt_util::human(self.bytes));
            use std::io::Write;
            let _ = std::io::stderr().flush();
        }
    }
    pub fn finish(&self) {
        if !self.quiet && self.entries >= 200_000 {
            eprintln!();
        }
    }
}

/// Scan the config roots and return deduplicated candidates.
/// `root_filter` restricts the scan to a subdirectory (roots are
/// trimmed so we do not walk the world for nothing).
///
/// The installed `vassoura` binary scans in several worker processes
/// ([`crate::scan_pool`]). Tests and any other host stay in-process.
pub fn scan(cfg: &Config, root_filter: Option<&Path>, quiet: bool) -> Vec<Candidate> {
    scan_with(cfg, root_filter, quiet, None)
}

/// `deadline`: when the disk is already critical, stop after this long and
/// evict from whatever was measured. `None` waits for every job (a wedged
/// worker is still killed). In-process scans ignore the deadline and return
/// the full catalog, so LRU stays exact.
pub fn scan_with(
    cfg: &Config,
    root_filter: Option<&Path>,
    quiet: bool,
    deadline: Option<std::time::Duration>,
) -> Vec<Candidate> {
    if let Some(exe) = vassoura_exe() {
        match crate::scan_pool::run(&exe, cfg, root_filter, deadline) {
            Ok(v) => return v,
            Err(e) => eprintln!("# warning: parallel scan unavailable ({e}); scanning in-process"),
        }
    }
    scan_inprocess(cfg, root_filter, quiet)
}

fn vassoura_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if exe.file_name().is_some_and(|n| n == "vassoura") {
        Some(exe)
    } else {
        None
    }
}

fn scan_inprocess(cfg: &Config, root_filter: Option<&Path>, quiet: bool) -> Vec<Candidate> {
    let names: HashSet<&str> = cfg.artifact_names.iter().map(|s| s.as_str()).collect();
    let prune: HashSet<&str> = PRUNE_DIRS.iter().copied().collect();
    let mut out: Vec<Candidate> = Vec::new();
    let mut prog = Progress::new(quiet);

    for root in &cfg.artifact_roots {
        let root = expand(root);
        if !root.is_dir() {
            continue;
        }
        let scan_from = match root_filter.map(expand) {
            Some(f) if root.starts_with(&f) => root.clone(),
            Some(f) if f.starts_with(&root) => f,
            Some(_) => continue,
            None => root.clone(),
        };
        hunt(&scan_from, &names, &prune, &mut out, &mut prog);
    }

    for root in &cfg.app_cache_roots {
        let root = expand(root);
        if !root.is_dir() {
            continue;
        }
        let scan_from = match root_filter.map(expand) {
            Some(f) if root.starts_with(&f) => root.clone(),
            Some(f) if f.starts_with(&root) => f,
            Some(_) => continue,
            None => root.clone(),
        };
        if let Ok(rd) = fs::read_dir(&scan_from) {
            for entry in rd.flatten() {
                if entry.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false) {
                    out.push(measure(&entry.path(), Class::AppCache, &mut prog));
                }
            }
        }
    }

    prog.finish();
    accept(cfg, out)
}

/// Drop candidates outside the allowlist and duplicate paths.
pub(crate) fn accept(cfg: &Config, mut out: Vec<Candidate>) -> Vec<Candidate> {
    let allowed = crate::config::expanded_roots(cfg);
    let mut seen: HashSet<PathBuf> = HashSet::new();
    out.retain(|c| {
        let ok = allowed.iter().any(|(class, root)| {
            *class == c.class && c.path.starts_with(root)
        }) && seen.insert(c.path.clone());
        ok
    });
    out
}

/// One unit of parallel work: a project tree to hunt, or a single
/// cache/artifact directory to measure.
pub(crate) enum ScanJob {
    Hunt(PathBuf),
    Measure { path: PathBuf, class: Class },
}

/// Top-level jobs. A wedge inside one job must not be the whole scan,
/// so each immediate child of an allowlisted root is its own job.
pub(crate) fn list_jobs(cfg: &Config, root_filter: Option<&Path>) -> Vec<ScanJob> {
    let names: HashSet<&str> = cfg.artifact_names.iter().map(|s| s.as_str()).collect();
    let prune: HashSet<&str> = PRUNE_DIRS.iter().copied().collect();
    let mut jobs = Vec::new();
    for root in &cfg.artifact_roots {
        let root = expand(root);
        if !root.is_dir() {
            continue;
        }
        let scan_from = match root_filter.map(expand) {
            Some(f) if root.starts_with(&f) => root.clone(),
            Some(f) if f.starts_with(&root) => f,
            Some(_) => continue,
            None => root.clone(),
        };
        jobs.extend(split_root(&scan_from, &names, &prune));
    }
    for root in &cfg.app_cache_roots {
        let root = expand(root);
        if !root.is_dir() {
            continue;
        }
        let scan_from = match root_filter.map(expand) {
            Some(f) if root.starts_with(&f) => root.clone(),
            Some(f) if f.starts_with(&root) => f,
            Some(_) => continue,
            None => root.clone(),
        };
        if let Ok(rd) = fs::read_dir(&scan_from) {
            for entry in rd.flatten() {
                if entry.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false) {
                    jobs.push(ScanJob::Measure { path: entry.path(), class: Class::AppCache });
                }
            }
        }
    }
    jobs
}

fn has_sub_repos(dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(dir) else { return false };
    for entry in rd.flatten() {
        if entry.path().join(".git").exists() {
            return true;
        }
    }
    false
}

fn split_root(dir: &Path, names: &HashSet<&str>, prune: &HashSet<&str>) -> Vec<ScanJob> {
    let mut jobs = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else { return jobs };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if prune.contains(name.as_ref()) {
            continue;
        }
        if names.contains(name.as_ref()) {
            jobs.push(ScanJob::Measure { path: entry.path(), class: Class::Artifact });
        } else if !entry.path().join(".git").exists() && has_sub_repos(&entry.path()) {
            // Container directory without its own .git (e.g. software/railway):
            // split its immediate children into jobs so parallel workers can balance
            // sub-projects across the pool instead of one worker doing 100 repos alone.
            let sub = split_root(&entry.path(), names, prune);
            if sub.is_empty() {
                jobs.push(ScanJob::Hunt(entry.path()));
            } else {
                jobs.extend(sub);
            }
        } else {
            jobs.push(ScanJob::Hunt(entry.path()));
        }
    }
    jobs
}

/// Manual recursion: on a regenerable name, measure and do NOT descend
/// (node_modules inside node_modules belongs to the outer owner).
fn hunt(dir: &Path, names: &HashSet<&str>, prune: &HashSet<&str>, out: &mut Vec<Candidate>, prog: &mut Progress) {
    hunt_emit(dir, names, prune, prog, false, &mut || {}, &mut |c| out.push(c));
}

/// `budgeted`: worker processes cap each candidate and emit heartbeats so
/// the parent can kill a wedged `lstat` without losing the other jobs.
pub(crate) fn hunt_emit(
    dir: &Path,
    names: &HashSet<&str>,
    prune: &HashSet<&str>,
    prog: &mut Progress,
    budgeted: bool,
    beat: &mut dyn FnMut(),
    on_cand: &mut dyn FnMut(Candidate),
) {
    beat();
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if prune.contains(name.as_ref()) {
            continue;
        }
        if names.contains(name.as_ref()) {
            let c = if budgeted {
                measure_budgeted(&entry.path(), Class::Artifact, prog, beat)
            } else {
                measure(&entry.path(), Class::Artifact, prog)
            };
            on_cand(c);
        } else {
            hunt_emit(&entry.path(), names, prune, prog, budgeted, beat, on_cand);
        }
    }
}

fn mtime_of(md: &std::fs::Metadata) -> Option<SystemTime> {
    md.modified().ok()
}

/// Worker-side measure. Caps the walk so one enormous tree returns a lower
/// bound instead of occupying a worker for the rest of the cycle. Heartbeats
/// (`beat`) are what the parent watches: a blocked `lstat` stops them, and
/// the parent kills that process.
pub(crate) fn measure_budgeted(
    path: &Path,
    class: Class,
    prog: &mut Progress,
    beat: &mut dyn FnMut(),
) -> Candidate {
    // 40k entries is enough to know a tree is huge and, via BFS, to have seen
    // the shallow mtimes that decide age. 15s bounds a slow-but-alive disk.
    measure_inner(path, class, prog, 40_000, std::time::Duration::from_secs(15), beat)
}

/// Measure the tree: summed bytes (without following symlinks), newest mtime
/// of any entry (file or directory), and presence of `.git`.
fn measure(path: &Path, class: Class, prog: &mut Progress) -> Candidate {
    measure_inner(
        path,
        class,
        prog,
        u64::MAX,
        std::time::Duration::from_secs(86_400),
        &mut || {},
    )
}

fn measure_inner(
    path: &Path,
    class: Class,
    prog: &mut Progress,
    entry_cap: u64,
    budget: std::time::Duration,
    beat: &mut dyn FnMut(),
) -> Candidate {
    use std::collections::VecDeque;
    use std::time::Instant;

    let mut bytes = 0u64;
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut root_mtime = SystemTime::UNIX_EPOCH;
    let mut contains_git = false;
    let mut entries = 0u64;
    let started = Instant::now();

    beat();
    if let Ok(md) = fs::symlink_metadata(path) {
        if let Some(t) = mtime_of(&md) {
            newest = t;
            root_mtime = t;
        }
    }

    // BFS: a fresh file near the top is seen before the cap cuts the walk.
    let mut queue = VecDeque::new();
    queue.push_back(path.to_path_buf());
    while let Some(dir) = queue.pop_front() {
        if entries >= entry_cap || started.elapsed() >= budget {
            break;
        }
        beat();
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            if entries >= entry_cap || started.elapsed() >= budget {
                break;
            }
            beat();
            let Ok(md) = fs::symlink_metadata(entry.path()) else { continue };
            let ft = md.file_type();
            prog.tick(if ft.is_dir() { 0 } else { md.len() });
            entries += 1;
            if let Some(t) = mtime_of(&md) {
                if t > newest {
                    newest = t;
                }
            }
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if entry.file_name() == ".git" {
                    contains_git = true;
                    continue;
                }
                queue.push_back(entry.path());
                continue;
            }
            bytes += md.len();
        }
    }

    Candidate { path: path.to_path_buf(), class, bytes, newest_mtime: newest, root_mtime, contains_git, entries }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn age_dir(p: &Path, days: u64) {
        let t = filetime::FileTime::from_system_time(
            SystemTime::now() - Duration::from_secs(days * 86400),
        );
        filetime::set_file_times(p, t, t).unwrap();
    }

    fn mk(p: &Path, bytes: usize, days: u64) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![b'x'; bytes]).unwrap();
        age_dir(p, days);
        age_dir(p.parent().unwrap(), days);
    }

    #[test]
    fn hunt_measures_skips_symlink_git_and_nested() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        mk(&root.join("a/node_modules/pkg.js"), 100, 40);
        mk(&root.join("b/target/lib.rlib"), 200, 10);
        // inside .git: untouchable
        mk(&root.join(".git/node_modules/x.js"), 999, 400);
        // symlink with a regenerable name: skipped
        std::os::unix::fs::symlink(root.join("a"), root.join("c")).unwrap();
        std::os::unix::fs::symlink(root.join("a/node_modules"), root.join("d")).unwrap();

        let names: HashSet<&str> = ["node_modules", "target"].into_iter().collect();
        let prune: HashSet<&str> = [".git"].into_iter().collect();
        let mut out = Vec::new();
        let mut prog = Progress::new(true);
        hunt(root, &names, &prune, &mut out, &mut prog);

        out.sort_by_key(|c| c.path.clone());
        assert_eq!(out.len(), 2, "symlink and .git do not count: {:?}", out);
        assert_eq!(out[0].path, root.join("a/node_modules"));
        assert_eq!(out[0].bytes, 100);
        assert_eq!(out[1].bytes, 200);
        assert!(out[0].newest_mtime < SystemTime::now() - Duration::from_secs(39 * 86400));
    }

    #[test]
    fn measure_detects_nested_git_and_fresh_file() {
        let tmp = tempfile::tempdir().unwrap();
        let nm = tmp.path().join("node_modules");
        mk(&nm.join("dep/index.js"), 10, 40);
        std::fs::create_dir_all(nm.join("dep/.git")).unwrap();
        age_dir(&nm.join("dep/.git"), 400);

        let mut prog = Progress::new(true);
        let c = measure(&nm, Class::Artifact, &mut prog);
        assert!(c.contains_git, "inner .git must be detected");
    }
}
