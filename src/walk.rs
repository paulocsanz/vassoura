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
    /// A git worktree checkout (source tree). Its `.git` is a pointer file,
    /// not a repository — removal goes through `git worktree remove`, which
    /// refuses dirty or locked checkouts, and only after the merged gate.
    Worktree,
}

impl Class {
    pub fn label(self) -> &'static str {
        match self {
            Class::Artifact => "artifact",
            Class::AppCache => "app-cache",
            Class::Worktree => "worktree",
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
    scan_with_health(cfg, root_filter, quiet, deadline).0
}

/// Scan with the pool's health report — the daemon's scan-health breaker
/// reads worker deaths from here. When the pool cannot run at all the
/// in-process fallback is used AND flagged: under IO collapse that fallback
/// is what wedged processes in the kernel for hours.
pub fn scan_with_health(
    cfg: &Config,
    root_filter: Option<&Path>,
    quiet: bool,
    deadline: Option<std::time::Duration>,
) -> (Vec<Candidate>, crate::scan_pool::ScanHealth) {
    if let Some(exe) = vassoura_exe() {
        match crate::scan_pool::run(&exe, cfg, root_filter, deadline) {
            Ok((v, health)) => return (v, health),
            Err(e) => eprintln!("# warning: parallel scan unavailable ({e}); scanning in-process"),
        }
    }
    (
        scan_inprocess(cfg, root_filter, quiet),
        crate::scan_pool::ScanHealth { in_process_fallback: true, ..Default::default() },
    )
}

/// Probe budget: a shallow measurement is a lower bound on size, good
/// enough to rank the biggest targets — the 80/20 with ~1% of the IO of a
/// full walk. 400 entries / 250ms per candidate.
pub(crate) fn measure_probe(path: &Path, class: Class) -> Candidate {
    let mut prog = Progress::new(true);
    let mut c = measure_inner(
        path,
        class,
        &mut prog,
        400,
        std::time::Duration::from_millis(250),
        &mut |_| {},
    );
    if class == Class::Worktree {
        c.contains_git = false;
    }
    c
}

/// Shallow scan for degraded mode (scan-health breaker): measures only the
/// immediate artifact/cache/worktree children of the allowlisted roots —
/// `list_jobs` already knows that shape, so this runs its jobs with probe
/// budgets instead of full hunts. Seconds where the full scan dies.
pub fn scan_probe(cfg: &Config, root_filter: Option<&Path>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for job in list_jobs(cfg, root_filter) {
        match job {
            ScanJob::Measure { path, class } => {
                out.push(measure_probe(&path, class));
            }
            ScanJob::Hunt(dir) => {
                // one level only: regenerable-named children, nothing deeper
                let Ok(rd) = fs::read_dir(&dir) else { continue };
                for entry in rd.flatten() {
                    let Ok(ft) = entry.file_type() else { continue };
                    if !ft.is_dir() || ft.is_symlink() {
                        continue;
                    }
                    let name = entry.file_name();
                    if cfg.artifact_names.iter().any(|n| n == &name.to_string_lossy()) {
                        out.push(measure_probe(&entry.path(), Class::Artifact));
                    }
                }
            }
        }
    }
    accept(cfg, out)
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
            Some(f) if path_matches_root(&root, &f) => root.clone(),
            Some(f) if path_matches_root(&f, &root) => f,
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
            Some(f) if path_matches_root(&root, &f) => root.clone(),
            Some(f) if path_matches_root(&f, &root) => f,
            Some(_) => continue,
            None => root.clone(),
        };
        for (path, class) in enumerate_app_cache_candidates(&root, &scan_from, &names) {
            out.push(measure(&path, class, &mut prog));
        }
    }

    for root in &cfg.worktree_repos {
        let repo = expand(root);
        if !repo.is_dir() {
            continue;
        }
        let in_scope = |wt: &Path| match root_filter.map(expand) {
            Some(f) => path_matches_root(wt, &f) || path_matches_root(&f, wt),
            None => true,
        };
        let origin_ref = crate::worktrees::origin_default(&repo);
        if let Some(oref) = origin_ref {
            let mut candidate_wts = Vec::new();
            let now = SystemTime::now();
            let one_hour = std::time::Duration::from_secs(3600);
            for wt in crate::worktrees::enumerate(&repo) {
                if !in_scope(&wt.path) {
                    continue;
                }
                if let Ok(md) = fs::symlink_metadata(&wt.path) {
                    if let Ok(mtime) = md.modified() {
                        if now.duration_since(mtime).unwrap_or_default() < one_hour {
                            continue;
                        }
                    }
                }
                candidate_wts.push(wt);
            }
            let merged_heads = crate::worktrees::filter_merged_heads(
                &repo,
                candidate_wts.iter().map(|w| w.head.as_str()),
                &oref,
            ).unwrap_or_default();

            for wt in candidate_wts {
                if merged_heads.contains(&wt.head) {
                    out.push(crate::worktrees::measure_worktree(&wt.path, &mut prog));
                }
            }
        }
    }

    prog.finish();
    accept(cfg, out)
}

