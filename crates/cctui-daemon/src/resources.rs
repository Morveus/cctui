//! Host resource sampler feeding the heartbeat's `resources` block.
//!
//! CPU busy share since the previous sample (`/proc/stat`), memory
//! (`/proc/meminfo`) and the fill of the filesystem holding the home directory
//! (`statvfs`). Linux-only; on other platforms every sample is `None` and the
//! heartbeat simply omits the block, which the server reads as "unknown", not
//! "0 %".

use cctui_proto::resources::MachineResources;

/// Keeps the previous `/proc/stat` counters so each sample yields the busy
/// share over the heartbeat interval rather than since boot.
#[derive(Debug, Default)]
pub struct ResourceSampler {
    prev_cpu: Option<CpuTimes>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CpuTimes {
    idle: u64,
    total: u64,
}

impl ResourceSampler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One snapshot. The first call primes the CPU counters and reports no
    /// CPU figure yet (`None` overall), so the first heartbeat after start
    /// carries nothing rather than a bogus since-boot average.
    pub fn sample(&mut self) -> Option<MachineResources> {
        #[cfg(target_os = "linux")]
        {
            self.sample_linux()
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    #[cfg(target_os = "linux")]
    fn sample_linux(&mut self) -> Option<MachineResources> {
        let cur = std::fs::read_to_string("/proc/stat").ok().and_then(|s| parse_cpu_times(&s));
        let cpu_pct = match (self.prev_cpu, cur) {
            (Some(prev), Some(cur)) => cpu_busy_pct(prev, cur),
            _ => None,
        };
        if cur.is_some() {
            self.prev_cpu = cur;
        }
        let cpu_pct = cpu_pct?;
        let (mem_used_bytes, mem_total_bytes) =
            std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| parse_meminfo(&s))?;
        let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
        let (disk_used_bytes, disk_total_bytes) = disk_usage(&home)?;
        let load1 = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse::<f32>().ok()));
        Some(MachineResources {
            cpu_pct,
            mem_pct: pct(mem_used_bytes, mem_total_bytes),
            mem_used_bytes,
            mem_total_bytes,
            disk_pct: pct(disk_used_bytes, disk_total_bytes),
            disk_used_bytes,
            disk_total_bytes,
            disk_path: home.to_string_lossy().into_owned(),
            load1,
        })
    }
}

#[allow(clippy::cast_precision_loss)]
fn pct(used: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    ((used as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32
}

/// The aggregate `cpu` line of `/proc/stat`: idle = idle + iowait, total = sum
/// of every column (user nice system idle iowait irq softirq steal ...).
fn parse_cpu_times(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let cols: Vec<u64> = line.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
    if cols.len() < 4 {
        return None;
    }
    let idle = cols[3] + cols.get(4).copied().unwrap_or(0);
    let total = cols.iter().sum();
    Some(CpuTimes { idle, total })
}

#[allow(clippy::cast_precision_loss)]
fn cpu_busy_pct(prev: CpuTimes, cur: CpuTimes) -> Option<f32> {
    let total = cur.total.checked_sub(prev.total)?;
    let idle = cur.idle.checked_sub(prev.idle)?;
    if total == 0 {
        return None;
    }
    let busy = total.saturating_sub(idle);
    Some(((busy as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32)
}

/// `(used, total)` bytes from `/proc/meminfo`, used = `MemTotal` - `MemAvailable`
/// (what `free` calls "used" minus reclaimable cache).
fn parse_meminfo(info: &str) -> Option<(u64, u64)> {
    let (avail, total) = parse_mem_available(info)?;
    Some((total.saturating_sub(avail), total))
}

/// `(available, total)` bytes from `/proc/meminfo` (`MemAvailable`, `MemTotal`).
pub(crate) fn parse_mem_available(info: &str) -> Option<(u64, u64)> {
    let kb = |key: &str| -> Option<u64> {
        info.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
    };
    let total = kb("MemTotal:")? * 1024;
    let avail = kb("MemAvailable:")? * 1024;
    Some((avail, total))
}

/// `(used, total)` bytes of the filesystem holding `path`. Used counts the
/// root-reserved blocks as used, matching `df`'s "Use%" denominator choice
/// closely enough for a gauge.
#[cfg(target_os = "linux")]
fn disk_usage(path: &std::path::Path) -> Option<(u64, u64)> {
    let st = rustix::fs::statvfs(path).ok()?;
    let frag = st.f_frsize;
    let total = st.f_blocks.saturating_mul(frag);
    let free = st.f_bfree.saturating_mul(frag);
    Some((total.saturating_sub(free), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT_A: &str = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 1 2 3 4 5 6 7 8 9 10\n";
    const STAT_B: &str = "cpu  200 0 200 1000 100 0 0 0 0 0\n";

    #[test]
    fn cpu_busy_is_delta_over_interval() {
        let a = parse_cpu_times(STAT_A).unwrap();
        let b = parse_cpu_times(STAT_B).unwrap();
        // Δtotal = 1500-1000 = 500, Δidle (idle+iowait) = 1100-800 = 300 → 40 % busy.
        assert!((cpu_busy_pct(a, b).unwrap() - 40.0).abs() < f32::EPSILON);
        // No interval elapsed → nothing to report, never a division by zero.
        assert_eq!(cpu_busy_pct(a, a), None);
    }

    #[test]
    fn meminfo_used_excludes_reclaimable() {
        let info = "MemTotal:       1000 kB\nMemFree:         100 kB\nMemAvailable:    600 kB\n";
        assert_eq!(parse_meminfo(info), Some((400 * 1024, 1000 * 1024)));
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn first_sample_only_primes() {
        let mut s = ResourceSampler::new();
        let first = s.sample();
        if cfg!(target_os = "linux") {
            assert!(first.is_none(), "first sample has no CPU interval yet");
            std::thread::sleep(std::time::Duration::from_millis(20));
            if let Some(r) = s.sample() {
                assert!((0.0..=100.0).contains(&r.cpu_pct));
                assert!((0.0..=100.0).contains(&r.mem_pct));
                assert!((0.0..=100.0).contains(&r.disk_pct));
                assert!(r.mem_total_bytes > 0);
                assert!(!r.disk_path.is_empty());
            }
        } else {
            assert!(first.is_none());
        }
    }

    #[test]
    fn pct_is_clamped_and_zero_safe() {
        assert!((pct(0, 0) - 0.0).abs() < f32::EPSILON);
        assert!((pct(50, 100) - 50.0).abs() < f32::EPSILON);
        assert!((pct(200, 100) - 100.0).abs() < f32::EPSILON);
    }
}
