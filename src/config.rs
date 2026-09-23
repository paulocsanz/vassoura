use std::fs;
use std::path::{Path, PathBuf};

/// Nomes de diretório regeneráveis por padrão (config pode sobrescrever).
/// Deliberadamente conservador: `vendor` e `out` ficam de fora (podem conter
/// patches locais / dados); quem quiser adiciona no config.
pub const DEFAULT_ARTIFACT_NAMES: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".turbo",
    ".output",
    ".parcel-cache",
    ".venv",
    "venv",
    "virtualenv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".gradle",
    ".terraform",
    "Pods",
    ".dart_tool",
    ".stack-work",
    "cmake-build-debug",
    "cmake-build-release",
];

/// Diretórios nunca atravessados durante o scan (são dados, nunca lixo).
pub const PRUNE_DIRS: &[&str] = &[".git", ".fonte"];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub version: u32,
    /// Ledger append-only de toda evicção.
    pub ledger: PathBuf,
    /// Meta de espaço livre (marca d'água alta): a limpeza evicta até aqui.
    pub until_free_gib: f64,
    /// Marca d'água baixa: abaixo dela o disco está "lotando" (gatilho do daemon P1).
    pub low_watermark_gib: f64,
    /// Intervalo do loop do daemon (P1).
    pub watch_interval_secs: u64,
    /// Idade mínima (dias) para um artifact de build ser evictável.
    pub min_age_days_artifacts: u64,
    /// Idade mínima (dias) para um cache de app (~/Library/Caches etc.).
    pub min_age_days_app_caches: u64,
    /// Raízes onde caçar artifacts de build (allowlist — nada fora daqui é tocado).
    pub artifact_roots: Vec<PathBuf>,
    /// Raízes cujos filhos são caches de app (allowlist).
    pub app_cache_roots: Vec<PathBuf>,
    /// Nomes de diretório considerados artifacts.
    pub artifact_names: Vec<String>,
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into()))
}

pub fn expand(p: &Path) -> PathBuf {
    if let Some(s) = p.to_str() {
        if s == "~" {
            return home();
        }
        if let Some(rest) = s.strip_prefix("~/") {
            return home().join(rest);
        }
    }
    p.to_path_buf()
}

fn software_root() -> PathBuf {
    let sw = home().join("software");
    if sw.is_dir() {
        sw
    } else {
        home()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            ledger: home().join(".vassoura").join("ledger.jsonl"),
            until_free_gib: 100.0,
            low_watermark_gib: 40.0,
            watch_interval_secs: 300,
            min_age_days_artifacts: 14,
            min_age_days_app_caches: 30,
            artifact_roots: vec![software_root()],
            app_cache_roots: vec![home().join("Library").join("Caches"), home().join(".cache")],
            artifact_names: DEFAULT_ARTIFACT_NAMES.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl Config {
    pub fn min_age_days_for(&self, class: crate::walk::Class, override_min: Option<u64>) -> u64 {
        override_min.unwrap_or(match class {
            crate::walk::Class::Artifact => self.min_age_days_artifacts,
            crate::walk::Class::AppCache => self.min_age_days_app_caches,
        })
    }
}

fn serialize_pretty(cfg: &Config) -> String {
    format!(
        "# vassoura — coletor de build-lixo com marca d'água\n\
         # Tudo fora de artifact_roots/app_cache_roots é intocável por construção.\n\
         # Idade mínima protege o que está em uso; toda remoção vai para o ledger.\n\n{}",
        toml::to_string_pretty(cfg).expect("config serializa")
    )
}

/// Carrega o config. `--config` explícito inexistente é erro; o caminho
/// default ausente é criado com os padrões.
pub fn load(path: Option<&Path>) -> Result<(Config, PathBuf), String> {
    let default_path = home().join(".vassoura").join("config.toml");
    let (path, explicit) = match path {
        Some(p) => (expand(p), true),
        None => (default_path, false),
    };
    if !path.exists() {
        if explicit {
            return Err(format!("config não encontrado: {}", path.display()));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("criar {}: {e}", parent.display()))?;
        }
        let cfg = Config::default();
        fs::write(&path, serialize_pretty(&cfg))
            .map_err(|e| format!("escrever {}: {e}", path.display()))?;
        eprintln!("# config criado em {} (padrões; edite as raízes se quiser)", path.display());
        return Ok((cfg, path));
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("ler {}: {e}", path.display()))?;
    let cfg: Config =
        toml::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
    if cfg.version != 1 {
        return Err(format!("version {} não suportada (esperado 1)", cfg.version));
    }
    Ok((cfg, path))
}

/// Expande as raízes do config para caminhos absolutos canônicos-lexicais.
pub fn expanded_roots(cfg: &Config) -> Vec<(crate::walk::Class, PathBuf)> {
    let mut v: Vec<(crate::walk::Class, PathBuf)> = cfg
        .artifact_roots
        .iter()
        .map(|p| (crate::walk::Class::Artifact, expand(p)))
        .collect();
    v.extend(
        cfg.app_cache_roots
            .iter()
            .map(|p| (crate::walk::Class::AppCache, expand(p))),
    );
    v
}
