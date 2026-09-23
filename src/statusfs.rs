//! Status directory (P2.2): one plain file per metric (default `~/Vassoura`),
//! filled by `vassoura refresh` and by every daemon cycle, always with the
//! same values as `vassoura status --json`.

use std::fs;
use std::io;
use std::path::Path;

use crate::config::Config;
use crate::disk::Disk;
use crate::fmt_util::{now_iso, GIB};

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct StatusReport {
    pub updated_iso: String,
    pub disk: DiskReport,
    pub watermarks: Watermarks,
    /// "critical" | "tight" | "ok"
    pub verdict: String,
    pub eligible: Eligible,
    pub need_bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct DiskReport {
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub used_pct: f64,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct Watermarks {
    pub low_gib: f64,
    pub high_gib: f64,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct Eligible {
    pub bytes: u64,
    pub count: usize,
}

pub fn verdict_of(free: u64, low_bytes: u64, high_bytes: u64) -> &'static str {
    if free < low_bytes {
        "critical"
    } else if free < high_bytes {
        "tight"
    } else {
        "ok"
    }
}

pub fn build_report(disk: Disk, cfg: &Config, eligible_bytes: u64, eligible_count: usize) -> StatusReport {
    let low = (cfg.low_watermark_gib * GIB as f64) as u64;
    let high = (cfg.until_free_gib * GIB as f64) as u64;
    StatusReport {
        updated_iso: now_iso(),
        disk: DiskReport {
            free_bytes: disk.free,
            total_bytes: disk.total,
            used_pct: (disk.used() as f64 / disk.total as f64 * 10_000.0).round() / 100.0,
        },
        watermarks: Watermarks { low_gib: cfg.low_watermark_gib, high_gib: cfg.until_free_gib },
        verdict: verdict_of(disk.free, low, high).to_string(),
        eligible: Eligible { bytes: eligible_bytes, count: eligible_count },
        need_bytes: high.saturating_sub(disk.free),
    }
}

/// Write one file per metric; return the files written.
pub fn write_status_dir(dir: &Path, r: &StatusReport) -> io::Result<Vec<std::path::PathBuf>> {
    fs::create_dir_all(dir)?;
    let metrics: Vec<(&str, String)> = vec![
        ("updated_iso", r.updated_iso.clone()),
        ("disk_free_bytes", r.disk.free_bytes.to_string()),
        ("disk_total_bytes", r.disk.total_bytes.to_string()),
        ("disk_used_pct", format!("{:.2}", r.disk.used_pct)),
        ("watermark_low_gib", format!("{}", r.watermarks.low_gib)),
        ("watermark_high_gib", format!("{}", r.watermarks.high_gib)),
        ("verdict", r.verdict.clone()),
        ("eligible_bytes", r.eligible.bytes.to_string()),
        ("eligible_count", r.eligible.count.to_string()),
        ("need_bytes", r.need_bytes.to_string()),
    ];
    let mut written = Vec::new();
    for (name, value) in metrics {
        let p = dir.join(format!("{name}.txt"));
        fs::write(&p, format!("{value}\n"))?;
        written.push(p);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            low_watermark_gib: 40.0,
            until_free_gib: 100.0,
            ..Config::default()
        }
    }

    #[test]
    fn verdicts_match_marks() {
        let gib = GIB as u64;
        assert_eq!(verdict_of(30 * gib, 40 * gib, 100 * gib), "critical");
        assert_eq!(verdict_of(50 * gib, 40 * gib, 100 * gib), "tight");
        assert_eq!(verdict_of(120 * gib, 40 * gib, 100 * gib), "ok");
    }

    #[test]
    fn status_dir_files_match_report() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Vassoura");
        let d = Disk { total: 926 * GIB, free: 30 * GIB };
        let r = build_report(d, &cfg(), 110 * GIB, 42);
        let written = write_status_dir(&dir, &r).unwrap();
        assert_eq!(written.len(), 10);

        let read = |n: &str| fs::read_to_string(dir.join(format!("{n}.txt"))).unwrap().trim().to_string();
        assert_eq!(read("verdict"), r.verdict);
        assert_eq!(read("disk_free_bytes"), r.disk.free_bytes.to_string());
        assert_eq!(read("disk_used_pct"), format!("{:.2}", r.disk.used_pct));
        assert_eq!(read("eligible_bytes"), r.eligible.bytes.to_string());
        assert_eq!(read("need_bytes"), r.need_bytes.to_string());
        // and the JSON is the same contract
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["verdict"], "critical");
        assert_eq!(j["eligible"]["count"], 42);
    }
}
