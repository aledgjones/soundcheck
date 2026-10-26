//! The Mac that runs the simulator and the emulator. Its signals hold dispatches and the run's start, and a device
//! restart can't fix them.

use crate::device::{Health, Result};
use crate::ios::run;

const MIN_FREE_MEMORY_PERCENT: f64 = 10.0;
const MAX_CPU_BUSY: f64 = 0.80;

/// Adds the Mac's free memory, the kernel's memory pressure and its busy CPU over two seconds to `health`, as host
/// signals.
pub async fn sample(health: &mut Health) -> Result<()> {
    let pressure = run("memory_pressure", &["-Q"]).await?;
    if let Some(free) = pressure.lines().find_map(|l| {
        l.strip_prefix("System-wide memory free percentage:")?
            .trim()
            .trim_end_matches('%')
            .parse::<f64>()
            .ok()
    }) {
        health.host(
            format!("Mac memory {free:.0}% free"),
            free < MIN_FREE_MEMORY_PERCENT,
        );
    }
    // The kernel's own reading. Swap and compressed memory stay high long after pressure has passed, and "warning"
    // is the Mac's steady state with an emulator, a simulator and dev tools up (2026-10-26): only critical holds.
    let level = run("sysctl", &["-n", "kern.memorystatus_vm_pressure_level"]).await?;
    if let Some(level) = pressure_level(&level) {
        health.host(format!("Mac memory pressure {level}"), level == "critical");
    }
    // Busy CPU, not the load average: macOS counts threads waiting on disk, so paging read as load 100+ with the CPU
    // 60% idle (2026-10-24).
    let top = run("top", &["-l", "2", "-s", "1", "-n", "0"]).await?;
    if let Some(busy) = cpu_busy(&top) {
        health.host(
            format!("Mac CPU {:.0}% busy", busy * 100.0),
            busy > MAX_CPU_BUSY,
        );
    }

    Ok(())
}

/// Why the Mac is busy, if it is: for waiting on it before any device is touched.
pub async fn busy() -> Result<Vec<String>> {
    let mut health = Health::default();
    sample(&mut health).await?;
    Ok(health.busy)
}

/// `kern.memorystatus_vm_pressure_level` by name.
fn pressure_level(sysctl: &str) -> Option<&'static str> {
    match sysctl.trim() {
        "1" => Some("normal"),
        "2" => Some("warning"),
        "4" => Some("critical"),
        _ => None,
    }
}

/// The busy share of the CPU from `top -l 2`'s last sample; the first covers only the instant top started.
fn cpu_busy(top: &str) -> Option<f64> {
    let line = top.lines().rfind(|l| l.starts_with("CPU usage:"))?;
    let idle = line
        .split(',')
        .find(|part| part.contains("idle"))?
        .trim()
        .split('%')
        .next()?
        .trim()
        .parse::<f64>()
        .ok()?;
    Some((100.0 - idle) / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mac_is_judged_by_memory_pressure_and_busy_cpu() {
        assert_eq!(pressure_level("1\n"), Some("normal"));
        assert_eq!(pressure_level("4"), Some("critical"));
        assert_eq!(pressure_level(""), None);
        let top = "Processes: 712 total\n\
            CPU usage: 3.27% user, 9.1% sys, 87.61% idle \n\
            Processes: 712 total\n\
            CPU usage: 61.5% user, 30.0% sys, 8.50% idle \n";
        assert!((cpu_busy(top).unwrap() - 0.915).abs() < 1e-9);
        assert_eq!(cpu_busy("no samples"), None);
    }
}
