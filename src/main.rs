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
    about = "Watermarked build-artifact collector — the disk never fills up",
    long_about = None
)]
struct Cli {
    /// Config path (default ~/.vassoura/config.toml, created if missing)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Catalog regenerable directories (size + age + class)
    Scan {
        /// Restrict the scan to this subdirectory
        #[arg(long)]
        root: Option<PathBuf>,
        /// How many rows to print
        #[arg(long, default_value_t = 25)]
        top: usize,
        /// JSON output (one line per candidate)
        #[arg(long)]
        json: bool,
    },
    /// Disk, watermarks, and how much is reclaimable now
    Status {
        #[arg(long, default_value_t = 10)]
        top: usize,
        /// JSON output (disk / watermarks / verdict / eligible)
        #[arg(long)]
        json: bool,
    },
    /// LRU cleanup plan up to the free-space target (dry-run by default)
    Clean {
        #[arg(long)]
        root: Option<PathBuf>,
        /// Actually delete (default: print the plan only)
        #[arg(long)]
        apply: bool,
        /// Skip the prompt (required when stdin is not a terminal)
        #[arg(long)]
        yes: bool,
        /// Override the minimum age (days) for every class
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,
        /// Free-space target in GiB
        #[arg(long, value_name = "GIB")]
        until_free: Option<f64>,
        /// Maximum items in the plan
        #[arg(long, default_value_t = 60)]
        top: usize,
    },
    /// Watermark loop: low/high hysteresis, LRU eviction bounded per cycle
    Daemon {
        /// Run ONE cycle and exit
        #[arg(long)]
        once: bool,
    },
    /// Tool classes (ollama/docker/rustup/pnpm/go) — dry-run by default
    Tools {
        /// Actually run each tool's own CLI
        #[arg(long)]
        apply: bool,
        /// Skip the prompt (required when stdin is not a terminal)
        #[arg(long)]
        yes: bool,
    },
    /// Write the status directory (one file per metric) with the values of `status`
    Refresh,
    /// Install the daemon LaunchAgent (writes the plist; loading is up to you)
    InstallDaemon,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (cfg, cfg_path) = match config::load(cli.config.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
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
            eprintln!("error: {e}");
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
    println!("{:<10} {:>7}  {:<9} {:<4} PATH", "SIZE", "AGE(d)", "CLASS", "GIT");
    for c in sorted.iter().take(top) {
        let age = fmt_util::age_days(std::time::SystemTime::now(), c.newest_mtime);
        println!(
            "{:<10} {:>7.0}  {:<9} {:<4} {}",
            human(c.bytes),
            age,
            c.class.label(),
            if c.contains_git { "YES" } else { "" },
            c.path.display()
        );
    }
    let total: u64 = cands.iter().map(|c| c.bytes).sum();
    let protected: u64 = cands.iter().filter(|c| c.contains_git).map(|c| c.bytes).sum();
    println!(
        "\n{} candidates, {} total ({:.1} GiB); {:.1} GiB protected by an inner .git",
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
        println!("{}", serde_json::to_string(&report).expect("status serializes"));
        return Ok(ExitCode::SUCCESS);
    }

    println!("disk: {} free of {} ({:.0}% used)", human(d.free), human(d.total), d.used() as f64 / d.total as f64 * 100.0);
    println!(
        "watermarks: low {} GiB (daemon trigger) · high target {} GiB",
        cfg.low_watermark_gib, cfg.until_free_gib
    );
    let verdict = if d.free < low { "CRITICAL — below the low watermark" } else if d.free < high { "TIGHT — a clean plan fits" } else { "OK — inside the band" };
    println!("state: {verdict}");

    let rej: u64 = rejected.iter().map(|r| r.bytes).sum();
    println!(
        "\nreclaimable now: {:.1} GiB · outside the plan (young / .git): {:.1} GiB",
        eligible as f64 / GIB as f64,
        rej as f64 / GIB as f64
    );
    println!("{} short of the {} GiB free target", human(report.need_bytes), cfg.until_free_gib);

    let mut by_age = items.clone();
    by_age.sort_by(|a, b| b.age_days.partial_cmp(&a.age_days).unwrap_or(std::cmp::Ordering::Equal));
    println!("\noldest first (eviction order):");
    println!("{:<10} {:>7}  PATH", "SIZE", "AGE(d)");
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
    let until = until_free.unwrap_or(cfg.until_free_gib);
    let cands = walk::scan(cfg, root, false);
    let (items, rejected) = plan::build(cands, cfg, older_than);
    // Re-stat after the scan: the plan uses free space now. The walk is long
    // and the disk moves during it — the same bug as the daemon cycle.
    let d = disk_of(cfg);
    let packed = plan::pack(items, d.free, until, top);

    println!("eviction plan (LRU: oldest first) — target: {until} GiB free");
    println!("{:<10} {:>7}  {:<24} PATH", "SIZE", "AGE(d)", "REGEN");
    for i in &packed.items {
        println!("{:<10} {:>7.0}  {:<24} {}", human(i.cand.bytes), i.age_days, i.hint, i.cand.path.display());
    }
    if packed.items.is_empty() {
        println!("(nothing eligible: all too young, protected by .git, or already at the target)");
    }
    println!(
        "\nitems: {} · would free: {} · short of target: {} · {}",
        packed.items.len(),
        human(packed.planned_bytes),
        human(packed.need_bytes),
        if packed.reached { "target reachable" } else { "target NOT reachable with these items/top" }
    );

    let mut rej_counts: std::collections::BTreeMap<String, (usize, u64)> = std::collections::BTreeMap::new();
    for r in &rejected {
        let key = r.reason.split(':').next().unwrap_or(&r.reason).trim().to_string();
        let e = rej_counts.entry(key).or_insert((0, 0));
        e.0 += 1;
        e.1 += r.bytes;
    }
    for (reason, (n, b)) in &rej_counts {
        println!("outside the plan: {reason} — {n} dirs, {}", human(*b));
    }

    if !apply {
        println!("\ndry-run (nothing removed). Run with --apply to execute.");
        return Ok(ExitCode::SUCCESS);
    }

    if packed.items.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }

    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err("stdin is not a terminal: confirm with --yes (or run without --apply for the dry-run)".into());
        }
        print!(
            "\nApply {} removals, freeing {}? [y/N] ",
            packed.items.len(),
            human(packed.planned_bytes)
        );
        let _ = std::io::stdout().flush();
        let mut ans = String::new();
        std::io::stdin().read_line(&mut ans).map_err(|e| e.to_string())?;
        if !ans.trim().eq_ignore_ascii_case("y") && !ans.trim().eq_ignore_ascii_case("yes") {
            println!("aborted — nothing removed.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    let out = clean::apply(&packed.items, &config::expand(&cfg.ledger));
    let after = disk_of(cfg);
    println!(
        "\nremoved: {} dirs · freed: {} · free now: {} (was {})",
        out.removed,
        human(out.freed),
        human(after.free),
        human(d.free)
    );
    for (p, why) in &out.skipped {
        println!("skipped: {} — {why}", p.display());
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
            format!("EVICT (needs {})", human(*need_bytes))
        }
    };
    println!(
        "cycle: {} candidates · eligible {} · free {} → {} · {}{}",
        rep.scanned,
        human(rep.eligible_bytes),
        human(rep.free_before),
        human(rep.free_after),
        action,
        if rep.rate_limited { " [rate-limited: holding this cycle]" } else { "" }
    );
    if rep.removed > 0 {
        println!(
            "  removed: {} · freed: {} · ledger: {}",
            rep.removed,
            human(rep.freed),
            config::expand(&cfg.ledger).display()
        );
        for (p, why) in &rep.skipped {
            println!("  skipped: {} — {why}", p.display());
        }
    }
}