pub(crate) fn is_artifact_candidate_dir(path: &Path, names: &HashSet<&str>) -> bool {
    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if names.contains(file_name) {
        return true;
    }
    if file_name.ends_with("-target")
        || file_name.ends_with("_target")
        || file_name.starts_with("target-")
        || file_name.starts_with("target_")
        || file_name.starts_with("cargo-")
        || file_name.starts_with("cargo_")
    {
        return true;
    }
    if path.join("CACHEDIR.TAG").exists() {
        return true;
    }
    false
}

/// Helper to identify if a path is part of a Darwin `var/folders` structure.
///
/// On macOS, `getconf DARWIN_USER_TEMP_DIR` and `DARWIN_USER_CACHE_DIR` point to:
/// `/var/folders/<bucket>/<user_hash>/[T,C,X,0]/...`
/// (or `/private/var/folders/<bucket>/<user_hash>/[T,C,X,0]/...`).
///
/// The buckets (e.g. `q_`, `zz`) and user hashes contain directories for multiple
/// users and system daemons. Evicting at the bucket or hash level is impossible
/// (root permissions) and catastrophic. Furthermore, macOS daemons write to `T` and `C`
/// continuously, so the bucket directory is always 0 seconds old and in use.
///
/// The actual regenerable build dust and test artifacts (e.g. Playwright, cargo test,
/// browser caches) sit as individual directories inside `T`, `C`, `X`, and `0` (depth 4
/// from `var/folders`).
pub(crate) fn is_var_folders_root(root: &Path) -> bool {
    let s = root.to_string_lossy();
    let s = s.trim_end_matches('/');
    s == "/var/folders" || s == "/private/var/folders" || s.ends_with("/var/folders")
}

pub(crate) fn strip_private_prefix(p: &Path) -> Option<PathBuf> {
    if let Ok(rel) = p.strip_prefix("/private") {
        Some(Path::new("/").join(rel))
    } else {
        None
    }
}

pub(crate) fn path_matches_root(path: &Path, root: &Path) -> bool {
    if path.starts_with(root) {
        return true;
    }
    if let Some(norm_path) = strip_private_prefix(path) {
        if norm_path.starts_with(root) {
            return true;
        }
    }
    if let Some(norm_root) = strip_private_prefix(root) {
        if path.starts_with(&norm_root) {
            return true;
        }
    }
    false
}

