//! E2E pela biblioteca: scan → plan → clean numa árvore falsa, com config real.

use std::path::{Path, PathBuf};
use std::time::Duration;
use vassoura::config::Config;
use vassoura::walk::Class;

fn age_dir(p: &Path, days: u64) {
    let t = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - Duration::from_secs(days * 86400),
    );
    filetime::set_file_times(p, t, t).unwrap();
}

fn mkfile(p: &Path, bytes: usize) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, vec![b'x'; bytes]).unwrap();
}

#[test]
fn scan_plan_clean_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projetos");
    let fake_home = tmp.path().to_path_buf();

    // projeto velho: node_modules de 40 dias, lockfile pnpm
    mkfile(&root.join("velho/package.json"), 20);
    mkfile(&root.join("velho/pnpm-lock.yaml"), 20);
    mkfile(&root.join("velho/node_modules/dep/a.js"), 1000);
    age_dir(&root.join("velho/node_modules/dep/a.js"), 40);
    age_dir(&root.join("velho/node_modules/dep"), 40);
    age_dir(&root.join("velho/node_modules"), 40);
    age_dir(&root.join("velho"), 40);

    // projeto novo: target de 2 dias (protegido pela idade)
    mkfile(&root.join("novo/Cargo.toml"), 10);
    mkfile(&root.join("novo/target/debug/app"), 2000);
    age_dir(&root.join("novo/target/debug/app"), 2);
    age_dir(&root.join("novo/target/debug"), 2);
    age_dir(&root.join("novo/target"), 2);
    age_dir(&root.join("novo"), 2);

    let cfg = Config {
        ledger: fake_home.join("ledger.jsonl"),
        artifact_roots: vec![root.clone()],
        app_cache_roots: vec![fake_home.join("Caches")],
        until_free_gib: 0.0, // meta 0 → need 0 → plan pack pega o que houver
        ..Config::default()
    };

    let cands = vassoura::walk::scan(&cfg, Some(&root), true);
    assert_eq!(cands.len(), 2, "node_modules velho + target novo: {cands:?}");

    let (items, rejected) = vassoura::plan::build(cands, &cfg, None);
    assert_eq!(items.len(), 1, "target novo é jovem: {items:?}");
    assert!(rejected.iter().any(|r| r.reason.contains("jovem")));
    assert_eq!(items[0].hint, "pnpm install");

    // mtime guarda: escrever dentro depois do scan torna o diretório "fresco"
    std::fs::write(root.join("velho/node_modules/dep/novo.js"), b"y").unwrap();
    let cands2 = vassoura::walk::scan(&cfg, Some(&root), true);
    let (items2, _) = vassoura::plan::build(cands2, &cfg, None);
    assert!(items2.is_empty(), "tocou dentro → jovem demais → fora do plano");

    // volta o mtime velho (arquivo E diretórios-pai que a escrita tocou)
    age_dir(&root.join("velho/node_modules/dep/novo.js"), 40);
    age_dir(&root.join("velho/node_modules/dep"), 40);
    age_dir(&root.join("velho/node_modules"), 40);
    let cands3 = vassoura::walk::scan(&cfg, Some(&root), true);
    let (items3, _) = vassoura::plan::build(cands3, &cfg, None);
    let out = vassoura::clean::apply(&items3, &cfg.ledger);
    assert_eq!(out.removed, 1);
    assert!(!root.join("velho/node_modules").exists());
    assert!(root.join("novo/target").exists(), "o jovem fica");
    let ledger = std::fs::read_to_string(&cfg.ledger).unwrap();
    assert_eq!(ledger.lines().count(), 1);
    assert!(ledger.contains("pnpm install"));
    assert!(ledger.contains("node_modules"));
}

#[test]
fn app_cache_children_are_cataloged() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_root = tmp.path().join("Caches");
    mkfile(&cache_root.join("app-velho/blob"), 500);
    age_dir(&cache_root.join("app-velho/blob"), 90);
    age_dir(&cache_root.join("app-velho"), 90);

    let cfg = Config {
        ledger: tmp.path().join("l.jsonl"),
        artifact_roots: vec![tmp.path().join("inexistente")],
        app_cache_roots: vec![cache_root.clone()],
        ..Config::default()
    };
    let cands = vassoura::walk::scan(&cfg, None, true);
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].class, Class::AppCache);

    let (items, rejected) = vassoura::plan::build(cands, &cfg, None);
    assert_eq!(items.len(), 1, "90d > 30d de min de app-cache: {rejected:?}");
}

