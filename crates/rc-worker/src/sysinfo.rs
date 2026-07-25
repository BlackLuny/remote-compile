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

/// Free space in GB on the filesystem holding `path`.
pub fn disk_free_gb(path: &Path) -> u64 {
    let Ok(out) = std::process::Command::new("df")
        .arg("-Pk")
        .arg(path)
        .output()
    else {
        return 0;
    };
    parse_df_available_kb(&String::from_utf8_lossy(&out.stdout))
        .map(|kb| kb / (1024 * 1024))
        .unwrap_or(0)
}

/// `df -Pk` guarantees POSIX output: a header line, then
/// `Filesystem 1024-blocks Used Available Capacity Mounted-on`.
fn parse_df_available_kb(output: &str) -> Option<u64> {
    let line = output.lines().nth(1)?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    // A long device name can wrap onto the next line; POSIX mode prevents
    // that, but stay defensive and index from the end instead.
    if fields.len() < 5 {
        let joined: Vec<&str> = output.lines().skip(1).flat_map(|l| l.split_whitespace()).collect();
        return joined.get(joined.len().checked_sub(3)?)?.parse().ok();
    }
    fields[fields.len() - 3].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_posix_df_output() {
        let out = "Filesystem 1024-blocks      Used Available Capacity Mounted on\n\
                   /dev/nvme0n1p2 982940788 123456789 809483999      14% /\n";
        assert_eq!(parse_df_available_kb(out), Some(809_483_999));
    }

    #[test]
    fn parses_df_with_a_wrapped_device_name() {
        let out = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                   /dev/mapper/a-very-long-logical-volume-name\n\
                    982940788 123456789 809483999 14% /\n";
        assert_eq!(parse_df_available_kb(out), Some(809_483_999));
    }

    #[test]
    fn malformed_df_output_does_not_panic() {
        assert_eq!(parse_df_available_kb(""), None);
        assert_eq!(parse_df_available_kb("Filesystem\n"), None);
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
        assert!(disk_free_gb(&std::env::temp_dir()) > 0);
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