/// Enumerate candidate directories inside an `app_cache_roots` entry.
///
/// For standard cache roots (e.g. `~/Library/Caches`, `/private/tmp`), this
/// returns the immediate child directories.
/// For Darwin `var/folders` roots, this traverses down to depth 4 (the child
/// directories inside `T`, `C`, `X`, `0`) where discrete test and build artifacts
/// reside.
pub(crate) fn enumerate_app_cache_candidates(
    root: &Path,
    scan_from: &Path,
    names: &HashSet<&str>,
) -> Vec<(PathBuf, Class)> {
    let mut candidates = Vec::new();

    if is_var_folders_root(root) {
        let rel_depth = if let Ok(rel) = scan_from.strip_prefix(root) {
            rel.components().count()
        } else if let Some(norm_scan) = strip_private_prefix(scan_from) {
            if let Ok(rel) = norm_scan.strip_prefix(root) {
                rel.components().count()
            } else {
                0
            }
        } else if let Some(norm_root) = strip_private_prefix(root) {
            if let Ok(rel) = scan_from.strip_prefix(&norm_root) {
                rel.components().count()
            } else {
                0
            }
        } else {
            0
        };

        if rel_depth >= 4 {
            if let Ok(meta) = fs::symlink_metadata(scan_from) {
                if meta.is_dir() && !meta.is_symlink() {
                    let class = if is_artifact_candidate_dir(scan_from, names) {
                        Class::Artifact
                    } else {
                        Class::AppCache
                    };
                    candidates.push((scan_from.to_path_buf(), class));
                }
            }
            return candidates;
        }

        let remaining_levels = 4 - rel_depth;
        let mut current_dirs = vec![scan_from.to_path_buf()];

        for _ in 1..remaining_levels {
            let mut next_dirs = Vec::new();
            for dir in current_dirs {
                if let Ok(rd) = fs::read_dir(dir) {
                    for entry in rd.flatten() {
                        if entry.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false) {
                            next_dirs.push(entry.path());
                        }
                    }
                }
            }
            current_dirs = next_dirs;
        }

        for dir in current_dirs {
            if let Ok(rd) = fs::read_dir(dir) {
                for entry in rd.flatten() {
                    if entry.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false) {
                        let path = entry.path();
                        let class = if is_artifact_candidate_dir(&path, names) {
                            Class::Artifact
                        } else {
                            Class::AppCache
                        };
                        candidates.push((path, class));
                    }
                }
            }
        }
        return candidates;
    }

    if let Ok(rd) = fs::read_dir(scan_from) {
        for entry in rd.flatten() {
            if entry.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false) {
                let path = entry.path();
                let class = if is_artifact_candidate_dir(&path, names) {
                    Class::Artifact
                } else {
                    Class::AppCache
                };
                candidates.push((path, class));
            }
        }
    }

    candidates
}

