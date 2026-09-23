use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// One line of the append-only ledger — the provenance of a cleanup:
/// what left, when, how much, at what age, and how to regenerate it.
#[derive(Debug, serde::Serialize)]
pub struct Line {
    pub ts: i64,
    pub iso: String,
    pub action: &'static str,
    pub class: String,
    pub path: String,
    pub bytes: u64,
    pub age_days: f64,
    pub hint: String,
}

pub fn append(ledger: &Path, line: &Line) -> std::io::Result<()> {
    if let Some(parent) = ledger.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(ledger)?;
    writeln!(f, "{}", serde_json::to_string(line).expect("ledger serializes"))
}
