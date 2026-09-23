use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use vassoura::fmt_util::{self, human, GIB};
use vassoura::{clean, config, daemon, disk, plan, statusfs, tools, walk};

#[derive(Parser)]
#[command(
    name = "vassoura",
    version,
    about = "Coletor de build-lixo com marca d'água — o disco nunca enche",
    long_about = None
)]
struct Cli {
    /// Caminho do config (default ~/.vassoura/config.toml, criado se ausente)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Cataloga diretórios regeneráveis (tamanho + idade + classe)
    Scan {
        /// Restringe o scan a este subdiretório
        #[arg(long)]
        root: Option<PathBuf>,
        /// Quantidade de linhas na tabela
        #[arg(long, default_value_t = 25)]
        top: usize,
        /// Saída JSON (uma linha por candidato)
        #[arg(long)]
        json: bool,
    },
    /// Disco, marcas d'água e quanto dá pra recuperar agora
    Status {
        #[arg(long, default_value_t = 10)]
        top: usize,
        /// Saída JSON (disco/marcas/veredito/elegível)
        #[arg(long)]
        json: bool,
    },
    /// Plano de limpeza LRU até a meta de espaço livre (dry-run por default)
    Clean {
        #[arg(long)]
        root: Option<PathBuf>,
        /// Executa de fato (default: só mostra o plano)
        #[arg(long)]
        apply: bool,
        /// Confirma sem perguntar (obrigatório se stdin não for terminal)
        #[arg(long)]
        yes: bool,
        /// Sobrepõe a idade mínima (dias) de todas as classes
        #[arg(long, value_name = "DIAS")]
        older_than: Option<u64>,
        /// Meta de espaço livre em GiB
        #[arg(long, value_name = "GIB")]
        until_free: Option<f64>,
        /// Máximo de itens no plano
        #[arg(long, default_value_t = 60)]
        top: usize,
    },
    /// Loop com marca d'água: histerese low/high, evicção LRU bounded por ciclo
    Daemon {
        /// Roda UM ciclo e sai (exercitável de fora)
        #[arg(long)]
        once: bool,
    },
    /// Classes de ferramenta (ollama/docker/rustup/pnpm/go) — dry-run por default
    Tools {
        /// Executa de fato via o CLI de cada ferramenta
        #[arg(long)]
        apply: bool,
        /// Confirma sem perguntar (obrigatório se stdin não for terminal)
        #[arg(long)]
        yes: bool,
    },
    /// Escreve o diretório de status (um arquivo por métrica) com os valores de `status`
    Refresh,
    /// Instala o LaunchAgent do daemon (escreve o plist; carregar é com o operador)
    InstallDaemon,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (cfg, cfg_path) = match config::load(cli.config.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("erro: {e}");
            return ExitCode::FAILURE;
        }
    };
    let r = match &cli.cmd {
        Cmd::Scan { root, top, json } => cmd_scan(&cfg, root.as_deref(), *top, *json),
        Cmd::Status { top, json } => cmd_status(&cfg, *top, *json),
        Cmd::Clean { root, apply, yes, older_than, until_free, top } => {
            cmd_clean(&cfg, root.as_deref(), *apply, *yes, *older_than, *until_free, *top)
        }
        Cmd::Daemon { once } => cmd_daemon(&cfg, *once),
        Cmd::Tools { apply, yes } => cmd_tools(&cfg, *apply, *yes),
        Cmd::Refresh => cmd_refresh(&cfg),
        Cmd::InstallDaemon => cmd_install_daemon(&cfg, &cfg_path),
    };
    match r {
        Ok(code) => code,
        Err(e) => {
            eprintln!("erro: {e}");
            ExitCode::FAILURE
        }
    }
}

fn disk_of(cfg: &config::Config) -> disk::Disk {
    daemon::disk_of(cfg)
}