// ---------------------------------------------------------------- P1.2
// Gates de uso ao vivo no caminho REAL de clean (produção: lsof + git).

fn git(tmp: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(tmp)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
}

fn sandbox_cfg(root: PathBuf, home: PathBuf) -> Config {
    Config {
        ledger: home.join("ledger.jsonl"),
        seen_db: home.join("seen.db"),
        status_dir: home.join("Vassoura"),
        artifact_roots: vec![root.clone()],
        app_cache_roots: vec![home.join("Caches")],
        ..Config::default()
    }
}

#[test]
fn clean_skips_dir_with_open_file_and_no_ledger_line() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let root = tmp.path().join("projetos");
    let nm = root.join("v/project/node_modules");
    mkfile(&nm.join("dep/a.js"), 800);
    for p in [&nm.join("dep/a.js"), &nm.join("dep"), &nm, &root.join("v/project"), &root.join("v"), &root] {
        age_dir(p, 40);
    }
    let cfg = sandbox_cfg(root.clone(), home.clone());
    let _ = std::fs::create_dir_all(&home);

    // processo REAL segurando arquivo aberto dentro do candidato
    let mut hold = std::process::Command::new("python3")
        .arg("-c")
        .arg(format!(
            "f=open({:?},'rb'); import time; time.sleep(20)",
            nm.join("dep/a.js").display().to_string()
        ))
        .spawn()
        .expect("python3");
    std::thread::sleep(Duration::from_millis(800));

    let cands = vassoura::walk::scan(&cfg, Some(&root), true);
    let (items, _) = vassoura::plan::build(cands, &cfg, None);
    assert_eq!(items.len(), 1);
    let out = vassoura::clean::apply(&items, &cfg.ledger); // gates de produção

    let _ = hold.kill();
    let _ = hold.wait();

    assert_eq!(out.removed, 0, "dir com arquivo aberto não sai");
    assert!(nm.exists(), "dir com arquivo aberto continua existindo");
    let skipped_reason = out.skipped.iter().map(|(_, w)| w.clone()).collect::<Vec<_>>().join("; ");
    assert!(skipped_reason.contains("em uso"), "{skipped_reason}");
    assert!(!cfg.ledger.exists() || std::fs::read_to_string(&cfg.ledger).unwrap().trim().is_empty(),
        "sem remoção → sem linha de ledger");
}

#[test]
fn clean_skips_dirty_worktree_but_clean_worktree_is_eligible() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let root = tmp.path().join("projetos");
    let project = root.join("repo");
    mkfile(&project.join("package.json"), 10);
    mkfile(&project.join("node_modules/dep/a.js"), 600);
    for p in [&project.join("node_modules/dep/a.js"), &project.join("node_modules/dep"), &project.join("node_modules"), &project, &root] {
        age_dir(p, 40);
    }
    let cfg = sandbox_cfg(root.clone(), home.clone());
    let _ = std::fs::create_dir_all(&home);

    git(&project, &["init", "-q"]);
    git(&project, &["config", "user.email", "t@t"]);
    git(&project, &["config", "user.name", "t"]);
    std::fs::write(project.join(".gitignore"), "node_modules\n").unwrap();
    git(&project, &["add", ".gitignore", "package.json"]);
    git(&project, &["commit", "-qm", "init"]);

    // trabalho não-commitado: pula
    std::fs::write(project.join("wip.txt"), b"trabalho").unwrap();
    let cands = vassoura::walk::scan(&cfg, Some(&root), true);
    let (items, _) = vassoura::plan::build(cands, &cfg, None);
    let out = vassoura::clean::apply(&items, &cfg.ledger);
    assert_eq!(out.removed, 0, "worktree suja é pulada");
    assert!(project.join("node_modules").exists());
    assert!(out.skipped.iter().any(|(_, w)| w.contains("não-commitado")), "{:?}", out.skipped);

    // commitou: elegível de novo
    git(&project, &["add", "wip.txt"]);
    git(&project, &["commit", "-qm", "wip"]);
    let cands = vassoura::walk::scan(&cfg, Some(&root), true);
    let (items, _) = vassoura::plan::build(cands, &cfg, None);
    let out = vassoura::clean::apply(&items, &cfg.ledger);
    assert_eq!(out.removed, 1, "worktree limpa segue elegível");
    assert!(!project.join("node_modules").exists());
    let ledger = std::fs::read_to_string(&cfg.ledger).unwrap();
    assert_eq!(ledger.lines().count(), 1);
}

