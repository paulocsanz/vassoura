use std::time::SystemTime;

pub const GIB: u64 = 1024 * 1024 * 1024;

pub fn human(bytes: u64) -> String {
    humansize::format_size(bytes, humansize::BINARY)
}

/// Idade em dias desde `t` até `now`; mtimes no futuro contam como 0
/// (conservador: "novo demais para evictar").
pub fn age_days(now: SystemTime, t: SystemTime) -> f64 {
    now.duration_since(t).map(|d| d.as_secs_f64() / 86400.0).unwrap_or(0.0)
}

pub fn now_iso() -> String {
    jiff::Timestamp::now().to_string()
}
