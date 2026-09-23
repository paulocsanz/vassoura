use std::time::SystemTime;

pub const GIB: u64 = 1024 * 1024 * 1024;

pub fn human(bytes: u64) -> String {
    humansize::format_size(bytes, humansize::BINARY)
}

/// Age in days from `t` to `now`. A future mtime counts as 0
/// (conservative: "too new to evict").
pub fn age_days(now: SystemTime, t: SystemTime) -> f64 {
    now.duration_since(t).map(|d| d.as_secs_f64() / 86400.0).unwrap_or(0.0)
}

pub fn now_iso() -> String {
    jiff::Timestamp::now().to_string()
}
