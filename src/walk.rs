use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use walkdir::WalkDir;

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

/// Um diretório regenerável catalogado: quanto ocupa, quando foi escrito
/// pela última vez (mtime mais novo encontrado na árvore) e se contém `.git`
/// (contém → protegido, nunca evictado).
#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub class: Class,
    pub bytes: u64,
    pub newest_mtime: SystemTime,
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
            eprint!("\r  … {} entradas, {}", self.entries, crate::fmt_util::human(self.bytes));
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

/// Escaneia as raízes do config e devolve os candidatos deduplicados.
/// `root_filter` restringe o scan a um subdiretório (as raízes são
/// aparadas para não andar o mundo à toa).
pub fn scan(cfg: &Config, root_filter: Option<&Path>, quiet: bool) -> Vec<Candidate> {
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

    // Dedup por caminho (raízes sobrepostas) + allowlist reforçada.
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

/// Recursão manual: ao casar um nome regenerável, mede e NÃO desce
/// (node_modules dentro de node_modules é do dono de cima).
fn hunt(dir: &Path, names: &HashSet<&str>, prune: &HashSet<&str>, out: &mut Vec<Candidate>, prog: &mut Progress) {
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
            out.push(measure(&entry.path(), Class::Artifact, prog));
        } else {
            hunt(&entry.path(), names, prune, out, prog);
        }
    }
}

fn mtime_of(md: &std::fs::Metadata) -> Option<SystemTime> {
    md.modified().ok()
}

/// Mede a árvore: bytes somados (sem seguir symlinks), mtime mais novo
/// de qualquer entrada (arquivo ou diretório) e presença de `.git`.
fn measure(path: &Path, class: Class, prog: &mut Progress) -> Candidate {
    let mut bytes = 0u64;
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut contains_git = false;
    let mut entries = 0u64;

    if let Ok(md) = fs::symlink_metadata(path) {
        if let Some(t) = mtime_of(&md) {
            newest = t;
        }
    }

    for e in WalkDir::new(path).follow_links(false).into_iter().filter_map(|r| r.ok()) {
        let Ok(md) = e.metadata() else { continue };
        let ft = e.file_type();
        prog.tick(if ft.is_dir() { 0 } else { md.len() });
        entries += 1;
        if let Some(t) = mtime_of(&md) {
            if t > newest {
                newest = t;
            }
        }
        if e.depth() > 0 && ft.is_dir() && e.file_name() == ".git" {
            contains_git = true;
        }
        if ft.is_dir() || ft.is_symlink() {
            continue;
        }
        bytes += md.len();
    }

    Candidate { path: path.to_path_buf(), class, bytes, newest_mtime: newest, contains_git, entries }
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
        // dentro de .git: intocável
        mk(&root.join(".git/node_modules/x.js"), 999, 400);
        // symlink com nome regenerável: pulado
        std::os::unix::fs::symlink(root.join("a"), root.join("c")).unwrap();
        std::os::unix::fs::symlink(root.join("a/node_modules"), root.join("d")).unwrap();

        let names: HashSet<&str> = ["node_modules", "target"].into_iter().collect();
        let prune: HashSet<&str> = [".git"].into_iter().collect();
        let mut out = Vec::new();
        let mut prog = Progress::new(true);
        hunt(root, &names, &prune, &mut out, &mut prog);

        out.sort_by_key(|c| c.path.clone());
        assert_eq!(out.len(), 2, "symlink e .git não contam: {:?}", out);
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
        assert!(c.contains_git, ".git interno precisa ser detectado");
    }
}
