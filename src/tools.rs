//! Classes de ferramenta (P1.3): evicção estendida a coisas que não são
//! diretórios de build — modelos ollama (LRU por modified via `ollama rm`),
//! docker/orbstack prune (a construção do comando JAMAIS toca volumes),
//! toolchains rustup velhas (mantém as N mais novas), `pnpm store prune` e
//! `go clean -modcache`.
//!
//! Separação: SELEÇÃO é lógica pura (testada aqui); EXECUÇÃO só acontece via
//! o CLI da própria ferramenta, é seca por default (`--apply`) e fail-closed:
//! ferramenta ausente/quebrada é pulada e reportada. Toda remoção executada
//! grava linha no ledger com dica de regeneração.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::config::{Config, ToolsCfg};
use crate::fmt_util::{age_days, GIB};

// ---------------------------------------------------------------- seleção

#[derive(Debug, Clone, PartialEq)]
pub struct OllamaModel {
    pub name: String,
    pub bytes: u64,
    /// Última modificação (coluna MODIFIED do `ollama list`).
    pub used: SystemTime,
}

fn parse_size(s: &str) -> Option<u64> {
    let t: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let split = t.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult = match unit {
        "B" => 1.0,
        "KB" | "kB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((n * mult) as u64)
}

fn relative_age_secs(n: u64, unit: &str) -> Option<u64> {
    let per = match unit {
        "second" | "seconds" => 1,
        "minute" | "minutes" => 60,
        "hour" | "hours" => 3600,
        "day" | "days" => 86400,
        "week" | "weeks" => 7 * 86400,
        "month" | "months" => 30 * 86400,
        "year" | "years" => 365 * 86400,
        _ => return None,
    };
    Some(n * per)
}

/// Parseia `ollama list`. Linhas sem MODIFIED legível são FAIL-CLOSED:
/// ficam de fora do plano (nunca removemos o que não sabemos medir).
/// Devolve (modelos parseados, linhas descartadas).
pub fn parse_ollama_list(output: &str, now: SystemTime) -> (Vec<OllamaModel>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for line in output.lines() {
        let tok: Vec<&str> = line.split_whitespace().collect();
        if tok.is_empty() || tok[0] == "NAME" {
            continue;
        }
        // formato esperado: NAME ID SIZE "N unit ago" (SIZE pode ter espaço:
        // "5.4 GB")
        let Some(ago) = tok.iter().rposition(|t| *t == "ago") else {
            skipped += 1;
            continue;
        };
        let (n, unit) = match (ago.checked_sub(2), ago.checked_sub(1)) {
            (Some(i), Some(j)) if ago >= 4 => (tok[i], tok[j]),
            _ => {
                skipped += 1;
                continue;
            }
        };
        let Ok(n) = n.parse::<u64>() else {
            skipped += 1;
            continue;
        };
        let Some(age) = relative_age_secs(n, unit) else {
            skipped += 1;
            continue;
        };
        let size_two = tok[ago - 4..ago - 2].concat();
        let size_one = tok[ago - 3].to_string();
        let Some(bytes) = parse_size(&size_two).or_else(|| parse_size(&size_one)) else {
            skipped += 1;
            continue;
        };
        out.push(OllamaModel {
            name: tok[0].to_string(),
            bytes,
            used: now - Duration::from_secs(age),
        });
    }
    (out, skipped)
}

/// LRU por modified: mais velho primeiro até cobrir `need_bytes`.
pub fn select_ollama(models: &[OllamaModel], need_bytes: u64) -> Vec<&OllamaModel> {
    let mut sorted: Vec<&OllamaModel> = models.iter().collect();
    sorted.sort_by_key(|m| m.used);
    let mut planned = 0u64;
    let mut out = Vec::new();
    for m in sorted {
        if planned >= need_bytes {
            break;
        }
        planned += m.bytes;
        out.push(m);
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct Toolchain {
    pub name: String,
    pub default: bool,
    /// Versão semver do nome (None = canal/rolling → protegido).
    pub version: Option<(u64, u64, u64)>,
}

fn parse_version(name: &str) -> Option<(u64, u64, u64)> {
    let head = name.split('-').next()?;
    let mut it = head.split('.');
    let a: u64 = it.next()?.parse().ok()?;
    let b: u64 = it.next()?.parse().ok()?;
    let c: u64 = it.next()?.parse().ok()?;
    Some((a, b, c))
}

/// Parseia `rustup toolchain list` (linhas "N.N.N-triple" com marcador
/// "(default)" opcional).
pub fn parse_rustup_list(output: &str) -> Vec<Toolchain> {
    output
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() {
                return None;
            }
            let name = l.split_whitespace().next()?.to_string();
            let version = parse_version(&name);
            Some(Toolchain {
                name,
                default: l.contains("(default)"),
                version,
            })
        })
        .collect()
}

/// Mantém as `keep` toolchains VERSIONADAS mais novas (e tudo que é canal
/// rolling, custom ou default). O resto sai.
pub fn select_rustup(tcs: &[Toolchain], keep: usize) -> Vec<&Toolchain> {
    let mut versioned: Vec<&Toolchain> = tcs
        .iter()
        .filter(|t| t.version.is_some() && !t.default)
        .collect();
    versioned.sort_by_key(|t| t.version);
    if versioned.len() <= keep {
        return Vec::new();
    }
    versioned[..versioned.len() - keep].to_vec()
}

// ---------------------------------------------------------------- ações

#[derive(Debug, Clone)]
pub struct ToolAction {
    pub tool: &'static str,
    /// O que sai (nome do modelo, toolchain, ou classe do prune).
    pub subject: String,
    /// argv SEM o binário (resolvido na execução).
    pub argv: Vec<String>,
    /// Como regenerar — vai para o ledger.
    pub hint: String,
    pub bytes: u64,
    /// Último uso, se soubermos (para idade no ledger).
    pub used: Option<SystemTime>,
}

/// docker/orbstack prune: a construção NUNCA inclui volumes ou o subcomando
/// `volume` — volumes de dados são intocáveis por construção.
pub fn docker_action() -> ToolAction {
    ToolAction {
        tool: "docker",
        subject: "docker system prune".into(),
        argv: vec!["system".into(), "prune".into(), "--force".into()],
        hint: "docker pull/build recria imagens e camadas".into(),
        bytes: 0,
        used: None,
    }
}

pub fn pnpm_action() -> ToolAction {
    ToolAction {
        tool: "pnpm",
        subject: "pnpm store prune".into(),
        argv: vec!["store".into(), "prune".into()],
        hint: "pnpm install baixa de volta o que for necessário".into(),
        bytes: 0,
        used: None,
    }
}

pub fn go_action() -> ToolAction {
    ToolAction {
        tool: "go",
        subject: "go clean -modcache".into(),
        argv: vec!["clean".into(), "-modcache".into()],
        hint: "go mod download reconstroi o cache de módulos".into(),
        bytes: 0,
        used: None,
    }
}

pub fn ollama_actions(models: &[&OllamaModel]) -> Vec<ToolAction> {
    models
        .iter()
        .map(|m| ToolAction {
            tool: "ollama",
            subject: m.name.clone(),
            argv: vec!["rm".into(), m.name.clone()],
            hint: format!("ollama pull {}", m.name),
            bytes: m.bytes,
            used: Some(m.used),
        })
        .collect()
}

pub fn rustup_actions(tcs: &[&Toolchain]) -> Vec<ToolAction> {
    tcs.iter()
        .map(|t| ToolAction {
            tool: "rustup",
            subject: t.name.clone(),
            argv: vec!["toolchain".into(), "uninstall".into(), t.name.clone()],
            hint: format!("rustup toolchain install {}", t.name),
            bytes: 0,
            used: None,
        })
        .collect()
}

// --------------------------------------------------------------- execução

/// Caminho dos binários (produção: nomes resolvidos pelo PATH; testes:
/// fakes injetáveis).
#[derive(Debug, Clone)]
pub struct Bins {
    pub ollama: OsString,
    pub docker: OsString,
    pub rustup: OsString,
    pub pnpm: OsString,
    pub go: OsString,
}

impl Default for Bins {
    fn default() -> Self {
        Self {
            ollama: "ollama".into(),
            docker: "docker".into(),
            rustup: "rustup".into(),
            pnpm: "pnpm".into(),
            go: "go".into(),
        }
    }
}

pub enum ToolPlanOutcome {
    Disabled,
    AtTarget,
    Missing,
    Failed(String),
    Plan(Vec<ToolAction>),
}

pub struct ToolPlan {
    pub tool: &'static str,
    pub outcome: ToolPlanOutcome,
}

pub enum ListErr {
    Missing(String),
    Failed(String),
}

fn list_output(bin: &OsString, args: &[&str]) -> Result<String, ListErr> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| ListErr::Missing(format!("binário indisponível: {e}")))?;
    if !out.status.success() {
        return Err(ListErr::Failed(format!(
            "exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Levanta o plano de todas as classes habilitadas. `free` decide o alvo
/// (need = marca alta − livres; na meta → nada a fazer).
pub fn gather(cfg: &Config, bins: &Bins, free: u64) -> Vec<ToolPlan> {
    let t: &ToolsCfg = &cfg.tools;
    let high = (cfg.until_free_gib * GIB as f64) as u64;
    let need = high.saturating_sub(free);
    let mut plans = Vec::new();

    if t.ollama {
        let outcome = if need == 0 {
            ToolPlanOutcome::AtTarget
        } else {
            match list_output(&bins.ollama, &["list"]) {
                Err(ListErr::Missing(_)) => ToolPlanOutcome::Missing,
                Err(ListErr::Failed(e)) => ToolPlanOutcome::Failed(e),
                Ok(list) => {
                    let (models, skipped) = parse_ollama_list(&list, SystemTime::now());
                    let sel = select_ollama(&models, need);
                    let mut acts = ollama_actions(&sel);
                    if skipped > 0 {
                        // linhas ilegíveis ficam de fora, mas o motivo é dito
                        for a in &mut acts {
                            a.hint = format!("{} [{} modelo(s) fora do plano: modified ilegível]", a.hint, skipped);
                        }
                    }
                    ToolPlanOutcome::Plan(acts)
                }
            }
        };
        plans.push(ToolPlan { tool: "ollama", outcome });
    }

    if t.rustup {
        let outcome = if need == 0 {
            ToolPlanOutcome::AtTarget
        } else {
            match list_output(&bins.rustup, &["toolchain", "list"]) {
                Err(ListErr::Missing(_)) => ToolPlanOutcome::Missing,
                Err(ListErr::Failed(e)) => ToolPlanOutcome::Failed(e),
                Ok(list) => ToolPlanOutcome::Plan(rustup_actions(&select_rustup(&parse_rustup_list(&list), t.rustup_keep))),
            }
        };
        plans.push(ToolPlan { tool: "rustup", outcome });
    }

    for (enabled, tool, bin, action) in [
        (t.docker, "docker", &bins.docker, docker_action()),
        (t.pnpm, "pnpm", &bins.pnpm, pnpm_action()),
        (t.go, "go", &bins.go, go_action()),
    ] {
        if !enabled {
            continue;
        }
        let outcome = if need == 0 {
            ToolPlanOutcome::AtTarget
        } else if Command::new(bin).arg("--version").output().is_err() {
            ToolPlanOutcome::Missing
        } else {
            ToolPlanOutcome::Plan(vec![action])
        };
        plans.push(ToolPlan { tool, outcome });
    }
    plans
}

#[derive(Debug)]
pub enum RunStatus {
    Missing,
    Failed(String),
    /// Executou; bytes confirmados (docker parseia stdout; ollama usa a
    /// estimativa do list; pnpm/go não reportam e ficam 0).
    Ran { reclaimed_bytes: u64 },
}

#[derive(Debug)]
pub struct RunOutcome {
    pub tool: &'static str,
    pub subject: String,
    pub status: RunStatus,
}

fn parse_docker_reclaimed(stdout: &str) -> u64 {
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix("Total reclaimed space:") {
            if let Some(b) = parse_size(rest) {
                return b;
            }
        }
    }
    0
}

/// Executa UMA ação via o CLI da ferramenta e grava a linha do ledger.
/// Fail-closed: binário sumiu → Missing (nada acontece, sem ledger).
pub fn run_action(bin: &OsString, a: &ToolAction, ledger: &Path) -> RunOutcome {
    let out = match Command::new(bin).args(&a.argv).output() {
        Err(_) => {
            return RunOutcome {
                tool: a.tool,
                subject: a.subject.clone(),
                status: RunStatus::Missing,
            }
        }
        Ok(o) => o,
    };
    if !out.status.success() {
        return RunOutcome {
            tool: a.tool,
            subject: a.subject.clone(),
            status: RunStatus::Failed(format!(
                "exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            )),
        };
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let reclaimed = if a.tool == "docker" {
        parse_docker_reclaimed(&stdout)
    } else {
        a.bytes
    };
    let line = crate::ledger::Line {
        ts: jiff::Timestamp::now().as_second(),
        iso: crate::fmt_util::now_iso(),
        action: "rm",
        class: a.tool.to_string(),
        path: a.subject.clone(),
        bytes: reclaimed,
        age_days: a
            .used
            .map(|u| age_days(SystemTime::now(), u))
            .unwrap_or(0.0),
        hint: a.hint.clone(),
    };
    if let Err(e) = crate::ledger::append(ledger, &line) {
        eprintln!("# ERRO ledger {e} (ação executada: {})", a.subject);
    }
    RunOutcome {
        tool: a.tool,
        subject: a.subject.clone(),
        status: RunStatus::Ran { reclaimed_bytes: reclaimed },
    }
}

pub fn execute(plans: &[ToolPlan], bins: &Bins, ledger: &Path) -> Vec<RunOutcome> {
    let mut out = Vec::new();
    for p in plans {
        if let ToolPlanOutcome::Plan(actions) = &p.outcome {
            for a in actions {
                let bin = match p.tool {
                    "ollama" => &bins.ollama,
                    "rustup" => &bins.rustup,
                    "docker" => &bins.docker,
                    "pnpm" => &bins.pnpm,
                    "go" => &bins.go,
                    _ => continue,
                };
                out.push(run_action(bin, a, ledger));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_parse_and_lru_selection_until_target() {
        let list = "NAME                ID              SIZE      MODIFIED\n\
                    gemma2:2b           b0f9d26f0f0f    5.4 GB    3 weeks ago\n\
                    llama3:8b           365c0bd4e139    4.7 GB    2 months ago\n\
                    qwen2:7b            2094ee0c1515    4.5 GB    5 days ago\n";
        let now = SystemTime::now();
        let (models, skipped) = parse_ollama_list(list, now);
        assert_eq!(skipped, 0);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].bytes, (5.4 * 1024.0 * 1024.0 * 1024.0) as u64);

        // need 9 GiB: mais velho primeiro — llama3 (2 meses) + gemma2 (3 sem)
        let sel = select_ollama(&models, 9 * GIB);
        assert_eq!(sel.iter().map(|m| m.name.clone()).collect::<Vec<_>>(), ["llama3:8b", "gemma2:2b"]);

        // need pequeno: só o mais velho
        let sel1 = select_ollama(&models, 1 * GIB);
        assert_eq!(sel1.len(), 1);
        assert_eq!(sel1[0].name, "llama3:8b");
    }

    #[test]
    fn ollama_rows_without_modified_fail_closed() {
        let list = "NAME       ID        SIZE\ngemma2:2b  b0f9d2   5.4 GB\n";
        let (models, skipped) = parse_ollama_list(list, SystemTime::now());
        assert!(models.is_empty());
        assert_eq!(skipped, 1, "sem MODIFIED legível não entra no plano");
    }

    #[test]
    fn rustup_keeps_newest_n_and_protects_channels_and_default() {
        let list = "1.74.0-aarch64-apple-darwin\n\
                    1.75.0-aarch64-apple-darwin\n\
                    1.76.0-aarch64-apple-darwin\n\
                    1.77.2-aarch64-apple-darwin\n\
                    stable-aarch64-apple-darwin (default)\n\
                    nightly-2024-03-01-aarch64-apple-darwin\n\
                    custom-toolchain\n";
        let tcs = parse_rustup_list(list);
        assert_eq!(tcs.len(), 7);
        let sel = select_rustup(&tcs, 2);
        assert_eq!(
            sel.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
            ["1.74.0-aarch64-apple-darwin", "1.75.0-aarch64-apple-darwin"],
            "mantém as 2 mais novas; canais e default ficam"
        );
        // keep ≥ tudo → nada sai
        assert!(select_rustup(&tcs, 10).is_empty());
    }

    #[test]
    fn docker_prune_never_touches_volumes() {
        let a = docker_action();
        assert_eq!(a.argv, vec!["system", "prune", "--force"]);
        assert!(
            !a.argv.iter().any(|x| x.contains("volume")),
            "o prune jamais inclui volumes"
        );
        assert!(!a.argv.contains(&"volume".to_string()));
    }

    #[test]
    fn prune_actions_shape() {
        assert_eq!(pnpm_action().argv, vec!["store", "prune"]);
        assert_eq!(go_action().argv, vec!["clean", "-modcache"]);
    }

    #[test]
    fn docker_reclaimed_parse() {
        assert_eq!(
            parse_docker_reclaimed("Deleted Images:\n...\nTotal reclaimed space: 1.5GB"),
            (1.5 * 1024.0 * 1024.0 * 1024.0) as u64
        );
        assert_eq!(parse_docker_reclaimed("nada"), 0);
    }

    #[test]
    fn missing_binary_is_fail_closed_no_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = tmp.path().join("ledger.jsonl");
        let bins = Bins {
            ollama: tmp.path().join("binario-que-nao-existe").into_os_string(),
            ..Bins::default()
        };
        let models = [OllamaModel { name: "m:1b".into(), bytes: 1000, used: SystemTime::now() }];
        let a = ollama_actions(&[&models[0]]).remove(0);
        let out = run_action(&bins.ollama, &a, &ledger);
        assert!(matches!(out.status, RunStatus::Missing));
        assert!(!ledger.exists(), "fail-closed não nasce ledger");
    }
}
