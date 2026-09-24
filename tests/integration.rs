//! E2E through the library: scan → plan → clean on a fake tree, with a real config.

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

    // old project: node_modules of 40 days, pnpm lockfile
    mkfile(&root.join("velho/package.json"), 20);
    mkfile(&root.join("velho/pnpm-lock.yaml"), 20);
    mkfile(&root.join("velho/node_modules/dep/a.js"), 1000);
    age_dir(&root.join("velho/node_modules/dep/a.js"), 40);
    age_dir(&root.join("velho/node_modules/dep"), 40);
    age_dir(&root.join("velho/node_modules"), 40);
    age_dir(&root.join("velho"), 40);

    // new project: target created today (protected by age < 1d)
    mkfile(&root.join("novo/Cargo.toml"), 10);
    mkfile(&root.join("novo/target/debug/app"), 2000);

    let cfg = Config {
        ledger: fake_home.join("ledger.jsonl"),
        artifact_roots: vec![root.clone()],
        app_cache_roots: vec![fake_home.join("Caches")],
        until_free_gib: 0.0, // target 0 → need 0 → plan pack takes whatever there is
        ..Config::default()
    };

    let cands = vassoura::walk::scan(&cfg, Some(&root), true);
    assert_eq!(cands.len(), 2, "old node_modules + new target: {cands:?}");

    let (items, rejected) = vassoura::plan::build(cands, &cfg, None);
    assert_eq!(items.len(), 1, "new target is young: {items:?}");
    assert!(rejected.iter().any(|r| r.reason.contains("young")));
    assert_eq!(items[0].hint, "pnpm install");

    // mtime guard: writing inside after the scan makes the directory "fresh"
    std::fs::write(root.join("velho/node_modules/dep/novo.js"), b"y").unwrap();
    let cands2 = vassoura::walk::scan(&cfg, Some(&root), true);
    let (items2, _) = vassoura::plan::build(cands2, &cfg, None);
    assert!(items2.is_empty(), "wrote inside → too young → out of the plan");

    // restore the old mtime (the file AND parent dirs the write touched)
    age_dir(&root.join("velho/node_modules/dep/novo.js"), 40);
    age_dir(&root.join("velho/node_modules/dep"), 40);
    age_dir(&root.join("velho/node_modules"), 40);
    let cands3 = vassoura::walk::scan(&cfg, Some(&root), true);
    let (items3, _) = vassoura::plan::build(cands3, &cfg, None);
    let out = vassoura::clean::apply(&items3, &cfg.ledger);
    assert_eq!(out.removed, 1);
    assert!(!root.join("velho/node_modules").exists());
    assert!(root.join("novo/target").exists(), "the young one stays");
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
    assert_eq!(items.len(), 1, "90d > 30d app-cache minimum: {rejected:?}");
}

// ---------------------------------------------------------------- P1.2
// Live use gates on the REAL clean path (production: lsof + git).

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

    // REAL process holding a file open inside the candidate
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
    let out = vassoura::clean::apply(&items, &cfg.ledger); // production gates

    let _ = hold.kill();
    let _ = hold.wait();

    assert_eq!(out.removed, 0, "dir with an open file does not leave");
    assert!(nm.exists(), "dir with an open file still exists");
    let skipped_reason = out.skipped.iter().map(|(_, w)| w.clone()).collect::<Vec<_>>().join("; ");
    assert!(skipped_reason.contains("in use"), "{skipped_reason}");
    assert!(!cfg.ledger.exists() || std::fs::read_to_string(&cfg.ledger).unwrap().trim().is_empty(),
        "no removal → no ledger line");
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

    // uncommitted work: skip
    std::fs::write(project.join("wip.txt"), b"trabalho").unwrap();
    let cands = vassoura::walk::scan(&cfg, Some(&root), true);
    let (items, _) = vassoura::plan::build(cands, &cfg, None);
    let out = vassoura::clean::apply(&items, &cfg.ledger);
    assert_eq!(out.removed, 0, "dirty worktree is skipped");
    assert!(project.join("node_modules").exists());
    assert!(out.skipped.iter().any(|(_, w)| w.contains("uncommitted")), "{:?}", out.skipped);

    // committed: eligible again
    git(&project, &["add", "wip.txt"]);
    git(&project, &["commit", "-qm", "wip"]);
    let cands = vassoura::walk::scan(&cfg, Some(&root), true);
    let (items, _) = vassoura::plan::build(cands, &cfg, None);
    let out = vassoura::clean::apply(&items, &cfg.ledger);
    assert_eq!(out.removed, 1, "clean worktree stays eligible");
    assert!(!project.join("node_modules").exists());
    let ledger = std::fs::read_to_string(&cfg.ledger).unwrap();
    assert_eq!(ledger.lines().count(), 1);
}

