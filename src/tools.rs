//! Tool classes (P1.3): eviction extended to things that are not build
//! directories — ollama models (LRU by modified time via `ollama rm`),
//! docker/orbstack prune (the command is constructed so it NEVER touches
//! volumes), old rustup toolchains (keeps the N newest), `pnpm store prune`, and
//! `go clean -modcache`.
//!
//! Split: SELECTION is pure logic (tested here); EXECUTION happens only
//! through the tool's own CLI, is dry by default (`--apply`), and is
//! fail-closed: a missing or broken tool is skipped and reported. Every
//! removal that runs writes a ledger line with a regeneration hint.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::config::{Config, ToolsCfg};
use crate::fmt_util::{age_days, GIB};

// ---------------------------------------------------------------- selection

#[derive(Debug, Clone, PartialEq)]
pub struct OllamaModel {
    pub name: String,
    pub bytes: u64,
    /// Last modification (MODIFIED column of `ollama list`).
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

/// Parse `ollama list`. Lines without a readable MODIFIED are FAIL-CLOSED:
/// they stay out of the plan (we never remove what we cannot measure).
/// Returns (parsed models, discarded lines).
pub fn parse_ollama_list(output: &str, now: SystemTime) -> (Vec<OllamaModel>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for line in output.lines() {
        let tok: Vec<&str> = line.split_whitespace().collect();
        if tok.is_empty() || tok[0] == "NAME" {
            continue;
        }
        // expected format: NAME ID SIZE "N unit ago" (SIZE may contain a space:
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

/// LRU by modified: oldest first until `need_bytes` is covered.
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
    /// Semver of the name (None = channel/rolling → protected).
    pub version: Option<(u64, u64, u64)>,
}

fn parse_version(name: &str) -> Option<(u64, u64, u64)> {
    let head = name.split('-').next()?;
    let mut it = head.split('.');
    let a: u64 = it.next()?.parse().ok()?;
    let b: u64 = it.next()?.parse().ok()?;
    let c: u64 = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((a, b, c))
}

fn parse_elan_version(name: &str) -> Option<(u64, u64, u64)> {
    let tag = name.rsplit(':').next().unwrap_or(name);
    let clean = tag.strip_prefix('v').unwrap_or(tag);
    if clean.contains('-') {
        return None;
    }
    let mut it = clean.split('.');
    let a: u64 = it.next()?.parse().ok()?;
    let b: u64 = it.next()?.parse().ok()?;
    let c: u64 = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((a, b, c))
}

/// Parse `rustup toolchain list` (lines "N.N.N-triple" with an optional
/// "(default)" marker).
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
                default: l.contains("(default)") || l.contains("(override)"),
                version,
            })
        })
        .collect()
}

/// Parse `elan toolchain list` (lines like `leanprover/lean4:v4.31.0` with
/// optional `(default)` / `(override)` markers).
pub fn parse_elan_list(output: &str) -> Vec<Toolchain> {
    output
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with("no installed") {
                return None;
            }
            let name = l.split_whitespace().next()?.to_string();
            let version = parse_elan_version(&name);
            Some(Toolchain {
                name,
                default: l.contains("(default)") || l.contains("(override)"),
                version,
            })
        })
        .collect()
}

fn rustup_matches_pin(tc_name: &str, pins: &std::collections::HashSet<String>) -> bool {
    pins.iter()
        .any(|pin| tc_name == pin || tc_name.starts_with(&format!("{pin}-")))
}

fn elan_matches_pin(tc_name: &str, pins: &std::collections::HashSet<String>) -> bool {
    if pins.contains(tc_name) {
        return true;
    }
    if let Some(tag) = tc_name.rsplit(':').next() {
        if pins.contains(tag) || pins.contains(tag.strip_prefix('v').unwrap_or(tag)) {
            return true;
        }
    }
    false
}