// ---------------------------------------------------------------- P1.1
// Ciclo do daemon na árvore falsa: gatilho (marcas impossíveis) e idle.

fn daemon_tree(root: &Path) {
    for (name, days) in [("velho1", 40), ("velho2", 35), ("velho3", 30)] {
        let proj = root.join(name);
        mkfile(&proj.join("package.json"), 20);
        mkfile(&proj.join("pnpm-lock.yaml"), 20);
        mkfile(&proj.join("node_modules/dep/a.js"), 1000);
        let dir = proj.join("node_modules");
        age_dir(&dir.join("dep/a.js"), days);
        age_dir(&dir.join("dep"), days);
        age_dir(&dir, days);
        age_dir(&proj, days);
    }
    mkfile(&root.join("novo/Cargo.toml"), 10);
    mkfile(&root.join("novo/target/app"), 500);
    age_dir(&root.join("novo/target/app"), 2);
    age_dir(&root.join("novo/target"), 2);
    age_dir(&root.join("novo"), 2);
}

#[test]
fn daemon_cycle_triggers_bounded_and_ledgers_each_removal() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let root = tmp.path().join("projetos");
    daemon_tree(&root);

    let cfg = Config {
        low_watermark_gib: 1_000_000.0,   // baixa gigante: livre < baixa SEMPRE
        until_free_gib: 2_000_000.0,      // meta inalcançável: bound esgota o ciclo
        daemon: vassoura::config::DaemonCfg {
            max_items_per_cycle: 2,       // bound de itens
            notify: false,                // teste não dispara notificação real
            ..vassoura::config::DaemonCfg::default()
        },
        ..sandbox_cfg(root.clone(), home.clone())
    };

    let rep = vassoura::daemon::run_cycle(&cfg);

    assert_eq!(rep.removed, 2, "bound de 2 itens por ciclo: {:?}", rep.skipped);
    assert!(matches!(rep.action, Some(vassoura::daemon::CycleAction::Evict { .. })));
    assert!(!root.join("velho1/node_modules").exists(), "mais velho sai primeiro");
    assert!(!root.join("velho2/node_modules").exists());
    assert!(root.join("velho3/node_modules").exists(), "3º velho fica para o próximo ciclo");
    assert!(root.join("novo/target").exists(), "jovem é protegido pela idade");
    let ledger = std::fs::read_to_string(&cfg.ledger).unwrap();
    assert_eq!(ledger.lines().count(), 2, "uma linha por remoção");
    for l in ledger.lines() {
        let v: serde_json::Value = serde_json::from_str(l).unwrap();
        assert!(v["hint"].as_str().unwrap().contains("install"), "dica de regeneração: {}", v["hint"]);
    }

    // segundo ciclo imediato: rate-limit segura (default 900s)
    let cfg2 = Config { low_watermark_gib: 1_000_000.0, until_free_gib: 2_000_000.0, ..cfg.clone() };
    let rep2 = vassoura::daemon::run_cycle(&cfg2);
    assert!(rep2.rate_limited, "evicção há segundos → rate-limited");
    assert_eq!(rep2.removed, 0);
    let ledger2 = std::fs::read_to_string(&cfg2.ledger).unwrap();
    assert_eq!(ledger2.lines().count(), 2, "idle não acrescenta ledger");
}

#[test]
fn daemon_cycle_idle_between_marks_removes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let root = tmp.path().join("projetos");
    daemon_tree(&root);

    let cfg = Config {
        low_watermark_gib: 0.000001, // baixa ~0: livre ≥ baixa sempre → IDLE
        daemon: vassoura::config::DaemonCfg { notify: false, ..vassoura::config::DaemonCfg::default() },
        ..sandbox_cfg(root.clone(), home.clone())
    };
    let rep = vassoura::daemon::run_cycle(&cfg);

    assert_eq!(rep.action, Some(vassoura::daemon::CycleAction::Idle));
    assert_eq!(rep.removed, 0);
    assert!(!cfg.ledger.exists(), "idle: nada no ledger");
    // mesmo assim o statusfs e o seen.db são refreshados
    assert!(cfg.status_dir.join("verdict.txt").exists(), "ciclo alimenta o fs de status");
    assert!(cfg.seen_db.exists(), "ciclo persiste o seen.db");
    let seen = std::fs::read_to_string(&cfg.seen_db).unwrap();
    assert!(seen.contains("velho1/node_modules"), "seen.db registra candidatos: {seen}");
}

