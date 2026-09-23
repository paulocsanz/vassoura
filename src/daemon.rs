//! Daemon (P1.1): loop de poll com marca d'água e HISTERESE.
//! livre ≥ marca baixa (ou entre as marcas) → nada acontece; livre < baixa →
//! evicção LRU (mais velho primeiro) limitada por ciclo (bound de bytes +
//! itens) até livre ≥ marca alta ou esgotar o bound; rate-limit entre ciclos
//! destrutivos. Toda remoção grava linha no ledger (o ponto único é o
//! `clean::apply`).

use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::clean;
use crate::config::Config;
use crate::fmt_util::{human, GIB};
use crate::plan;
use crate::seen::SeenDb;
use crate::statusfs::{self, StatusReport};
use crate::walk;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleAction {
    /// Nada a fazer: livre ≥ marca baixa (ou já ≥ alta).
    Idle,
    /// Evictar em LRU até `need_bytes` ou esgotar o bound do ciclo.
    Evict { need_bytes: u64, cap_bytes: u64, cap_items: usize },
}

/// Decisão pura de histerese do ciclo.
pub fn decide_cycle(free: u64, low_bytes: u64, high_bytes: u64, cap_bytes: u64, cap_items: usize) -> CycleAction {
    if free >= low_bytes {
        return CycleAction::Idle;
    }
    let need = high_bytes.saturating_sub(free);
    if need == 0 {
        // marcas invertidas ou já na meta alta: nada a fazer
        return CycleAction::Idle;
    }
    CycleAction::Evict { need_bytes: need, cap_bytes, cap_items }
}

/// Rate-limit: houve evicção há menos de `rate_limit` → segura este ciclo.
pub fn rate_limited(last_eviction: Option<SystemTime>, now: SystemTime, rate_limit: Duration) -> bool {
    match last_eviction {
        None => false,
        Some(t) => now.duration_since(t).map(|d| d < rate_limit).unwrap_or(false),
    }
}

#[derive(Debug, Default)]
pub struct CycleReport {
    pub free_before: u64,
    pub free_after: u64,
    pub scanned: usize,
    pub eligible_bytes: u64,
    pub action: Option<CycleAction>,
    pub rate_limited: bool,
    pub removed: usize,
    pub freed: u64,
    pub skipped: Vec<(std::path::PathBuf, String)>,
}

/// Um ciclo do daemon: statfs → scan → seen.db → statusfs → decisão →
/// (evicção bounded com os gates de uso) → ledger.
pub fn run_cycle(cfg: &Config) -> CycleReport {
    let mut rep = CycleReport::default();
    let d = disk_of(cfg);
    rep.free_before = d.free;

    let cands = walk::scan(cfg, None, true);
    rep.scanned = cands.len();

    // seen.db (P2.1): LRU por último-visto, persistido a cada ciclo
    let mut seen = SeenDb::load(&crate::config::expand(&cfg.seen_db));
    seen.refresh(&cands, SystemTime::now());

    let (items, _rejected) = plan::build_seen(cands, cfg, None, &seen);
    rep.eligible_bytes = items.iter().map(|i| i.cand.bytes).sum();

    // fs de status (P2.2): mesmos valores de `status --json`
    let report: StatusReport =
        statusfs::build_report(d, cfg, rep.eligible_bytes, items.len());
    if let Err(e) = statusfs::write_status_dir(&crate::config::expand(&cfg.status_dir), &report) {
        eprintln!("# aviso: statusfs {}: {e}", cfg.status_dir.display());
    }

    let low = (cfg.low_watermark_gib * GIB as f64) as u64;
    let high = (cfg.until_free_gib * GIB as f64) as u64;
    let cap_bytes = (cfg.daemon.max_bytes_per_cycle_gib * GIB as f64) as u64;
    let action = decide_cycle(d.free, low, high, cap_bytes, cfg.daemon.max_items_per_cycle);
    rep.action = Some(action.clone());
    if let CycleAction::Evict { need_bytes, cap_bytes, cap_items } = action {
        let rl = Duration::from_secs(cfg.daemon.rate_limit_secs);
        if rate_limited(seen.last_eviction, SystemTime::now(), rl) {
            rep.rate_limited = true;
        } else if need_bytes > 0 {
            let packed = plan::pack_capped(items, d.free, cfg.until_free_gib, cap_items, Some(cap_bytes));
            let out = clean::apply(&packed.items, &crate::config::expand(&cfg.ledger));
            rep.removed = out.removed;
            rep.freed = out.freed;
            if out.removed > 0 {
                seen.mark_eviction(SystemTime::now());
                if cfg.daemon.notify {
                    let removed: Vec<&Path> = packed
                        .items
                        .iter()
                        .map(|i| i.cand.path.as_path())
                        .filter(|p| !out.skipped.iter().any(|(sp, _)| sp == p))
                        .collect();
                    notify(
                        "vassoura",
                        &evicted_body(&removed, out.freed),
                        Some(&crate::config::expand(&cfg.ledger)),
                    );
                }
            }
            rep.skipped = out.skipped;
        }
    }

    if let Err(e) = seen.persist(&crate::config::expand(&cfg.seen_db)) {
        eprintln!("# aviso: seen.db {}: {e}", cfg.seen_db.display());
    }

    rep.free_after = disk_of(cfg).free;
    rep
}

