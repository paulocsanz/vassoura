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

/// Executa o plano com os gates do último instante:
/// 1. o caminho ainda existe e ainda é diretório (sem symlink);
/// 2. o mtime da raiz é exatamente o do scan — se algo escreveu lá
///    dentro depois do scan, a evicção é abortada para aquele item;
/// 3. toda remoção bem-sucedida nasce com linha no ledger.
pub fn apply(items: &[PlanItem], ledger_path: &Path) -> Outcome {
    let mut out = Outcome::default();
    for it in items {
        let path = &it.cand.path;
        let Ok(md) = fs::symlink_metadata(path) else {
            out.skipped.push((path.clone(), "sumiu desde o scan".into()));
            continue;
        };
        if md.is_symlink() || !md.is_dir() {
            out.skipped.push((path.clone(), "não é mais um diretório comum".into()));
            continue;
        }
        if md.modified().ok() != Some(it.cand.newest_mtime) {
            out.skipped.push((path.clone(), "mudou desde o scan (protegido)".into()));
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
                    // remoção sem ledger viola a recusa "nada-sem-ledger":
                    // o diretório já saiu; registramos o erro no stderr.
                    eprintln!("# ERRO ledger {}: {e} (item removido: {})", ledger_path.display(), path.display());
                }
                out.freed += it.cand.bytes;
                out.removed += 1;
            }
            Err(e) => {
                out.skipped.push((path.clone(), format!("rm falhou: {e}")));
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
                contains_git: false,
                entries: 1,
            },
            age_days: age_d as f64,
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
        // simula idades
        filetime::set_file_times(&a, old_mtime.into(), old_mtime.into()).unwrap();
        filetime::set_file_times(&b, old_mtime.into(), old_mtime.into()).unwrap();

        let ledger = tmp.path().join("ledger.jsonl");
        // item `b` foi tocado depois do scan (mtime diverge)
        let stale_b = SystemTime::now() - Duration::from_secs(60);
        let items = vec![
            item(&a, 200, 30, old_mtime),
            item(&b, 200, 30, stale_b),
        ];
        let out = apply(&items, &ledger);

        assert_eq!(out.removed, 1);
        assert!(out.skipped.iter().any(|(p, _)| p == &b), "b precisa ser protegido");
        assert!(!a.exists());
        assert!(b.exists(), "b não pode ser removido");
        let txt = fs::read_to_string(&ledger).unwrap();
        let lines: Vec<&str> = txt.lines().collect();
        assert_eq!(lines.len(), 1);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["path"], a.display().to_string());
        assert_eq!(v["hint"], "pnpm install");
    }
}
