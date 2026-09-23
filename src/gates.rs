//! Gates de uso (P1.2): um candidato só é removido se NENHUM processo vivo
//! tiver arquivo aberto sob ele (lsof real) e a worktree git que o contém
//! estiver limpa. Gates ausentes/indisponíveis falham FECHADO: o candidato
//! é pulado, nunca removido às cegas.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseState {
    /// Nada em uso: candidato segue elegível.
    Free,
    /// Em uso (motivo legível): pular.
    InUse(String),
    /// Gate indisponível (binário ausente/erro): fail-closed → pular.
    Unavailable(String),
}

fn lsof_probe(args: &[&OsStr]) -> Result<bool, String> {
    let out = Command::new("lsof")
        .args(args)
        .output()
        .map_err(|e| format!("spawn lsof: {e}"))?;
    // O exit code do lsof varia (1 mesmo com achados); o sinal é o stdout
    // (-F p imprime uma linha "p<pid>" por processo).
    Ok(!out.stdout.is_empty())
}

/// lsof real: processo vivo com arquivo aberto SOB o diretório (+D,
/// recursivo) ou com o próprio diretório aberto (cwd/fd).
pub fn lsof_state(path: &Path) -> UseState {
    let p = path.as_os_str();
    let under = ["-w", "-F", "p", "+D"].iter().map(|s| OsStr::new(*s)).chain(std::iter::once(p));
    match lsof_probe(&under.collect::<Vec<_>>()) {
        Ok(true) => return UseState::InUse("processo com arquivo aberto (lsof +D)".into()),
        Err(e) => return UseState::Unavailable(e),
        Ok(false) => {}
    }
    let itself = ["-w", "-F", "p", "--"].iter().map(|s| OsStr::new(*s)).chain(std::iter::once(p));
    match lsof_probe(&itself.collect::<Vec<_>>()) {
        Ok(true) => UseState::InUse("processo com o diretório aberto (lsof cwd/fd)".into()),
        Err(e) => UseState::Unavailable(e),
        Ok(false) => UseState::Free,
    }
}

/// Repo git mais próximo acima do candidato (dir OU arquivo — worktree).
pub fn nearest_repo(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|a| a.join(".git").exists())
        .map(|a| a.to_path_buf())
}

/// git real: candidato dentro de worktree com trabalho não-commitado → pulado.
/// Worktree limpa (ou nem-repo) segue elegível.
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
                UseState::InUse(format!("worktree com trabalho não-commitado ({})", repo.display()))
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
        UseState::Unavailable(e) => Some(format!("{gate} indisponível (fail-closed): {e}")),
    }
}

/// Gate de produção usado por `clean --apply` e pelo daemon:
/// `Some(motivo)` = pular (sem remoção, sem ledger).
pub fn in_use(path: &Path) -> Option<String> {
    skip_of(lsof_state(path), "em uso")
        .or_else(|| skip_of(git_state(path), "em uso"))
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
            .expect("python3 para segurar arquivo");
        std::thread::sleep(std::time::Duration::from_millis(800));
        let st = lsof_state(tmp.path());
        let _ = hold.kill();
        let _ = hold.wait();
        assert!(matches!(st, UseState::InUse(_)), "esperava InUse, veio {st:?}");
    }

    #[test]
    fn git_gate_non_repo_is_free_dirty_is_in_use_clean_is_free() {
        let tmp = tempfile::tempdir().unwrap();
        let cand = tmp.path().join("node_modules");
        std::fs::create_dir_all(&cand).unwrap();
        assert_eq!(git_state(&cand), UseState::Free, "sem repo → elegível");

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
            "não-commitado → pulado"
        );
        git(&["add", "."]);
        git(&["commit", "-qm", "x"]);
        assert_eq!(git_state(&cand), UseState::Free, "limpa → elegível");
    }

    #[test]
    fn unavailable_gate_fails_closed_in_combined() {
        // A falha fechada em produção é a composição em `in_use`; aqui o
        // mapeamento Unavailable → pulado é verificado direto.
        let why = skip_of(UseState::Unavailable("boom".into()), "em uso").unwrap();
        assert!(why.contains("fail-closed"), "{why}");
        assert!(skip_of(UseState::Free, "em uso").is_none());
    }
}