/// Disco da primeira raiz da allowlist (fallback: /).
pub fn disk_of(cfg: &Config) -> crate::disk::Disk {
    let p = crate::config::expand(
        cfg.artifact_roots.first().map(|r| r.as_path()).unwrap_or_else(|| Path::new("/")),
    );
    crate::disk::Disk::snapshot(&p).unwrap_or_else(|| crate::disk::Disk::snapshot(Path::new("/")).expect("statfs de /"))
}

/// Notificação macOS best-effort (falha silenciosa: é cortesia, não gate).
/// Se `open` for dado e o `terminal-notifier` existir, o CLIQUE abre esse
/// caminho (o ledger); o osascript puro não permite vincular ação ao clique
/// (o macOS ativa o app que postou — Script Editor), então o fallback põe o
/// caminho no corpo da mensagem.
pub fn notify(title: &str, body: &str, open: Option<&Path>) {
    if let Some(target) = open {
        let url = format!("file://{}", target.display());
        let via_tn = Command::new("terminal-notifier")
            .args(["-title", title, "-message", body, "-open", &url])
            .status();
        if via_tn.is_ok() {
            return;
        }
    }
    let body = match open {
        Some(p) => format!("{body} — detalhes: {}", p.display()),
        None => body.to_string(),
    };
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(format!(
            "display notification {} with title {}",
            quote_os(&body),
            quote_os(title)
        ))
        .status();
}

/// Corpo da notificação de evicção: O QUE saiu (até 3 caminhos + contagem)
/// e quanto liberou.
pub fn evicted_body(paths: &[&Path], freed: u64) -> String {
    let shown: Vec<String> = paths.iter().take(3).map(|p| p.display().to_string()).collect();
    let extra = paths.len().saturating_sub(shown.len());
    let list = match extra {
        0 => shown.join(", "),
        n => format!("{} (+{n})", shown.join(", ")),
    };
    format!("removido: {list} · {} liberados", human(freed))
}

/// Aspas duplas escapadas para AppleScript (nossas strings são simples).
fn quote_os(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = GIB;

    #[test]
    fn idle_when_free_at_or_above_low() {
        assert_eq!(decide_cycle(50 * G, 40 * G, 100 * G, 20 * G, 30), CycleAction::Idle);
        assert_eq!(decide_cycle(40 * G, 40 * G, 100 * G, 20 * G, 30), CycleAction::Idle);
        assert_eq!(decide_cycle(150 * G, 40 * G, 100 * G, 20 * G, 30), CycleAction::Idle);
    }

    #[test]
    fn evicts_below_low_until_high() {
        assert_eq!(
            decide_cycle(30 * G, 40 * G, 100 * G, 20 * G, 30),
            CycleAction::Evict { need_bytes: 70 * G, cap_bytes: 20 * G, cap_items: 30 }
        );
    }

    #[test]
    fn inverted_or_satisfied_marks_are_idle() {
        // free < low mas já ≥ high (marcas invertidas): não remove nada
        assert_eq!(decide_cycle(50 * G, 60 * G, 40 * G, 20 * G, 30), CycleAction::Idle);
    }

    #[test]
    fn rate_limit_holds_recent_cycles_only() {
        let now = SystemTime::now();
        let rl = Duration::from_secs(900);
        assert!(!rate_limited(None, now, rl));
        assert!(rate_limited(Some(now), now, rl));
        assert!(rate_limited(Some(now - Duration::from_secs(600)), now, rl));
        assert!(!rate_limited(Some(now - Duration::from_secs(901)), now, rl));
    }

    #[test]
    fn evicted_body_names_what_left() {
        let a = Path::new("/p/a/node_modules");
        let b = Path::new("/p/b/node_modules");
        let c = Path::new("/p/c/target");
        let body = evicted_body(&[a, b], 4096);
        assert!(body.contains("/p/a/node_modules"), "{body}");
        assert!(body.contains("/p/b/node_modules"), "{body}");
        assert!(body.contains("liberados"), "{body}");

        let many = evicted_body(&[a, b, c, Path::new("/p/d/dist")], 4096);
        assert!(many.contains("(+1)"), "mais que 3 resume a contagem: {many}");
    }

    #[test]
    fn quote_os_escapes() {
        assert_eq!(quote_os("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }
}
