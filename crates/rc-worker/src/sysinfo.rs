//! Host statistics for the heartbeat. These feed the scheduler's disk and CPU
//! terms (§6.1), so they must be cheap enough to sample every couple of
//! seconds.

use std::path::Path;

/// Normalised load: 1-minute load average divided by core count, clamped to
/// [0, 1]. The scheduler wants "how busy", not a raw number whose meaning
/// depends on the machine.
pub fn cpu_load() -> f64 {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(1.0);
    match raw_loadavg() {
        Some(load) => (load / cores).clamp(0.0, 1.0),
        None => 0.0,
    }
}

fn raw_loadavg() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/loadavg").ok()?;
        return text.split_whitespace().next()?.parse().ok();
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("uptime").output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        parse_uptime_load(&text)
    }
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_uptime_load(text: &str) -> Option<f64> {
    let tail = text.split("load average").nth(1)?;
    // BSD separates the three values with spaces, GNU with commas.
    tail.trim_start_matches(|c: char| c == ':' || c == 's' || c.is_whitespace())
        .split(|c: char| c == ',' || c.is_whitespace())
        .find(|s| !s.is_empty())?
        .parse()
        .ok()
}

/// Space on the filesystem holding a path.
///
/// The total matters as much as the free space: "20 GB left" means something
/// different on a 100 GB machine and on a 2 TB one, and the scheduler can only
/// express a reserve as a fraction of the disk if it is told how big it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskUsage {
    /// Available to unprivileged writers. Zero means unknown/unreadable.
    pub free_gb: u64,
    /// Filesystem size. Zero when `df` did not give one, which is what an
    /// older worker reports and what the scheduler must treat as "no total".
    pub total_gb: u64,
}

/// Free space in GB on the filesystem holding `path`.
pub fn disk_free_gb(path: &Path) -> u64 {
    disk_usage(path).free_gb
}

pub fn disk_usage(path: &Path) -> DiskUsage {
    let Ok(out) = std::process::Command::new("df")
        .arg("-Pk")
        .arg(path)
        .output()
    else {
        return DiskUsage::default();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some((total_kb, available_kb)) = parse_df_kb(&text) else {
        return DiskUsage::default();
    };
    DiskUsage {
        free_gb: available_kb / (1024 * 1024),
        total_gb: total_kb / (1024 * 1024),
    }
}

/// `df -Pk` guarantees POSIX output: a header line, then
/// `Filesystem 1024-blocks Used Available Capacity Mounted-on`.
///
/// Returns `(total, available)` in 1024-byte blocks.
fn parse_df_kb(output: &str) -> Option<(u64, u64)> {
    let line = output.lines().nth(1)?;
    let mut fields: Vec<&str> = line.split_whitespace().collect();
    // A long device name can wrap onto the next line; POSIX mode prevents
    // that, but stay defensive and index from the end instead.
    if fields.len() < 6 {
        fields = output.lines().skip(1).flat_map(|l| l.split_whitespace()).collect();
    }
    let n = fields.len();
    let total = fields.get(n.checked_sub(5)?)?.parse().ok()?;
    let available = fields.get(n.checked_sub(3)?)?.parse().ok()?;
    Some((total, available))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_posix_df_output() {
        let out = "Filesystem 1024-blocks      Used Available Capacity Mounted on\n\
                   /dev/nvme0n1p2 982940788 123456789 809483999      14% /\n";
        assert_eq!(parse_df_kb(out), Some((982_940_788, 809_483_999)));
    }

    #[test]
    fn parses_df_with_a_wrapped_device_name() {
        let out = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                   /dev/mapper/a-very-long-logical-volume-name\n\
                    982940788 123456789 809483999 14% /\n";
        assert_eq!(parse_df_kb(out), Some((982_940_788, 809_483_999)));
    }

    #[test]
    fn the_total_is_read_from_the_same_line_as_the_free_space() {
        // A 2 TB volume with 900 GB left: the two numbers the scheduler needs
        // to turn "GB left" into a fraction of the disk.
        let out = "Filesystem 1024-blocks      Used Available Capacity Mounted on\n\
                   /dev/nvme1n1p1 1953514584 1000000000 943514584      52% /data\n";
        let usage = parse_df_kb(out).expect("parsed");
        assert_eq!(usage, (1_953_514_584, 943_514_584));
    }

    #[test]
    fn malformed_df_output_does_not_panic() {
        assert_eq!(parse_df_kb(""), None);
        assert_eq!(parse_df_kb("Filesystem\n"), None);
        // A line with no numbers at all must not be read as a zero-sized disk.
        assert_eq!(parse_df_kb("Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                                /dev/disk3s1 x y z 10% /\n"), None);
    }

    #[test]
    fn load_is_normalised_into_a_unit_range() {
        let load = cpu_load();
        assert!((0.0..=1.0).contains(&load), "got {load}");
    }

    #[test]
    fn disk_free_reports_something_for_a_real_path() {
        // The exact number is environment-specific; zero would mean the
        // scheduler always treats this worker as full.
        let usage = disk_usage(&std::env::temp_dir());
        assert!(usage.free_gb > 0);
        // The total may legitimately be 0 on an exotic `df`, but when it is
        // there it must bound the free space.
        if usage.total_gb > 0 {
            assert!(usage.free_gb <= usage.total_gb, "{usage:?}");
        }
    }

    #[test]
    fn uptime_load_parsing_handles_both_bsd_and_gnu_formats() {
        assert_eq!(
            parse_uptime_load("12:00  up 3 days, load averages: 1.50 2.00 1.75"),
            Some(1.50)
        );
        assert_eq!(
            parse_uptime_load("12:00:00 up 1:00,  1 user,  load average: 0.42, 0.30, 0.25"),
            Some(0.42)
        );
    }
}