// ---------------------------------------------------------------- P1.1
// Daemon cycle on the fake tree: trigger (impossible marks) and idle.

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
        low_watermark_gib: 1_000_000.0,   // huge low mark: free < low ALWAYS
        until_free_gib: 2_000_000.0,      // unreachable target: the bound exhausts the cycle
        daemon: vassoura::config::DaemonCfg {
            max_items_per_cycle: 2,       // item bound
            notify: false,                // test does not fire a real notification
            ..vassoura::config::DaemonCfg::default()
        },
        ..sandbox_cfg(root.clone(), home.clone())
    };

    let rep = vassoura::daemon::run_cycle(&cfg);

    assert_eq!(rep.removed, 2, "bound of 2 items per cycle: {:?}", rep.skipped);
    assert!(matches!(rep.action, Some(vassoura::daemon::CycleAction::Evict { .. })));
    assert!(!root.join("velho1/node_modules").exists(), "oldest leaves first");
    assert!(!root.join("velho2/node_modules").exists());
    assert!(root.join("velho3/node_modules").exists(), "3rd oldest stays for the next cycle");
    assert!(root.join("novo/target").exists(), "young one is protected by age");
    let ledger = std::fs::read_to_string(&cfg.ledger).unwrap();
    assert_eq!(ledger.lines().count(), 2, "one line per removal");
    for l in ledger.lines() {
        let v: serde_json::Value = serde_json::from_str(l).unwrap();
        assert!(v["hint"].as_str().unwrap().contains("install"), "regeneration hint: {}", v["hint"]);
    }

    // immediate second cycle: rate-limit holds (default 900s)
    let cfg2 = Config { low_watermark_gib: 1_000_000.0, until_free_gib: 2_000_000.0, ..cfg.clone() };
    let rep2 = vassoura::daemon::run_cycle(&cfg2);
    assert!(rep2.rate_limited, "eviction seconds ago → rate-limited");
    assert_eq!(rep2.removed, 0);
    let ledger2 = std::fs::read_to_string(&cfg2.ledger).unwrap();
    assert_eq!(ledger2.lines().count(), 2, "idle does not append to the ledger");
}

#[test]
fn daemon_decides_on_free_space_measured_after_the_scan() {
    // The scan takes minutes on the real working set and free space drops in
    // the middle. The stat at the START (here 50 GiB, above the mark of 40)
    // must not decide: the stat at the END (10 GiB) is the one that counts,
    // and the cycle has to evict.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let root = tmp.path().join("projetos");
    daemon_tree(&root);

    let cfg = Config {
        low_watermark_gib: 40.0,
        until_free_gib: 100.0,
        daemon: vassoura::config::DaemonCfg {
            max_items_per_cycle: 30,
            max_bytes_per_cycle_gib: 20.0,
            notify: false,
            ..vassoura::config::DaemonCfg::default()
        },
        ..sandbox_cfg(root.clone(), home.clone())
    };

    let calls = std::cell::Cell::new(0u32);
    let gib = 1024u64 * 1024 * 1024;
    let rep = vassoura::daemon::run_cycle_sampling(&cfg, || {
        let n = calls.get();
        calls.set(n + 1);
        // 1st sample = start of the cycle (comfortable); the rest = after the scan
        let free = if n == 0 { 50 * gib } else { 10 * gib };
        vassoura::disk::Disk { total: 200 * gib, free }
    });

    assert!(
        matches!(rep.action, Some(vassoura::daemon::CycleAction::Evict { .. })),
        "free space fell below the mark during the scan: must evict, not idle: {:?}",
        rep.action
    );
    assert!(rep.removed >= 1, "old candidate leaves when free space at the end of the scan is critical");
    assert!(!root.join("velho1/node_modules").exists());
    assert!(cfg.ledger.exists(), "removal writes the ledger");
}

#[test]
fn daemon_cycle_idle_between_marks_removes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let root = tmp.path().join("projetos");
    daemon_tree(&root);

    let cfg = Config {
        low_watermark_gib: 0.000001, // low ~0: free ≥ low always → IDLE
        daemon: vassoura::config::DaemonCfg { notify: false, ..vassoura::config::DaemonCfg::default() },
        ..sandbox_cfg(root.clone(), home.clone())
    };
    let rep = vassoura::daemon::run_cycle(&cfg);

    assert_eq!(rep.action, Some(vassoura::daemon::CycleAction::Idle));
    assert_eq!(rep.removed, 0);
    assert!(!cfg.ledger.exists(), "idle: nothing in the ledger");
    // even so, statusfs and seen.db are refreshed
    assert!(cfg.status_dir.join("verdict.txt").exists(), "cycle feeds the status fs");
    assert!(cfg.seen_db.exists(), "cycle persists seen.db");
    let seen = std::fs::read_to_string(&cfg.seen_db).unwrap();
    assert!(seen.contains("velho1/node_modules"), "seen.db records candidates: {seen}");
}