/// Discover `rust-toolchain(.toml)` and `lean-toolchain` pins inside `cfg.artifact_roots`
/// (up to depth 4, skipping artifact/pruned/dot directories) so active repos never lose
/// their pinned compiler version even when newer toolchains are installed.
pub fn discover_pinned_toolchains(
    cfg: &Config,
) -> (std::collections::HashSet<String>, std::collections::HashSet<String>) {
    let mut rust_pins = std::collections::HashSet::new();
    let mut lean_pins = std::collections::HashSet::new();
    let art_names: std::collections::HashSet<&str> =
        cfg.artifact_names.iter().map(|s| s.as_str()).collect();

    for raw_root in &cfg.artifact_roots {
        let root = crate::config::expand(raw_root);
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((root, 0usize));
        while let Some((dir, depth)) = queue.pop_front() {
            if let Ok(raw) = std::fs::read_to_string(dir.join("lean-toolchain")) {
                if let Some(line) = raw.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')) {
                    lean_pins.insert(line.to_string());
                }
            }
            if let Ok(raw) = std::fs::read_to_string(dir.join("rust-toolchain.toml")) {
                if let Ok(val) = toml::from_str::<toml::Value>(&raw) {
                    if let Some(ch) = val
                        .get("toolchain")
                        .and_then(|t| t.get("channel"))
                        .and_then(|c| c.as_str())
                    {
                        rust_pins.insert(ch.trim().to_string());
                    }
                }
            } else if let Ok(raw) = std::fs::read_to_string(dir.join("rust-toolchain")) {
                if let Some(line) = raw
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('['))
                {
                    rust_pins.insert(line.to_string());
                }
            }
            if depth >= 4 {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                if ft.is_symlink() || !ft.is_dir() {
                    continue;
                }
                let os_name = entry.file_name();
                let Some(name) = os_name.to_str() else {
                    continue;
                };
                if name.starts_with('.')
                    || crate::config::PRUNE_DIRS.contains(&name)
                    || art_names.contains(name)
                {
                    continue;
                }
                queue.push_back((entry.path(), depth + 1));
            }
        }
    }
    (rust_pins, lean_pins)
}

/// Keep the `keep` newest VERSIONED toolchains (and everything that is a
/// rolling channel, custom, or default). The rest leaves.
pub fn select_rustup(tcs: &[Toolchain], keep: usize) -> Vec<&Toolchain> {
    select_rustup_pinned(tcs, keep, &std::collections::HashSet::new())
}

/// Like `select_rustup`, additionally protecting any version pinned in a workspace
/// `rust-toolchain` / `rust-toolchain.toml`.
pub fn select_rustup_pinned<'a>(
    tcs: &'a [Toolchain],
    keep: usize,
    pins: &std::collections::HashSet<String>,
) -> Vec<&'a Toolchain> {
    let mut versioned: Vec<&Toolchain> = tcs
        .iter()
        .filter(|t| t.version.is_some())
        .collect();
    versioned.sort_by_key(|t| t.version);
    let cutoff = versioned.len().saturating_sub(keep);
    versioned[..cutoff]
        .iter()
        .copied()
        .filter(|t| !t.default && !rustup_matches_pin(&t.name, pins))
        .collect()
}

/// Keep the `keep` newest VERSIONED Lean (elan) toolchains, plus rolling channels,
/// `(default)` / `(override)`, and any toolchain pinned in a workspace `lean-toolchain`.
pub fn select_elan<'a>(
    tcs: &'a [Toolchain],
    keep: usize,
    pins: &std::collections::HashSet<String>,
) -> Vec<&'a Toolchain> {
    let mut versioned: Vec<&Toolchain> = tcs
        .iter()
        .filter(|t| t.version.is_some())
        .collect();
    versioned.sort_by_key(|t| t.version);
    let cutoff = versioned.len().saturating_sub(keep);
    versioned[..cutoff]
        .iter()
        .copied()
        .filter(|t| !t.default && !elan_matches_pin(&t.name, pins))
        .collect()
}

// ---------------------------------------------------------------- actions

#[derive(Debug, Clone)]
pub struct ToolAction {
    pub tool: &'static str,
    /// What leaves (model name, toolchain, or prune class).
    pub subject: String,
    /// argv WITHOUT the binary (resolved at execution).
    pub argv: Vec<String>,
    /// How to regenerate — goes into the ledger.
    pub hint: String,
    pub bytes: u64,
    /// Last use, if we know it (for the age in the ledger).
    pub used: Option<SystemTime>,
}

