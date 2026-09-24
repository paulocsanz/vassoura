use std::fs;
use std::path::{Path, PathBuf};

/// Regenerable directory names by default (config may override).
/// Deliberately conservative: `vendor` and `out` stay out (they may contain
/// local patches / data); add them in the config if you want them.
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

/// Directories never walked during the scan (they are data, never junk).
pub const PRUNE_DIRS: &[&str] = &[".git", ".fonte"];

/// Bound and cadence of the daemon eviction cycle.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DaemonCfg {
    /// Maximum bytes removed per cycle (bound).
    #[serde(default = "default_max_bytes_per_cycle_gib")]
    pub max_bytes_per_cycle_gib: f64,
    /// Maximum items removed per cycle (bound).
    #[serde(default = "default_max_items_per_cycle")]
    pub max_items_per_cycle: usize,
    /// Minimum interval (seconds) between two eviction cycles (rate-limit).
    #[serde(default = "default_rate_limit_secs")]
    pub rate_limit_secs: u64,
    /// macOS notifications via osascript (best-effort, daemon only).
    #[serde(default = "default_notify")]
    pub notify: bool,
}

fn default_max_bytes_per_cycle_gib() -> f64 {
    // A cycle that fires has to be able to cross the low mark again.
    // 20 GiB was not enough: the scan takes ~1 h and the disk drops tens of
    // GiB in that time — the next cycle was born already underwater. 80 GiB
    // covers the typical distance between "critical" and the target of 100.
    80.0
}
fn default_max_items_per_cycle() -> usize {
    // The oldest are not always the largest; 30 small items do not
    // recover the disk. 200 is still a ceiling (it does not empty the working set).
    200
}
fn default_rate_limit_secs() -> u64 {
    60
}
fn default_notify() -> bool {
    true
}

impl Default for DaemonCfg {
    fn default() -> Self {
        Self {
            max_bytes_per_cycle_gib: default_max_bytes_per_cycle_gib(),
            max_items_per_cycle: default_max_items_per_cycle(),
            rate_limit_secs: default_rate_limit_secs(),
            notify: default_notify(),
        }
    }
}

/// Toggles for the tool classes (P1.3). A tool missing from PATH is
/// always fail-closed: the class is skipped and reported.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ToolsCfg {
    #[serde(default = "default_true")]
    pub ollama: bool,
    #[serde(default = "default_true")]
    pub docker: bool,
    #[serde(default = "default_true")]
    pub rustup: bool,
    /// How many of the newest versioned toolchains to keep.
    #[serde(default = "default_rustup_keep")]
    pub rustup_keep: usize,
    #[serde(default = "default_true")]
    pub pnpm: bool,
    #[serde(default = "default_true")]
    pub go: bool,
}

fn default_true() -> bool {
    true
}
fn default_rustup_keep() -> usize {
    3
}

impl Default for ToolsCfg {
    fn default() -> Self {
        Self {
            ollama: true,
            docker: true,
            rustup: true,
            rustup_keep: default_rustup_keep(),
            pnpm: true,
            go: true,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub version: u32,
    /// Append-only ledger of every eviction.
    pub ledger: PathBuf,
    /// Per-candidate "last seen" record (read-based LRU, P2.1).
    #[serde(default = "default_seen_db")]
    pub seen_db: PathBuf,
    /// Status directory (one file per metric, P2.2).
    #[serde(default = "default_status_dir")]
    pub status_dir: PathBuf,
    /// Free-space target (high watermark): cleanup evicts until here.
    pub until_free_gib: f64,
    /// Low watermark: below it the disk is "filling up" (daemon trigger, P1).
    pub low_watermark_gib: f64,
    /// Daemon loop interval (P1).
    pub watch_interval_secs: u64,
    /// Daemon bound and cadence (P1.1).
    #[serde(default)]
    pub daemon: DaemonCfg,
    /// Tool classes (P1.3).
    #[serde(default)]
    pub tools: ToolsCfg,
    /// Minimum age (days) before a build artifact is evictable.
    pub min_age_days_artifacts: u64,
    /// Minimum age (days) for an app cache (~/Library/Caches etc.).
    pub min_age_days_app_caches: u64,
    /// Minimum age (days) when disk is tight (< low_watermark). Default: 1.
    #[serde(default = "default_min_age_days_tight")]
    pub min_age_days_tight: u64,
    /// Roots where build artifacts are hunted (allowlist — nothing outside is touched).
    pub artifact_roots: Vec<PathBuf>,
    /// Roots whose children are app caches (allowlist).
    pub app_cache_roots: Vec<PathBuf>,
    /// Directory names treated as artifacts.
    pub artifact_names: Vec<String>,
}

fn default_min_age_days_tight() -> u64 {
    1
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into()))
}