fn cmd_tools(cfg: &config::Config, apply: bool, yes: bool) -> Result<ExitCode, String> {
    let d = disk_of(cfg);
    let bins = tools::Bins::default();
    let plans = tools::gather(cfg, &bins, d.free);

    println!("tool classes — target: {} GiB free (free now: {})", cfg.until_free_gib, human(d.free));
    let mut any = false;
    for p in &plans {
        match &p.outcome {
            tools::ToolPlanOutcome::Disabled => println!("{:<8} disabled in config", p.tool),
            tools::ToolPlanOutcome::AtTarget => println!("{:<8} at target — nothing to do", p.tool),
            tools::ToolPlanOutcome::Missing => println!("{:<8} SKIPPED: tool missing (fail-closed)", p.tool),
            tools::ToolPlanOutcome::Failed(e) => println!("{:<8} SKIPPED: {e}", p.tool),
            tools::ToolPlanOutcome::Plan(actions) => {
                any = true;
                if actions.is_empty() {
                    println!("{:<8} empty plan (nothing old or redundant)", p.tool);
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
            println!("\ndry-run (nothing removed). Run with --apply to execute.");
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
            return Err("stdin is not a terminal: confirm with --yes (or run without --apply for the dry-run)".into());
        }
        print!("\nApply {planned_total} tool actions? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut ans = String::new();
        std::io::stdin().read_line(&mut ans).map_err(|e| e.to_string())?;
        if !ans.trim().eq_ignore_ascii_case("y") && !ans.trim().eq_ignore_ascii_case("yes") {
            println!("aborted — nothing removed.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    let outs = tools::execute(&plans, &bins, &config::expand(&cfg.ledger));
    for o in &outs {
        match &o.status {
            tools::RunStatus::Missing => println!("skipped: {} — tool missing (fail-closed)", o.subject),
            tools::RunStatus::Failed(e) => println!("FAILED: {} — {e}", o.subject),
            tools::RunStatus::Ran { reclaimed_bytes } => {
                println!("ok: {} — confirmed {}", o.subject, human(*reclaimed_bytes))
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
        "{} files in {} · verdict {} · eligible {} · free {}",
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
    println!("plist written: {}", path.display());
    println!(
        "to load (your decision):\n  launchctl load {}",
        path.display()
    );
    println!("to unload:\n  launchctl unload {}", path.display());
    Ok(ExitCode::SUCCESS)
}
