use std::path::Path;

#[derive(Debug, Clone, Copy)]
pub struct Disk {
    pub total: u64,
    pub free: u64,
}

impl Disk {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
    pub fn used_percent(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.used() as f64 / self.total as f64) * 100.0
        }
    }
    pub fn snapshot(path: &Path) -> Option<Disk> {
        let free = fs2::free_space(path).ok()?;
        let total = fs2::total_space(path).ok()?;
        Some(Disk { total, free })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_percent_calculation() {
        let d = Disk { total: 1000, free: 200 };
        assert!((d.used_percent() - 80.0).abs() < 0.001);
        let empty = Disk { total: 0, free: 0 };
        assert_eq!(empty.used_percent(), 0.0);
    }
}