/// Drop candidates outside the allowlist and duplicate paths.
pub(crate) fn accept(cfg: &Config, mut out: Vec<Candidate>) -> Vec<Candidate> {
    let allowed = crate::config::expanded_roots(cfg);
    let mut seen: HashSet<PathBuf> = HashSet::new();
    out.retain(|c| {
        let ok = allowed.iter().any(|(class, root)| {
            let class_match = c.class == Class::Worktree
                || *class == c.class
                || (*class == Class::AppCache && c.class == Class::Artifact);
            class_match && path_matches_root(&c.path, root)
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
    let mut hunt_jobs = Vec::new();
    let mut measure_jobs = Vec::new();

    for root in &cfg.artifact_roots {
        let root = expand(root);
        if !root.is_dir() {
            continue;
        }
        let scan_from = match root_filter.map(expand) {
            Some(f) if path_matches_root(&root, &f) => root.clone(),
            Some(f) if path_matches_root(&f, &root) => f,
            Some(_) => continue,
            None => root.clone(),
        };
        for job in split_root(&scan_from, &names, &prune) {
            match job {
                ScanJob::Measure { .. } => measure_jobs.push(job),
                ScanJob::Hunt(..) => hunt_jobs.push(job),
            }
        }
    }
    for root in &cfg.app_cache_roots {
        let root = expand(root);
        if !root.is_dir() {
            continue;
        }
        let scan_from = match root_filter.map(expand) {
            Some(f) if path_matches_root(&root, &f) => root.clone(),
            Some(f) if path_matches_root(&f, &root) => f,
            Some(_) => continue,
            None => root.clone(),
        };
        for (path, class) in enumerate_app_cache_candidates(&root, &scan_from, &names) {
            measure_jobs.push(ScanJob::Measure { path, class });
        }
    }
    for repo in &cfg.worktree_repos {
        let repo = expand(repo);
        if !repo.is_dir() {
            continue;
        }
        let in_scope = |wt: &Path| match root_filter.map(expand) {
            Some(f) => path_matches_root(wt, &f) || path_matches_root(&f, wt),
            None => true,
        };
        let origin_ref = crate::worktrees::origin_default(&repo);
        if let Some(oref) = origin_ref {
            let mut candidate_wts = Vec::new();
            let now = SystemTime::now();
            let one_hour = std::time::Duration::from_secs(3600);
            for wt in crate::worktrees::enumerate(&repo) {
                if !in_scope(&wt.path) {
                    continue;
                }
                // Invariant 4: minimum age floor is 1h. If the root was modified
                // within the last hour, it is actively in use and cannot be evicted.
                if let Ok(md) = fs::symlink_metadata(&wt.path) {
                    if let Ok(mtime) = md.modified() {
                        if now.duration_since(mtime).unwrap_or_default() < one_hour {
                            continue;
                        }
                    }
                }
                candidate_wts.push(wt);
            }
            let merged_heads = crate::worktrees::filter_merged_heads(
                &repo,
                candidate_wts.iter().map(|w| w.head.as_str()),
                &oref,
            ).unwrap_or_default();

            for wt in candidate_wts {
                if merged_heads.contains(&wt.head) {
                    measure_jobs.push(ScanJob::Measure { path: wt.path, class: Class::Worktree });
                }
            }
        }
    }

    // Measure jobs (single-directory caches, /tmp targets, worktrees) complete in milliseconds.
    // Putting them first ensures they are never starved by deep recursive Hunt jobs when a cycle deadline hits.
    measure_jobs.extend(hunt_jobs);
    measure_jobs
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
            // Check for immediate shallow artifacts in this repository/project directory
            // (e.g. pedradb/target, centro/target, etc.) and promote them to Measure jobs
            // so they are prioritized ahead of deep recursive Hunt scans.
            if let Ok(sub_rd) = fs::read_dir(entry.path()) {
                for sub_entry in sub_rd.flatten() {
                    let sub_name = sub_entry.file_name();
                    let sub_name = sub_name.to_string_lossy();
                    if names.contains(sub_name.as_ref()) {
                        if let Ok(sft) = sub_entry.file_type() {
                            if sft.is_dir() && !sft.is_symlink() {
                                jobs.push(ScanJob::Measure { path: sub_entry.path(), class: Class::Artifact });
                            }
                        }
                    }
                }
            }
            jobs.push(ScanJob::Hunt(entry.path()));
        }
    }
    jobs
}

/// Manual recursion: on a regenerable name, measure and do NOT descend
/// (node_modules inside node_modules belongs to the outer owner).
fn hunt(dir: &Path, names: &HashSet<&str>, prune: &HashSet<&str>, out: &mut Vec<Candidate>, prog: &mut Progress) {
    hunt_emit(dir, names, prune, prog, false, &mut |_| {}, &mut |c| out.push(c));
}

/// `budgeted`: worker processes cap each candidate and emit heartbeats so
/// the parent can kill a wedged `lstat` without losing the other jobs.
pub(crate) fn hunt_emit(
    dir: &Path,
    names: &HashSet<&str>,
    prune: &HashSet<&str>,
    prog: &mut Progress,
    budgeted: bool,
    beat: &mut dyn FnMut(Option<&Candidate>),
    on_cand: &mut dyn FnMut(Candidate),
) {
    beat(None);
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
                measure_budgeted(&entry.path(), Class::Artifact, prog, &mut |c| beat(Some(c)))
            } else {
                measure(&entry.path(), Class::Artifact, prog)
            };
            on_cand(c);
        } else {
            hunt_emit(&entry.path(), names, prune, prog, budgeted, beat, on_cand);
        }
    }
}

pub(crate) fn mtime_of(md: &std::fs::Metadata) -> Option<SystemTime> {
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
    beat: &mut dyn FnMut(&Candidate),
) -> Candidate {
    // 40k entries is enough to know a tree is huge and, via BFS, to have seen
    // the shallow mtimes that decide age. 10s bounds a slow-but-alive disk.
    measure_inner(path, class, prog, 40_000, std::time::Duration::from_secs(10), beat)
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
        &mut |_| {},
    )
}

/// Public entry for non-hunt sources (worktree enumeration) that need the
/// full, unbudgeted measurement.
pub(crate) fn measure_path(path: &Path, class: Class, prog: &mut Progress) -> Candidate {
    measure(path, class, prog)
}

