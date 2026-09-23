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
    pub fn snapshot(path: &Path) -> Option<Disk> {
        let free = fs2::free_space(path).ok()?;
        let total = fs2::total_space(path).ok()?;
        Some(Disk { total, free })
    }
}