#[test]
fn daemon_tight_mode_evicts_younger_candidates_when_critical() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let root = tmp.path().join("projetos");
    daemon_tree(&root);

    let cfg = Config {
        low_watermark_gib: 1_000_000.0, // critical
        until_free_gib: 2_000_000.0,
        min_age_days_artifacts: 3,
        min_age_days_tight: 1,
        daemon: vassoura::config::DaemonCfg {
            max_items_per_cycle: 10,
            notify: false,
            ..vassoura::config::DaemonCfg::default()
        },
        ..sandbox_cfg(root.clone(), home.clone())
    };

    let rep = vassoura::daemon::run_cycle(&cfg);
    // In tight mode, novo/target (2 days old) is >= 1d, so it's evicted alongside the 3 old ones
    assert_eq!(rep.removed, 4, "all 4 candidates evicted including 2-day-old target under tight mode");
    assert!(!root.join("novo/target").exists());
}

// ---------------------------------------------------------------- P2.1/P2.2
// seen.db feeds the LRU; statusfs matches status --json (same construction).

#[test]
fn seen_ordering_drives_daemon_lru() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let root = tmp.path().join("projetos");

    // two candidates with equivalent mtime (40d)
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

    // cycle 1 (idle): both are born in seen with last_seen = mtime
    let cfg_cycle = Config {
        low_watermark_gib: 0.000001,
        daemon: vassoura::config::DaemonCfg { notify: false, ..vassoura::config::DaemonCfg::default() },
        ..cfg.clone()
    };
    let _ = vassoura::daemon::run_cycle(&cfg_cycle);

    // bb is REWRITTEN after cycle 1 and re-aged to 40d: on the next
    // refresh the mtime diverges from the recorded one → activity → seen advances to now
    std::fs::write(root.join("bb/node_modules/dep/b.js"), b"z").unwrap();
    let dir = root.join("bb/node_modules");
    age_dir(&dir.join("dep/b.js"), 40);
    age_dir(&dir.join("dep"), 40);
    age_dir(&dir, 40);

    let mut cands = vassoura::walk::scan(&cfg, Some(&root), true);
    let mut seen = vassoura::seen::SeenDb::load(&cfg.seen_db);
    seen.refresh(&cands, std::time::SystemTime::now()); // what cycle 2 would do
    cands.sort_by_key(|c| seen.last_used(c));
    assert_eq!(cands[0].path, root.join("aa/node_modules"), "aa, colder in seen, leaves first");
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

    // what `status --json` prints (same function):
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

    // what refresh/daemon writes to the status fs: the same values
    let written = vassoura::statusfs::write_status_dir(&cfg.status_dir, &report).unwrap();
    assert_eq!(written.len(), 10);
    let read = |n: &str| std::fs::read_to_string(cfg.status_dir.join(format!("{n}.txt"))).unwrap().trim().to_string();
    assert_eq!(read("disk_free_bytes"), v["disk"]["free_bytes"].to_string());
    assert_eq!(read("verdict"), v["verdict"].as_str().unwrap());
    assert_eq!(read("eligible_bytes"), v["eligible"]["bytes"].to_string());
    assert_eq!(read("need_bytes"), v["need_bytes"].to_string());
}

// ---------------------------------------------------------------- P1.3
// Execution of the tool classes with real FAKE binaries (nothing touches the
// operator's working set): proves the execution path + ledger.

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
        // huge need: ollama LRU takes every model
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
        panic!("expected an ollama plan")
    };
    assert_eq!(actions.len(), 2, "huge need takes both models (LRU)");
    assert_eq!(actions[0].subject, "old:1b", "oldest first");
    assert!(actions.iter().all(|a| a.argv[0] == "rm"));

    let outs = vassoura::tools::execute(&plans, &bins, &cfg.ledger);
    let ran = outs.iter().filter(|o| matches!(o.status, vassoura::tools::RunStatus::Ran { .. })).count();
    assert_eq!(ran, 3, "2 ollama rm + 1 docker prune: {outs:?}");

    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("rm old:1b"), "oldest leaves first: {calls}");
    assert!(calls.contains("system prune --force"), "{calls}");
    assert!(!calls.contains("volume"), "prune never touches volumes: {calls}");

    let ledger = std::fs::read_to_string(&cfg.ledger).unwrap();
    let lines: Vec<serde_json::Value> =
        ledger.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 3, "one line per removal/action that ran");
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
            "{} should be Missing (fail-closed)",
            p.tool
        );
    }
    let outs = vassoura::tools::execute(&plans, &bins, &cfg.ledger);
    assert!(outs.is_empty(), "Missing runs nothing");
    assert!(!cfg.ledger.exists());
}