fn measure_inner(
    path: &Path,
    class: Class,
    prog: &mut Progress,
    entry_cap: u64,
    budget: std::time::Duration,
    beat: &mut dyn FnMut(&Candidate),
) -> Candidate {
    use std::collections::VecDeque;
    use std::time::Instant;

    let mut bytes = 0u64;
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut root_mtime = SystemTime::UNIX_EPOCH;
    let mut contains_git = false;
    let mut entries = 0u64;
    let started = Instant::now();

    if let Ok(md) = fs::symlink_metadata(path) {
        if let Some(t) = mtime_of(&md) {
            newest = t;
            root_mtime = t;
        }
    }

    let make_cand = |bytes: u64, newest: SystemTime, entries: u64, contains_git: bool| Candidate {
        path: path.to_path_buf(),
        class,
        bytes,
        newest_mtime: newest,
        root_mtime,
        contains_git: if class == Class::Worktree { false } else { contains_git },
        entries,
    };

    beat(&make_cand(bytes, newest, entries, contains_git));

    // BFS: a fresh file near the top is seen before the cap cuts the walk.
    let mut queue = VecDeque::new();
    queue.push_back(path.to_path_buf());
    while let Some(dir) = queue.pop_front() {
        if entries >= entry_cap || started.elapsed() >= budget {
            break;
        }
        beat(&make_cand(bytes, newest, entries, contains_git));
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            if entries >= entry_cap || started.elapsed() >= budget {
                break;
            }
            beat(&make_cand(bytes, newest, entries, contains_git));
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

    make_cand(bytes, newest, entries, contains_git)
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

    #[test]
    fn probe_scan_finds_shallow_artifacts_without_deep_walking() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // shallow: artifact dir directly under the root
        mk(&root.join("proj-a/target/lib.rlib"), 300, 40);
        // a container with an inner REPO is split into Measure jobs by
        // list_jobs — its node_modules is probed at the job level
        std::fs::create_dir_all(root.join("container/proj-b/.git")).unwrap();
        mk(&root.join("container/proj-b/node_modules/x.js"), 500, 40);
        // artifact dir under a plain project dir: probed as Hunt 1-level
        mk(&root.join("proj-c/target/inner/deep.rlib"), 700, 40);

        let cfg = Config {
            artifact_roots: vec![root.to_path_buf()],
            app_cache_roots: vec![],
            worktree_repos: vec![],
            artifact_names: vec!["target".into(), "node_modules".into()],
            ..Config::default()
        };
        let cands = scan_probe(&cfg, None);
        let mut paths: Vec<_> = cands.iter().map(|c| c.path.display().to_string()).collect();
        paths.sort();
        assert_eq!(paths.len(), 3, "{paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("proj-a/target")), "{paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("proj-b/node_modules")), "{paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("proj-c/target")), "{paths:?}");
        assert!(cands.iter().all(|c| c.bytes > 0), "probe measures a lower bound: {:?}", cands);
    }

    #[test]
    fn app_cache_detects_target_directories_as_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_root = tmp.path();

        let target1 = cache_root.join("caixote-vigia-target");
        let target2 = cache_root.join("target-vigia");
        let target3 = cache_root.join("cargo-boundary");
        let target4 = cache_root.join("custom-build-tagged");
        let normal_cache = cache_root.join("com.apple.Safari");

        std::fs::create_dir_all(&target1).unwrap();
        std::fs::create_dir_all(&target2).unwrap();
        std::fs::create_dir_all(&target3).unwrap();
        std::fs::create_dir_all(&target4).unwrap();
        std::fs::write(target4.join("CACHEDIR.TAG"), "Signature: 8a477f597d28d172789f06886806bc55").unwrap();
        std::fs::create_dir_all(&normal_cache).unwrap();

        let names: HashSet<&str> = ["target", "node_modules"].into_iter().collect();
        assert!(is_artifact_candidate_dir(&target1, &names));
        assert!(is_artifact_candidate_dir(&target2, &names));
        assert!(is_artifact_candidate_dir(&target3, &names));
        assert!(is_artifact_candidate_dir(&target4, &names));
        assert!(!is_artifact_candidate_dir(&normal_cache, &names));

        let cfg = Config {
            artifact_roots: vec![],
            app_cache_roots: vec![cache_root.to_path_buf()],
            worktree_repos: vec![],
            ..Config::default()
        };

        let jobs = list_jobs(&cfg, None);
        for job in jobs {
            match job {
                ScanJob::Measure { path, class } => {
                    if path == normal_cache {
                        assert_eq!(class, Class::AppCache);
                    } else {
                        assert_eq!(class, Class::Artifact, "path {} should be Artifact", path.display());
                    }
                }
                ScanJob::Hunt(p) => panic!("unexpected Hunt job for app_cache_roots: {}", p.display()),
            }
        }
    }

    #[test]
    fn list_jobs_prioritizes_measure_ahead_of_hunt() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let art_dir = root.join("software/proj");
        let cache_dir = root.join("cache/some-cache");
        std::fs::create_dir_all(&art_dir).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();

        let cfg = Config {
            artifact_roots: vec![root.join("software")],
            app_cache_roots: vec![root.join("cache")],
            worktree_repos: vec![],
            ..Config::default()
        };

        let jobs = list_jobs(&cfg, None);
        let mut seen_hunt = false;
        for job in jobs {
            match job {
                ScanJob::Measure { .. } => {
                    assert!(!seen_hunt, "Measure job must come before any Hunt job");
                }
                ScanJob::Hunt(..) => {
                    seen_hunt = true;
                }
            }
        }
    }

    #[test]
    fn darwin_var_folders_enumerates_leaf_candidates_not_bucket() {
        let tmp = tempfile::tempdir().unwrap();
        let var_folders = tmp.path().join("var/folders");
        let bucket = var_folders.join("q_");
        let user_hash = bucket.join("5tskc6890_z2pk73666wb_m40000gn");
        let temp_dir = user_hash.join("T");
        let cache_dir = user_hash.join("C");
        let cand1 = temp_dir.join("playwright-artifacts-dead");
        let cand2 = temp_dir.join("cargo-test-target");
        let cand3 = cache_dir.join("com.google.Chrome");

        std::fs::create_dir_all(&cand1).unwrap();
        std::fs::create_dir_all(&cand2).unwrap();
        std::fs::create_dir_all(&cand3).unwrap();
        std::fs::write(cand1.join("trace.zip"), b"trace").unwrap();
        std::fs::write(cand2.join("output"), b"build").unwrap();
        std::fs::write(cand3.join("data"), b"cache").unwrap();

        let cfg = Config {
            artifact_roots: vec![],
            app_cache_roots: vec![var_folders.clone()],
            worktree_repos: vec![],
            ..Config::default()
        };

        let jobs = list_jobs(&cfg, None);
        let mut job_map = std::collections::HashMap::new();
        for j in jobs {
            match j {
                ScanJob::Measure { path, class } => {
                    job_map.insert(path, class);
                }
                ScanJob::Hunt(p) => panic!("unexpected Hunt job: {}", p.display()),
            }
        }

        assert!(!job_map.contains_key(&bucket), "bucket directory q_ must not be candidate");
        assert!(!job_map.contains_key(&user_hash), "user hash directory must not be candidate");
        assert!(!job_map.contains_key(&temp_dir), "T directory must not be candidate");
        assert!(!job_map.contains_key(&cache_dir), "C directory must not be candidate");

        assert_eq!(job_map.get(&cand1), Some(&Class::AppCache));
        assert_eq!(job_map.get(&cand2), Some(&Class::Artifact));
        assert_eq!(job_map.get(&cand3), Some(&Class::AppCache));

        // Scan in-process also finds the leaf candidates and measures bytes
        let scanned = scan_inprocess(&cfg, None, true);
        assert_eq!(scanned.len(), 3);
        assert!(scanned.iter().all(|c| c.bytes > 0));

        // Root filter down to T only returns T items
        let filtered_jobs = list_jobs(&cfg, Some(&temp_dir));
        assert_eq!(filtered_jobs.len(), 2);
        let filtered_paths: Vec<_> = filtered_jobs.into_iter().map(|j| match j {
            ScanJob::Measure { path, .. } => path,
            ScanJob::Hunt(..) => unreachable!(),
        }).collect();
        assert!(filtered_paths.contains(&cand1));
        assert!(filtered_paths.contains(&cand2));
        assert!(!filtered_paths.contains(&cand3));
    }

    #[test]
    fn path_matches_root_handles_darwin_private_symlink_aliases() {
        assert!(path_matches_root(Path::new("/private/var/folders/q_/item"), Path::new("/var/folders")));
        assert!(path_matches_root(Path::new("/var/folders/q_/item"), Path::new("/private/var/folders")));
        assert!(path_matches_root(Path::new("/private/tmp/cargo-target"), Path::new("/tmp")));
        assert!(path_matches_root(Path::new("/tmp/cargo-target"), Path::new("/private/tmp")));
        assert!(path_matches_root(Path::new("/Users/paulo/software"), Path::new("/Users/paulo/software")));
        assert!(!path_matches_root(Path::new("/etc/passwd"), Path::new("/tmp")));
        assert!(!path_matches_root(Path::new("/var/folders"), Path::new("/tmp")));
    }
}