// ---------------------------------------------------------------- P2.1/P2.2
// seen.db alimenta o LRU; statusfs bate com status --json (mesma construção).

#[test]
fn seen_ordering_drives_daemon_lru() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let root = tmp.path().join("projetos");

    // dois candidatos com mtime equivalente (40d)
    for name in ["aa", "bb"] {
        let proj = root.join(name);
        mkfile(&proj.join("package.json"), 20);
        mkfile(&proj.join("node_modules/dep/a.js"), 1000);
        let dir = proj.join("node_modules");
        age_dir(&dir.join("dep/a.js"), 40);
        age_dir(&dir.join("dep"), 40);
        age_dir(&dir, 40);
        age_dir(&proj, 40);
    }
    let cfg = sandbox_cfg(root.clone(), home.clone());

    // ciclo 1 (idle): ambos nascem no seen com last_seen = mtime
    let cfg_cycle = Config {
        low_watermark_gib: 0.000001,
        daemon: vassoura::config::DaemonCfg { notify: false, ..vassoura::config::DaemonCfg::default() },
        ..cfg.clone()
    };
    let _ = vassoura::daemon::run_cycle(&cfg_cycle);

    // bb é REESCRITO depois do ciclo 1 e re-envelhecido a 40d: no próximo
    // refresh o mtime diverge do registrado → atividade → seen avança p/ agora
    std::fs::write(root.join("bb/node_modules/dep/b.js"), b"z").unwrap();
    let dir = root.join("bb/node_modules");
    age_dir(&dir.join("dep/b.js"), 40);
    age_dir(&dir.join("dep"), 40);
    age_dir(&dir, 40);

    let mut cands = vassoura::walk::scan(&cfg, Some(&root), true);
    let mut seen = vassoura::seen::SeenDb::load(&cfg.seen_db);
    seen.refresh(&cands, std::time::SystemTime::now()); // o que o ciclo 2 faria
    cands.sort_by_key(|c| seen.last_used(c));
    assert_eq!(cands[0].path, root.join("aa/node_modules"), "aa mais frio no seen sai primeiro");
    assert!(seen.last_used(&cands[1]) > seen.last_used(&cands[0]));
}

#[test]
fn status_json_contract_and_statusfs_match() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let root = tmp.path().join("projetos");
    daemon_tree(&root);
    let cfg = sandbox_cfg(root.clone(), home.clone());

    // o que o `status --json` imprime (mesma função):
    let d = vassoura::daemon::disk_of(&cfg);
    let cands = vassoura::walk::scan(&cfg, None, true);
    let (items, _) = vassoura::plan::build(cands, &cfg, None);
    let eligible: u64 = items.iter().map(|i| i.cand.bytes).sum();
    let report = vassoura::statusfs::build_report(d, &cfg, eligible, items.len());

    let json = serde_json::to_string(&report).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(v["disk"]["free_bytes"].is_u64());
    assert!(v["disk"]["total_bytes"].is_u64());
    assert!(v["watermarks"]["low_gib"].is_f64());
    assert!(v["watermarks"]["high_gib"].is_f64());
    assert_eq!(v["verdict"].as_str().unwrap(), report.verdict);
    assert_eq!(v["eligible"]["bytes"].as_u64().unwrap(), eligible as u64);
    assert_eq!(v["eligible"]["count"].as_u64().unwrap(), items.len() as u64);

    // o que o refresh/daemon escreve no fs de status: mesmos valores
    let written = vassoura::statusfs::write_status_dir(&cfg.status_dir, &report).unwrap();
    assert_eq!(written.len(), 10);
    let read = |n: &str| std::fs::read_to_string(cfg.status_dir.join(format!("{n}.txt"))).unwrap().trim().to_string();
    assert_eq!(read("disk_free_bytes"), v["disk"]["free_bytes"].to_string());
    assert_eq!(read("verdict"), v["verdict"].as_str().unwrap());
    assert_eq!(read("eligible_bytes"), v["eligible"]["bytes"].to_string());
    assert_eq!(read("need_bytes"), v["need_bytes"].to_string());
}

// ---------------------------------------------------------------- P1.3
// Execução das classes de ferramenta com binários FAKES reais (nada toca a
// carteira do operador): prova o caminho de execução + ledger.

fn make_exec(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm).unwrap();
}