fn default_seen_db() -> PathBuf {
    home().join(".vassoura").join("seen.db")
}

fn default_status_dir() -> PathBuf {
    home().join("Vassoura")
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
            seen_db: default_seen_db(),
            status_dir: default_status_dir(),
            until_free_gib: 100.0,
            low_watermark_gib: 40.0,
            watch_interval_secs: 300,
            min_age_days_artifacts: 1,
            min_age_days_app_caches: 14,
            min_age_days_tight: 1,
            daemon: DaemonCfg::default(),
            tools: ToolsCfg::default(),
            artifact_roots: vec![software_root()],
            app_cache_roots: vec![home().join("Library").join("Caches"), home().join(".cache")],
            artifact_names: DEFAULT_ARTIFACT_NAMES.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl Config {
    pub fn min_age_days_for(&self, class: crate::walk::Class, override_min: Option<u64>) -> u64 {
        match class {
            crate::walk::Class::Artifact => override_min.unwrap_or(self.min_age_days_artifacts),
            // App caches do not need to be 1 day even in tight mode; preserve min_age_days_app_caches
            // unless an explicit CLI override asks for an even higher threshold.
            crate::walk::Class::AppCache => self.min_age_days_app_caches.max(override_min.unwrap_or(0)),
        }
    }
}

fn serialize_pretty(cfg: &Config) -> String {
    format!(
        "# vassoura — watermarked build-artifact collector\n\
         # Anything outside artifact_roots/app_cache_roots is untouchable by construction.\n\
         # Minimum age protects what is in use; every removal goes to the ledger.\n\n{}",
        toml::to_string_pretty(cfg).expect("config serializes")
    )
}

/// Load the config. An explicit `--config` that does not exist is an error;
/// a missing default path is created with the defaults.
pub fn load(path: Option<&Path>) -> Result<(Config, PathBuf), String> {
    let default_path = home().join(".vassoura").join("config.toml");
    let (path, explicit) = match path {
        Some(p) => (expand(p), true),
        None => (default_path, false),
    };
    if !path.exists() {
        if explicit {
            return Err(format!("config not found: {}", path.display()));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let cfg = Config::default();
        fs::write(&path, serialize_pretty(&cfg))
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        eprintln!("# config created at {} (defaults; edit the roots if you want)", path.display());
        return Ok((cfg, path));
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let cfg: Config =
        toml::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
    if cfg.version != 1 {
        return Err(format!("version {} is not supported (expected 1)", cfg.version));
    }
    Ok((cfg, path))
}

/// Expand config roots to lexically canonical absolute paths.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A v1 config written before P1/P2 (without the new fields) stays
    /// valid: the new ones get defaults.
    #[test]
    fn v1_config_without_new_fields_still_parses() {
        let raw = r#"
version = 1
ledger = "/tmp/l.jsonl"
until_free_gib = 100.0
low_watermark_gib = 40.0
watch_interval_secs = 300
min_age_days_artifacts = 14
min_age_days_app_caches = 30
artifact_roots = ["/tmp/software"]
app_cache_roots = ["/tmp/Caches"]
artifact_names = ["node_modules", "target"]
"#;
        let cfg: Config = toml::from_str(raw).expect("v1 config parses");
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.daemon, DaemonCfg::default());
        assert_eq!(cfg.tools, ToolsCfg::default());
        assert_eq!(cfg.seen_db, default_seen_db());
        assert_eq!(cfg.status_dir, default_status_dir());
    }

    #[test]
    fn new_fields_round_trip() {
        let cfg = Config::default();
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.daemon, cfg.daemon);
        assert_eq!(back.tools, cfg.tools);
        assert_eq!(back.seen_db, cfg.seen_db);
    }

    /// PARTIAL [daemon]/[tools] tables (e.g. only `notify = false`) also parse:
    /// each field gets its own default.
    #[test]
    fn partial_daemon_and_tools_tables_parse() {
        let raw = r#"
version = 1
ledger = "/tmp/l.jsonl"
until_free_gib = 100.0
low_watermark_gib = 40.0
watch_interval_secs = 300
min_age_days_artifacts = 14
min_age_days_app_caches = 30
artifact_roots = ["/tmp/software"]
app_cache_roots = ["/tmp/Caches"]
artifact_names = ["node_modules"]

[daemon]
notify = false

[tools]
ollama = false
"#;
        let cfg: Config = toml::from_str(raw).expect("partial table parses");
        assert!(!cfg.daemon.notify);
        assert_eq!(cfg.daemon.max_items_per_cycle, 200);
        assert_eq!(cfg.daemon.max_bytes_per_cycle_gib, 80.0);
        assert_eq!(cfg.daemon.rate_limit_secs, 60);
        assert!(!cfg.tools.ollama);
        assert_eq!(cfg.tools.rustup_keep, 3);
    }
}
