//! Pure-Rust parsers for /proc/{stat,meminfo,loadavg} + the CPU-delta
//! calculator they feed. Linux-only at the IO boundary; the parsers
//! themselves are pure `(&str) -> Option<T>` and unit-test without /proc.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CpuSnapshot {
    pub total: u64,
    pub idle: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MemInfo {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

impl MemInfo {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }
    pub fn used_pct(&self) -> Option<f64> {
        if self.total_bytes == 0 {
            return None;
        }
        Some((self.used_bytes() as f64 / self.total_bytes as f64) * 100.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoadAvg {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiskInfo {
    pub total_bytes: u64,
    /// Blocks available to an unprivileged process (`f_bavail` × `f_frsize`).
    /// We use `f_bavail` rather than `f_bfree` because `df` does the same:
    /// reserved blocks aren't "free" from the operator's perspective.
    pub available_bytes: u64,
}

impl DiskInfo {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }
    pub fn used_pct(&self) -> Option<f64> {
        if self.total_bytes == 0 {
            return None;
        }
        Some((self.used_bytes() as f64 / self.total_bytes as f64) * 100.0)
    }
}

/// CPU% averaged across the interval between two snapshots (0..=100).
/// `None` when the counter didn't advance, or appeared to go backwards.
pub fn compute_cpu_pct(prev: &CpuSnapshot, curr: &CpuSnapshot) -> Option<f64> {
    let total_d = curr.total.checked_sub(prev.total)?;
    let idle_d = curr.idle.checked_sub(prev.idle)?;
    if total_d == 0 {
        return None;
    }
    let active = total_d.saturating_sub(idle_d);
    Some((active as f64 / total_d as f64) * 100.0)
}

/// Parse the aggregate `cpu ` line of /proc/stat.
/// Columns: `cpu user nice system idle iowait irq softirq steal guest guest_nice`.
/// Idle bucket = idle + iowait (Linux convention). Total = sum of all columns.
pub fn parse_proc_stat(s: &str) -> Option<CpuSnapshot> {
    let line = s.lines().find(|l| l.starts_with("cpu "))?;
    let mut iter = line.split_whitespace();
    let _ = iter.next()?;
    let cols: Vec<u64> = iter.filter_map(|t| t.parse().ok()).collect();
    if cols.len() < 4 {
        return None;
    }
    let idle = cols[3] + cols.get(4).copied().unwrap_or(0);
    let total: u64 = cols.iter().sum();
    Some(CpuSnapshot { total, idle })
}

/// Parse MemTotal + MemAvailable (both reported in kB) out of /proc/meminfo.
pub fn parse_meminfo(s: &str) -> Option<MemInfo> {
    let mut total: Option<u64> = None;
    let mut avail: Option<u64> = None;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = rest.split_whitespace().next().and_then(|n| n.parse().ok());
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail = rest.split_whitespace().next().and_then(|n| n.parse().ok());
        }
        if total.is_some() && avail.is_some() {
            break;
        }
    }
    Some(MemInfo {
        total_bytes: total? * 1024,
        available_bytes: avail? * 1024,
    })
}

/// Parse the three load-average fields from /proc/loadavg.
/// Format: `0.42 0.51 0.48 3/456 12345`.
pub fn parse_loadavg(s: &str) -> Option<LoadAvg> {
    let mut iter = s.split_whitespace();
    let load1: f64 = iter.next()?.parse().ok()?;
    let load5: f64 = iter.next()?.parse().ok()?;
    let load15: f64 = iter.next()?.parse().ok()?;
    Some(LoadAvg {
        load1,
        load5,
        load15,
    })
}

#[cfg(target_os = "linux")]
pub fn read_cpu_snapshot() -> Option<CpuSnapshot> {
    parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
}

#[cfg(target_os = "linux")]
pub fn read_mem_info() -> Option<MemInfo> {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(target_os = "linux")]
pub fn read_loadavg() -> Option<LoadAvg> {
    parse_loadavg(&std::fs::read_to_string("/proc/loadavg").ok()?)
}

/// One `statvfs(2)` syscall against a mount point. `None` on syscall
/// failure (path doesn't exist, isn't mounted, EACCES, ...).
#[cfg(target_os = "linux")]
pub fn read_disk_info(path: &str) -> Option<DiskInfo> {
    let c_path = std::ffi::CString::new(path).ok()?;
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return None;
    }
    let block_size = buf.f_frsize as u64;
    Some(DiskInfo {
        total_bytes: (buf.f_blocks as u64) * block_size,
        available_bytes: (buf.f_bavail as u64) * block_size,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn read_cpu_snapshot() -> Option<CpuSnapshot> {
    None
}
#[cfg(not(target_os = "linux"))]
pub fn read_mem_info() -> Option<MemInfo> {
    None
}
#[cfg(not(target_os = "linux"))]
pub fn read_loadavg() -> Option<LoadAvg> {
    None
}
#[cfg(not(target_os = "linux"))]
pub fn read_disk_info(_path: &str) -> Option<DiskInfo> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_stat_aggregate_line() {
        let s = "cpu  100 10 20 1000 5 0 1 0 0 0\ncpu0 50 5 10 500 2 0 0 0 0 0\n";
        let snap = parse_proc_stat(s).unwrap();
        assert_eq!(snap.total, 100 + 10 + 20 + 1000 + 5 + 0 + 1 + 0 + 0 + 0);
        assert_eq!(snap.idle, 1000 + 5);
    }

    #[test]
    fn parses_proc_stat_minimal_four_columns() {
        let s = "cpu  100 10 20 1000\n";
        let snap = parse_proc_stat(s).unwrap();
        assert_eq!(snap.total, 1130);
        assert_eq!(snap.idle, 1000);
    }

    #[test]
    fn proc_stat_skips_per_cpu_lines() {
        // Make sure we lock onto "cpu " (with space), not "cpu0", "cpu1", ...
        let s = "cpu0 999 999 999 999\ncpu  1 2 3 4 5\n";
        let snap = parse_proc_stat(s).unwrap();
        assert_eq!(snap.total, 1 + 2 + 3 + 4 + 5);
        assert_eq!(snap.idle, 4 + 5);
    }

    #[test]
    fn proc_stat_rejects_malformed() {
        assert!(parse_proc_stat("garbage").is_none());
        assert!(parse_proc_stat("cpu  only three\n").is_none());
        assert!(parse_proc_stat("").is_none());
    }

    #[test]
    fn parses_meminfo_total_and_available() {
        let s = "MemTotal:       16345608 kB\nMemFree:         1234567 kB\n\
                 MemAvailable:   12345678 kB\nBuffers:           12345 kB\n";
        let m = parse_meminfo(s).unwrap();
        assert_eq!(m.total_bytes, 16345608u64 * 1024);
        assert_eq!(m.available_bytes, 12345678u64 * 1024);
        assert_eq!(m.used_bytes(), (16345608 - 12345678) * 1024);
        let pct = m.used_pct().unwrap();
        assert!((pct - 24.47).abs() < 0.1, "got {pct}");
    }

    #[test]
    fn meminfo_rejects_when_required_field_absent() {
        assert!(parse_meminfo("MemFree: 100 kB\n").is_none());
        assert!(parse_meminfo("MemTotal: 100 kB\n").is_none());
        assert!(parse_meminfo("").is_none());
    }

    #[test]
    fn parses_loadavg_fields() {
        let l = parse_loadavg("0.42 0.51 0.48 3/456 12345\n").unwrap();
        assert!((l.load1 - 0.42).abs() < 1e-9);
        assert!((l.load5 - 0.51).abs() < 1e-9);
        assert!((l.load15 - 0.48).abs() < 1e-9);
    }

    #[test]
    fn loadavg_rejects_malformed() {
        assert!(parse_loadavg("garbage").is_none());
        assert!(parse_loadavg("0.42").is_none());
        assert!(parse_loadavg("").is_none());
    }

    #[test]
    fn compute_cpu_pct_basic() {
        let prev = CpuSnapshot {
            total: 1000,
            idle: 800,
        };
        let curr = CpuSnapshot {
            total: 2000,
            idle: 1500,
        };
        // total_d=1000, idle_d=700, active=300, pct=30.0
        let pct = compute_cpu_pct(&prev, &curr).unwrap();
        assert!((pct - 30.0).abs() < 1e-9);
    }

    #[test]
    fn compute_cpu_pct_fully_idle() {
        let prev = CpuSnapshot {
            total: 1000,
            idle: 800,
        };
        let curr = CpuSnapshot {
            total: 2000,
            idle: 1800,
        };
        let pct = compute_cpu_pct(&prev, &curr).unwrap();
        assert!(pct.abs() < 1e-9, "expected 0%, got {pct}");
    }

    #[test]
    fn compute_cpu_pct_zero_delta_is_none() {
        let s = CpuSnapshot {
            total: 1000,
            idle: 800,
        };
        assert!(compute_cpu_pct(&s, &s).is_none());
    }

    #[test]
    fn compute_cpu_pct_counter_wrap_is_none() {
        // Counters are monotonic on Linux; treat curr<prev as no-signal.
        let prev = CpuSnapshot {
            total: 2000,
            idle: 1500,
        };
        let curr = CpuSnapshot {
            total: 1000,
            idle: 800,
        };
        assert!(compute_cpu_pct(&prev, &curr).is_none());
    }

    #[test]
    fn disk_used_pct_basic() {
        let d = DiskInfo {
            total_bytes: 64 * 1024 * 1024 * 1024,    // 64 GB
            available_bytes: 48 * 1024 * 1024 * 1024, // 48 GB free
        };
        assert_eq!(d.used_bytes(), 16 * 1024 * 1024 * 1024);
        let pct = d.used_pct().unwrap();
        assert!((pct - 25.0).abs() < 1e-9, "got {pct}");
    }

    #[test]
    fn disk_used_pct_zero_total_is_none() {
        let d = DiskInfo {
            total_bytes: 0,
            available_bytes: 0,
        };
        assert!(d.used_pct().is_none());
    }

    #[test]
    fn disk_used_pct_full() {
        let d = DiskInfo {
            total_bytes: 1000,
            available_bytes: 0,
        };
        let pct = d.used_pct().unwrap();
        assert!((pct - 100.0).abs() < 1e-9);
    }
}
