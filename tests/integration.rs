//! E2E pela biblioteca: scan → plan → clean numa árvore falsa, com config real.

use std::path::Path;
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