fn cmd_scan(cfg: &config::Config, root: Option<&std::path::Path>, top: usize, json: bool) -> Result<ExitCode, String> {
    let cands = walk::scan(cfg, root, false);
    if json {
        for c in &cands {
            let v = serde_json::json!({
                "path": c.path.display().to_string(),
                "class": c.class.label(),
                "bytes": c.bytes,
                "age_days": (fmt_util::age_days(std::time::SystemTime::now(), c.newest_mtime) * 10.0).round() / 10.0,
                "contains_git": c.contains_git,
            });
            println!("{v}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let mut sorted = cands.clone();
    sorted.sort_by_key(|c| std::cmp::Reverse(c.bytes));
    println!("{:<10} {:>7}  {:<9} {:<4} CAMINHO", "TAM", "IDADE(d)", "CLASSE", "GIT");
    for c in sorted.iter().take(top) {
        let age = fmt_util::age_days(std::time::SystemTime::now(), c.newest_mtime);
        println!(
            "{:<10} {:>7.0}  {:<9} {:<4} {}",
            human(c.bytes),
            age,
            c.class.label(),
            if c.contains_git { "SIM" } else { "" },
            c.path.display()
        );
    }
    let total: u64 = cands.iter().map(|c| c.bytes).sum();
    let protected: u64 = cands.iter().filter(|c| c.contains_git).map(|c| c.bytes).sum();
    println!(
        "\n{} candidatos, {} no total ({:.1} GiB); {:.1} GiB protegidos por .git interno",
        cands.len(),
        human(total),
        total as f64 / GIB as f64,
        protected as f64 / GIB as f64
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_status(cfg: &config::Config, top: usize, json: bool) -> Result<ExitCode, String> {
    let d = disk_of(cfg);
    let low = (cfg.low_watermark_gib * GIB as f64) as u64;
    let high = (cfg.until_free_gib * GIB as f64) as u64;
    let cands = walk::scan(cfg, None, false);
    let (items, rejected) = plan::build(cands, cfg, None);
    let eligible: u64 = items.iter().map(|i| i.cand.bytes).sum();
    let report = statusfs::build_report(d, cfg, eligible, items.len());

    if json {
        println!("{}", serde_json::to_string(&report).expect("status serializa"));
        return Ok(ExitCode::SUCCESS);
    }

    println!("disco: {} livres de {} ({:.0}% usado)", human(d.free), human(d.total), d.used() as f64 / d.total as f64 * 100.0);
    println!(
        "marcas d'água: baixa {} GiB (gatilho do daemon) · meta alta {} GiB",
        cfg.low_watermark_gib, cfg.until_free_gib
    );
    let verdict = if d.free < low { "CRÍTICO — abaixo da marca baixa" } else if d.free < high { "APERTADO — cabe o plano do clean" } else { "OK — dentro da faixa" };
    println!("estado: {verdict}");

    let rej: u64 = rejected.iter().map(|r| r.bytes).sum();
    println!(
        "\nregenerável elegível agora: {:.1} GiB · fora do plano (jovem/.git): {:.1} GiB",
        eligible as f64 / GIB as f64,
        rej as f64 / GIB as f64
    );
    println!("faltam {} para a meta de {} GiB livres", human(report.need_bytes), cfg.until_free_gib);

    let mut by_age = items.clone();
    by_age.sort_by(|a, b| b.age_days.partial_cmp(&a.age_days).unwrap_or(std::cmp::Ordering::Equal));
    println!("\nmais velhos primeiro (ordem de evicção):");
    println!("{:<10} {:>7}  CAMINHO", "TAM", "IDADE(d)");
    for i in by_age.iter().take(top) {
        println!("{:<10} {:>7.0}  {}", human(i.cand.bytes), i.age_days, i.cand.path.display());
    }
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
fn cmd_clean(
    cfg: &config::Config,
    root: Option<&std::path::Path>,
    apply: bool,
    yes: bool,
    older_than: Option<u64>,
    until_free: Option<f64>,
    top: usize,
) -> Result<ExitCode, String> {
    let d = disk_of(cfg);
    let until = until_free.unwrap_or(cfg.until_free_gib);
    let cands = walk::scan(cfg, root, false);
    let (items, rejected) = plan::build(cands, cfg, older_than);
    let packed = plan::pack(items, d.free, until, top);

    println!("plano de evicção (LRU: mais velho primeiro) — meta: {until} GiB livres");
    println!("{:<10} {:>7}  {:<24} CAMINHO", "TAM", "IDADE(d)", "REGENERA");
    for i in &packed.items {
        println!("{:<10} {:>7.0}  {:<24} {}", human(i.cand.bytes), i.age_days, i.hint, i.cand.path.display());
    }
    if packed.items.is_empty() {
        println!("(sem elegíveis: tudo jovem demais, protegido por .git, ou já na meta)");
    }
    println!(
        "\nitens: {} · seria liberado: {} · faltava p/ meta: {} · {}",
        packed.items.len(),
        human(packed.planned_bytes),
        human(packed.need_bytes),
        if packed.reached { "meta atingível" } else { "meta NÃO atingível com estes itens/top" }
    );

    let mut rej_counts: std::collections::BTreeMap<String, (usize, u64)> = std::collections::BTreeMap::new();
    for r in &rejected {
        let key = r.reason.split(':').next().unwrap_or(&r.reason).trim().to_string();
        let e = rej_counts.entry(key).or_insert((0, 0));
        e.0 += 1;
        e.1 += r.bytes;
    }
    for (reason, (n, b)) in &rej_counts {
        println!("fora do plano: {reason} — {n} dirs, {}", human(*b));
    }

    if !apply {
        println!("\ndry-run (nada foi removido). Rode com --apply para executar.");
        return Ok(ExitCode::SUCCESS);
    }

    if packed.items.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }

    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err("stdin não é terminal: confirme com --yes (ou rode sem --apply para o dry-run)".into());
        }
        print!(
            "\nAplicar {} remoções liberando {}? [y/N] ",
            packed.items.len(),
            human(packed.planned_bytes)
        );
        let _ = std::io::stdout().flush();
        let mut ans = String::new();
        std::io::stdin().read_line(&mut ans).map_err(|e| e.to_string())?;
        if !ans.trim().eq_ignore_ascii_case("y") && !ans.trim().eq_ignore_ascii_case("sim") {
            println!("abortado — nada removido.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    let out = clean::apply(&packed.items, &config::expand(&cfg.ledger));
    let after = disk_of(cfg);
    println!(
        "\nremovidos: {} dirs · liberado: {} · livres agora: {} (era {})",
        out.removed,
        human(out.freed),
        human(after.free),
        human(d.free)
    );
    for (p, why) in &out.skipped {
        println!("pulado: {} — {why}", p.display());
    }
    println!("ledger: {}", cfg.ledger.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_daemon(cfg: &config::Config, once: bool) -> Result<ExitCode, String> {
    let interval = std::time::Duration::from_secs(cfg.watch_interval_secs.max(1));
    loop {
        let rep = daemon::run_cycle(cfg);
        print_cycle(cfg, &rep);
        if once {
            return Ok(ExitCode::SUCCESS);
        }
        std::thread::sleep(interval);
    }
}

fn print_cycle(cfg: &config::Config, rep: &daemon::CycleReport) {
    let action = match rep.action.as_ref().unwrap_or(&daemon::CycleAction::Idle) {
        daemon::CycleAction::Idle => "IDLE".to_string(),
        daemon::CycleAction::Evict { need_bytes, .. } => {
            format!("EVICT (precisa {})", human(*need_bytes))
        }
    };
    println!(
        "ciclo: {} candidatos · elegível {} · livres {} → {} · {}{}",
        rep.scanned,
        human(rep.eligible_bytes),
        human(rep.free_before),
        human(rep.free_after),
        action,
        if rep.rate_limited { " [rate-limited: segura o ciclo]" } else { "" }
    );
    if rep.removed > 0 {
        println!(
            "  removidos: {} · liberado: {} · ledger: {}",
            rep.removed,
            human(rep.freed),
            config::expand(&cfg.ledger).display()
        );
        for (p, why) in &rep.skipped {
            println!("  pulado: {} — {why}", p.display());
        }
    }
}

fn cmd_tools(cfg: &config::Config, apply: bool, yes: bool) -> Result<ExitCode, String> {
    let d = disk_of(cfg);
    let bins = tools::Bins::default();
    let plans = tools::gather(cfg, &bins, d.free);

    println!("classes de ferramenta — meta: {} GiB livres (livres agora: {})", cfg.until_free_gib, human(d.free));
    let mut any = false;
    for p in &plans {
        match &p.outcome {
            tools::ToolPlanOutcome::Disabled => println!("{:<8} desligada no config", p.tool),
            tools::ToolPlanOutcome::AtTarget => println!("{:<8} na meta — nada a fazer", p.tool),
            tools::ToolPlanOutcome::Missing => println!("{:<8} PULADA: ferramenta ausente (fail-closed)", p.tool),
            tools::ToolPlanOutcome::Failed(e) => println!("{:<8} PULADA: {e}", p.tool),
            tools::ToolPlanOutcome::Plan(actions) => {
                any = true;
                if actions.is_empty() {
                    println!("{:<8} plano vazio (nada velho/supérfluo)", p.tool);
                    continue;
                }
                for a in actions {
                    println!("{:<8} {} {:<40} {}", p.tool, a.argv.join(" "), a.subject, human(a.bytes));
                }
            }
        }
    }

    if !apply {
        if any {
            println!("\ndry-run (nada foi removido). Rode com --apply para executar.");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let planned_total: usize = plans
        .iter()
        .map(|p| match &p.outcome {
            tools::ToolPlanOutcome::Plan(a) => a.len(),
            _ => 0,
        })
        .sum();
    if planned_total == 0 {
        return Ok(ExitCode::SUCCESS);
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err("stdin não é terminal: confirme com --yes (ou rode sem --apply para o dry-run)".into());
        }
        print!("\nAplicar {planned_total} ações de ferramenta? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut ans = String::new();
        std::io::stdin().read_line(&mut ans).map_err(|e| e.to_string())?;
        if !ans.trim().eq_ignore_ascii_case("y") && !ans.trim().eq_ignore_ascii_case("sim") {
            println!("abortado — nada removido.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    let outs = tools::execute(&plans, &bins, &config::expand(&cfg.ledger));
    for o in &outs {
        match &o.status {
            tools::RunStatus::Missing => println!("pulado: {} — ferramenta ausente (fail-closed)", o.subject),
            tools::RunStatus::Failed(e) => println!("FALHOU: {} — {e}", o.subject),
            tools::RunStatus::Ran { reclaimed_bytes } => {
                println!("ok: {} — confirmado {}", o.subject, human(*reclaimed_bytes))
            }
        }
    }
    println!("ledger: {}", config::expand(&cfg.ledger).display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_refresh(cfg: &config::Config) -> Result<ExitCode, String> {
    let d = disk_of(cfg);
    let cands = walk::scan(cfg, None, false);
    let (items, _) = plan::build(cands, cfg, None);
    let eligible: u64 = items.iter().map(|i| i.cand.bytes).sum();
    let report = statusfs::build_report(d, cfg, eligible, items.len());
    let dir = config::expand(&cfg.status_dir);
    let written = statusfs::write_status_dir(&dir, &report).map_err(|e| e.to_string())?;
    println!(
        "{} arquivos em {} · veredito {} · elegível {} · livres {}",
        written.len(),
        dir.display(),
        report.verdict,
        human(report.eligible.bytes),
        human(report.disk.free_bytes)
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_install_daemon(cfg: &config::Config, cfg_path: &Path) -> Result<ExitCode, String> {
    let bin = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let path = vassoura::launchd::install(&bin, cfg_path, cfg.watch_interval_secs)?;
    println!("plist escrito: {}", path.display());
    println!(
        "para carregar (decisão do operador):\n  launchctl load {}",
        path.display()
    );
    println!("para descarregar:\n  launchctl unload {}", path.display());
    Ok(ExitCode::SUCCESS)
}