/// docker/orbstack prune: the construction NEVER includes volumes or the
/// `volume` subcommand — data volumes are untouchable by construction.
pub fn docker_action() -> ToolAction {
    ToolAction {
        tool: "docker",
        subject: "docker system prune".into(),
        argv: vec!["system".into(), "prune".into(), "--force".into()],
        hint: "docker pull/build recreates images and layers".into(),
        bytes: 0,
        used: None,
    }
}

pub fn pnpm_action() -> ToolAction {
    ToolAction {
        tool: "pnpm",
        subject: "pnpm store prune".into(),
        argv: vec!["store".into(), "prune".into()],
        hint: "pnpm install fetches whatever is needed again".into(),
        bytes: 0,
        used: None,
    }
}

pub fn go_action() -> ToolAction {
    ToolAction {
        tool: "go",
        subject: "go clean -modcache".into(),
        argv: vec!["clean".into(), "-modcache".into()],
        hint: "go mod download rebuilds the module cache".into(),
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

pub fn elan_actions(tcs: &[&Toolchain]) -> Vec<ToolAction> {
    tcs.iter()
        .map(|t| ToolAction {
            tool: "elan",
            subject: t.name.clone(),
            argv: vec!["toolchain".into(), "uninstall".into(), t.name.clone()],
            hint: format!("elan toolchain install {}", t.name),
            bytes: 0,
            used: None,
        })
        .collect()
}

// --------------------------------------------------------------- execution

/// Paths of the binaries (production: names resolved via PATH; tests:
/// injectable fakes).
#[derive(Debug, Clone)]
pub struct Bins {
    pub ollama: OsString,
    pub docker: OsString,
    pub rustup: OsString,
    pub elan: OsString,
    pub pnpm: OsString,
    pub go: OsString,
}

impl Default for Bins {
    fn default() -> Self {
        Self {
            ollama: "ollama".into(),
            docker: "docker".into(),
            rustup: "rustup".into(),
            elan: "elan".into(),
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
        .map_err(|e| ListErr::Missing(format!("binary unavailable: {e}")))?;
    if !out.status.success() {
        return Err(ListErr::Failed(format!(
            "exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Build the plan of every enabled class. `free` decides the target
/// (need = high mark − free; at the target → nothing to do).
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
                        // unreadable lines stay out, but the reason is stated
                        for a in &mut acts {
                            a.hint = format!("{} [{} model(s) left out of the plan: unreadable modified]", a.hint, skipped);
                        }
                    }
                    ToolPlanOutcome::Plan(acts)
                }
            }
        };
        plans.push(ToolPlan { tool: "ollama", outcome });
    }

    let (rust_pins, lean_pins) = if need > 0 && (t.rustup || t.elan) {
        discover_pinned_toolchains(cfg)
    } else {
        (std::collections::HashSet::new(), std::collections::HashSet::new())
    };

    if t.rustup {
        let outcome = if need == 0 {
            ToolPlanOutcome::AtTarget
        } else {
            match list_output(&bins.rustup, &["toolchain", "list"]) {
                Err(ListErr::Missing(_)) => ToolPlanOutcome::Missing,
                Err(ListErr::Failed(e)) => ToolPlanOutcome::Failed(e),
                Ok(list) => ToolPlanOutcome::Plan(rustup_actions(&select_rustup_pinned(
                    &parse_rustup_list(&list),
                    t.rustup_keep,
                    &rust_pins,
                ))),
            }
        };
        plans.push(ToolPlan { tool: "rustup", outcome });
    }

    if t.elan {
        let outcome = if need == 0 {
            ToolPlanOutcome::AtTarget
        } else {
            match list_output(&bins.elan, &["toolchain", "list"]) {
                Err(ListErr::Missing(_)) => ToolPlanOutcome::Missing,
                Err(ListErr::Failed(e)) => ToolPlanOutcome::Failed(e),
                Ok(list) => ToolPlanOutcome::Plan(elan_actions(&select_elan(
                    &parse_elan_list(&list),
                    t.elan_keep,
                    &lean_pins,
                ))),
            }
        };
        plans.push(ToolPlan { tool: "elan", outcome });
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
    /// Ran; confirmed bytes (docker parses stdout; ollama uses the
    /// list estimate; pnpm/go do not report and stay 0).
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

/// Run ONE action through the tool's own CLI and write the ledger line.
/// Fail-closed: binary gone → Missing (nothing happens, no ledger).
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
        eprintln!("# LEDGER ERROR {e} (action already ran: {})", a.subject);
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
                    "elan" => &bins.elan,
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

        // need 9 GiB: oldest first — llama3 (2 months) + gemma2 (3 weeks)
        let sel = select_ollama(&models, 9 * GIB);
        assert_eq!(sel.iter().map(|m| m.name.clone()).collect::<Vec<_>>(), ["llama3:8b", "gemma2:2b"]);

        // small need: only the oldest
        let sel1 = select_ollama(&models, GIB);
        assert_eq!(sel1.len(), 1);
        assert_eq!(sel1[0].name, "llama3:8b");
    }

    #[test]
    fn ollama_rows_without_modified_fail_closed() {
        let list = "NAME       ID        SIZE\ngemma2:2b  b0f9d2   5.4 GB\n";
        let (models, skipped) = parse_ollama_list(list, SystemTime::now());
        assert!(models.is_empty());
        assert_eq!(skipped, 1, "without a readable MODIFIED it does not enter the plan");
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
            "keeps the 2 newest; channels and default stay"
        );
        // keep ≥ everything → nothing leaves
        assert!(select_rustup(&tcs, 10).is_empty());
    }

    #[test]
    fn elan_and_rustup_protect_workspace_pinned_toolchains() {
        let tmp = tempfile::tempdir().unwrap();
        let sw = tmp.path().join("software");
        std::fs::create_dir_all(sw.join("aeneas/backends/lean")).unwrap();
        std::fs::write(sw.join("aeneas/backends/lean/lean-toolchain"), "leanprover/lean4:v4.31.0\n").unwrap();
        std::fs::create_dir_all(sw.join("forja")).unwrap();
        std::fs::write(
            sw.join("forja/rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.74.0\"\n",
        )
        .unwrap();

        let cfg = Config {
            artifact_roots: vec![sw],
            ..Config::default()
        };
        let (rust_pins, lean_pins) = discover_pinned_toolchains(&cfg);
        assert!(lean_pins.contains("leanprover/lean4:v4.31.0"));
        assert!(rust_pins.contains("1.74.0"));

        let elan_out = "leanprover/lean4:v4.31.0\n\
                        leanprover/lean4:v4.33.0\n\
                        leanprover/lean4:v4.34.0\n\
                        leanprover/lean4:v4.34.1\n\
                        leanprover/lean4:v4.35.0 (default)\n\
                        leanprover/lean4:nightly-2026-10-01\n";
        let elan_tcs = parse_elan_list(elan_out);
        let elan_sel = select_elan(&elan_tcs, 2, &lean_pins);
        assert_eq!(
            elan_sel.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["leanprover/lean4:v4.33.0", "leanprover/lean4:v4.34.0"],
            "v4.31.0 stays (repo-pinned), v4.34.1 + v4.35.0 stay (2 newest), nightly stays"
        );

        let rust_out = "1.74.0-aarch64-apple-darwin\n\
                        1.75.0-aarch64-apple-darwin\n\
                        1.76.0-aarch64-apple-darwin\n\
                        1.77.2-aarch64-apple-darwin\n";
        let rust_tcs = parse_rustup_list(rust_out);
        let rust_sel = select_rustup_pinned(&rust_tcs, 2, &rust_pins);
        assert_eq!(
            rust_sel.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["1.75.0-aarch64-apple-darwin"],
            "1.74.0 stays (repo-pinned), 1.76.0 + 1.77.2 stay (2 newest)"
        );
    }

    #[test]
    fn docker_prune_never_touches_volumes() {
        let a = docker_action();
        assert_eq!(a.argv, vec!["system", "prune", "--force"]);
        assert!(
            !a.argv.iter().any(|x| x.contains("volume")),
            "prune never includes volumes"
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
        assert!(!ledger.exists(), "fail-closed does not create a ledger");
    }
}