#[test]
fn tools_execute_via_cli_and_ledger_each_removal() {
    let tmp = tempfile::tempdir().unwrap();
    let fakebin = tmp.path().join("bin");
    std::fs::create_dir_all(&fakebin).unwrap();
    let log = tmp.path().join("calls.log");
    let list = tmp.path().join("ollama-list.txt");
    std::fs::write(
        &list,
        "NAME       ID       SIZE      MODIFIED\n\
         old:1b     aaa      1.0 GB    2 months ago\n\
         new:1b     bbb      1.0 GB    3 days ago\n",
    )
    .unwrap();
    let ollama = fakebin.join("ollama");
    make_exec(
        &ollama,
        &format!(
            "#!/bin/sh\n[ \"$1\" = list ] && cat {} && exit 0\nprintf '%s\\n' \"$*\" >> {}\nexit 0\n",
            list.display(),
            log.display()
        ),
    );
    let docker = fakebin.join("docker");
    make_exec(
        &docker,
        &format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && echo ok && exit 0\nprintf '%s\\n' \"$*\" >> {}\nprintf 'Total reclaimed space: 1.5GB\\n'\nexit 0\n",
            log.display()
        ),
    );

    let cfg = Config {
        ledger: tmp.path().join("ledger.jsonl"),
        tools: vassoura::config::ToolsCfg {
            rustup: false,
            pnpm: false,
            go: false,
            ..vassoura::config::ToolsCfg::default()
        },
        // need gigante: o LRU do ollama leva todos os modelos
        until_free_gib: 1e9,
        ..Config::default()
    };
    let bins = vassoura::tools::Bins {
        ollama: ollama.into_os_string(),
        docker: docker.into_os_string(),
        ..vassoura::tools::Bins::default()
    };

    let free = 10 * 1024 * 1024 * 1024u64;
    let plans = vassoura::tools::gather(&cfg, &bins, free);
    let ollama_plan = plans.iter().find(|p| p.tool == "ollama").unwrap();
    let vassoura::tools::ToolPlanOutcome::Plan(actions) = &ollama_plan.outcome else {
        panic!("esperava plano do ollama")
    };
    assert_eq!(actions.len(), 2, "need gigante leva os 2 modelos (LRU)");
    assert_eq!(actions[0].subject, "old:1b", "mais velho primeiro");
    assert!(actions.iter().all(|a| a.argv[0] == "rm"));

    let outs = vassoura::tools::execute(&plans, &bins, &cfg.ledger);
    let ran = outs.iter().filter(|o| matches!(o.status, vassoura::tools::RunStatus::Ran { .. })).count();
    assert_eq!(ran, 3, "2 ollama rm + 1 docker prune: {outs:?}");

    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("rm old:1b"), "mais velho sai primeiro: {calls}");
    assert!(calls.contains("system prune --force"), "{calls}");
    assert!(!calls.contains("volume"), "prune nunca toca volumes: {calls}");

    let ledger = std::fs::read_to_string(&cfg.ledger).unwrap();
    let lines: Vec<serde_json::Value> =
        ledger.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 3, "uma linha por remoção/ação executada");
    let ollama_lines: Vec<&serde_json::Value> =
        lines.iter().filter(|v| v["class"] == "ollama").collect();
    assert_eq!(ollama_lines.len(), 2);
    for v in &ollama_lines {
        assert!(v["hint"].as_str().unwrap().starts_with("ollama pull"), "{}", v["hint"]);
    }
    let docker_line = lines.iter().find(|v| v["class"] == "docker").unwrap();
    assert_eq!(docker_line["bytes"].as_u64().unwrap(), (1.5 * 1024.0 * 1024.0 * 1024.0) as u64);
}

#[test]
fn tools_missing_binary_fail_closed_and_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        ledger: tmp.path().join("ledger.jsonl"),
        tools: vassoura::config::ToolsCfg {
            rustup: false,
            pnpm: false,
            go: false,
            ..vassoura::config::ToolsCfg::default()
        },
        until_free_gib: 1e9,
        ..Config::default()
    };
    let bins = vassoura::tools::Bins {
        ollama: tmp.path().join("nao-existe").into_os_string(),
        docker: tmp.path().join("nao-existe-2").into_os_string(),
        ..vassoura::tools::Bins::default()
    };
    let plans = vassoura::tools::gather(&cfg, &bins, 1024u64);
    for p in &plans {
        assert!(
            matches!(p.outcome, vassoura::tools::ToolPlanOutcome::Missing),
            "{} devia ser Missing (fail-closed)",
            p.tool
        );
    }
    let outs = vassoura::tools::execute(&plans, &bins, &cfg.ledger);
    assert!(outs.is_empty(), "Missing não executa nada");
    assert!(!cfg.ledger.exists());
}
