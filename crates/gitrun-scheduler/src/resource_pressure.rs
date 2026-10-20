//! Host-wide scheduler backpressure based on CPU utilization, available memory and critical filesystem usage.
use gitrun_core::Config;
use std::{fs, path::{Path, PathBuf}, process::Command};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ResourcePressureError {
    #[error("host resource probe failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("host resource probe returned invalid data")]
    Parse,
    #[error("filesystem usage probe failed for {path}: {detail}")]
    Disk { path: String, detail: String },
}

/// Conservative capacity left for new runner limits after a host reserve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceCapacity {
    pub cpu_cores: f64,
    pub memory_bytes: u64,
}

impl ResourceCapacity {
    /// Reserve 20% of schedulable CPU (at least half a core) and 15% of RAM
    /// (at least 1 GiB) for the host, GitRun, Docker and unrelated processes.
    /// These values are an upper-bound budget for newly planned containers,
    /// not a claim that all reported capacity is idle for the whole job.
    pub fn from_measurements(
        total_cpu_cores: f64,
        cpu_utilization_percent: f64,
        total_memory_bytes: u64,
        available_memory_bytes: u64,
    ) -> Option<Self> {
        const GIB: u64 = 1024 * 1024 * 1024;
        if !total_cpu_cores.is_finite()
            || total_cpu_cores <= 0.0
            || !cpu_utilization_percent.is_finite()
            || !(0.0..=100.0).contains(&cpu_utilization_percent)
            || total_memory_bytes == 0
            || available_memory_bytes > total_memory_bytes
        {
            return None;
        }
        let cpu_reserve = (total_cpu_cores * 0.20).max(0.5);
        let usable_cpu = (total_cpu_cores * (1.0 - cpu_utilization_percent / 100.0)
            - cpu_reserve)
            .max(0.0);
        let memory_reserve = ((total_memory_bytes as f64 * 0.15) as u64).max(GIB);
        let usable_memory = available_memory_bytes.saturating_sub(memory_reserve);
        Some(Self {
            cpu_cores: (usable_cpu * 1000.0).floor() / 1000.0,
            memory_bytes: usable_memory,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceSnapshot {
    /// Actual host CPU utilization over a short sampling interval.
    pub cpu_percent: f32,
    pub memory_percent: f32,
    /// Maximum filesystem utilization across all configured critical paths.
    pub disk_percent: f32,
}

impl ResourceSnapshot {
    pub fn is_pressured(&self, config: &Config) -> bool {
        self.cpu_percent >= config.resource_pressure_cpu_percent as f32
            || self.memory_percent >= config.resource_pressure_memory_percent as f32
            || self.disk_percent >= config.resource_pressure_disk_percent as f32
    }
}

pub fn sample(config: &Config) -> Result<ResourceSnapshot, ResourcePressureError> {
    let cpu_percent = sample_cpu_percent()?;

    let meminfo = fs::read_to_string("/proc/meminfo")?;
    let mut total = None;
    let mut available = None;
    for line in meminfo.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = v
                .split_whitespace()
                .next()
                .and_then(|x| x.parse::<f32>().ok());
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            available = v
                .split_whitespace()
                .next()
                .and_then(|x| x.parse::<f32>().ok());
        }
    }
    let total = total.ok_or(ResourcePressureError::Parse)?;
    let available = available.unwrap_or(0.0);
    let memory_percent = if total > 0.0 {
        ((total - available) / total * 100.0).clamp(0.0, 100.0)
    } else {
        100.0
    };

    let disk_percent = sample_disk_percent(config)?;

    Ok(ResourceSnapshot {
        cpu_percent,
        memory_percent,
        disk_percent,
    })
}

/// Estimate capacity for Adaptive Resource Management independently of
/// the scheduler's optional scale-up pressure gate. Errors are propagated to
/// the adaptive caller, which must defer a runner rather than guess capacity.
#[derive(Debug, Default)]
struct CgroupV2Limits {
    cpu_quota_cores: Option<f64>,
    cpu_utilization_percent: Option<f64>,
    memory_limit_bytes: Option<u64>,
    memory_available_bytes: Option<u64>,
}

fn process_cgroup_v2_directory() -> Result<Option<PathBuf>, ResourcePressureError> {
    let contents = match fs::read_to_string("/proc/self/cgroup") {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(relative) = contents.lines().find_map(|line| line.strip_prefix("0::")) else {
        let legacy_limited_controller = contents.lines().any(|line| {
            let Some((controllers, _)) =
                line.split_once(':').and_then(|(_, rest)| rest.split_once(':'))
            else {
                return false;
            };
            controllers.split(',').any(|name| matches!(name, "cpu" | "cpuacct" | "memory"))
        });
        // Do not silently use host figures when legacy CPU or memory controllers
        // may impose a tighter quota that this probe doesn't know how to read.
        if legacy_limited_controller {
            return Err(ResourcePressureError::Parse);
        }
        return Ok(None);
    };
    let relative = relative.trim_start_matches('/');
    if relative.split('/').any(|part| part == "." || part == "..") {
        return Err(ResourcePressureError::Parse);
    }
    let root = Path::new("/sys/fs/cgroup");
    let directory = if relative.is_empty() { root.to_path_buf() } else { root.join(relative) };
    if directory.is_dir() && directory.starts_with(root) {
        Ok(Some(directory))
    } else {
        Err(ResourcePressureError::Parse)
    }
}

fn read_optional_cgroup_file(path: &Path) -> Result<Option<String>, ResourcePressureError> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn parse_cgroup_cpu_max(raw: &str) -> Result<Option<f64>, ResourcePressureError> {
    let fields: Vec<_> = raw.split_whitespace().collect();
    if fields.len() != 2 {
        return Err(ResourcePressureError::Parse);
    }
    let period = fields[1].parse::<u64>().map_err(|_| ResourcePressureError::Parse)?;
    if period == 0 {
        return Err(ResourcePressureError::Parse);
    }
    if fields[0] == "max" {
        return Ok(None);
    }
    let quota = fields[0].parse::<u64>().map_err(|_| ResourcePressureError::Parse)?;
    if quota == 0 {
        return Err(ResourcePressureError::Parse);
    }
    let cores = quota as f64 / period as f64;
    if !cores.is_finite() || cores <= 0.0 {
        return Err(ResourcePressureError::Parse);
    }
    Ok(Some(cores))
}

fn parse_cgroup_memory_limit(raw: &str) -> Result<Option<u64>, ResourcePressureError> {
    let value = raw.trim();
    if value == "max" {
        return Ok(None);
    }
    let bytes = value.parse::<u64>().map_err(|_| ResourcePressureError::Parse)?;
    if bytes == 0 {
        return Err(ResourcePressureError::Parse);
    }
    Ok(Some(bytes))
}

fn read_cgroup_cpu_usage_usec(directory: &Path) -> Result<u64, ResourcePressureError> {
    let contents = fs::read_to_string(directory.join("cpu.stat"))?;
    contents.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let name = fields.next()?;
        let value = fields.next()?;
        (name == "usage_usec").then(|| value.parse::<u64>().ok()).flatten()
    }).ok_or(ResourcePressureError::Parse)
}

fn sample_cgroup_cpu_percent(
    directory: &Path,
    quota_cores: f64,
) -> Result<f64, ResourcePressureError> {
    let before = read_cgroup_cpu_usage_usec(directory)?;
    let started = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let after = read_cgroup_cpu_usage_usec(directory)?;
    let elapsed_usec = started.elapsed().as_micros().max(1) as f64;
    let usage_delta_usec = after.saturating_sub(before) as f64;
    let percent = usage_delta_usec / elapsed_usec / quota_cores * 100.0;
    if !percent.is_finite() || percent < 0.0 {
        return Err(ResourcePressureError::Parse);
    }
    Ok(percent.clamp(0.0, 100.0))
}

fn read_cgroup_v2_limits() -> Result<CgroupV2Limits, ResourcePressureError> {
    let Some(directory) = process_cgroup_v2_directory()? else {
        return Ok(CgroupV2Limits::default());
    };
    let mount_root = Path::new("/sys/fs/cgroup");
    let mut current = Some(directory.as_path());
    let mut cpu_quota_cores: Option<f64> = None;
    let mut cpu_utilization_percent: Option<f64> = None;
    let mut memory_limit_bytes: Option<u64> = None;
    let mut memory_available_bytes: Option<u64> = None;
    while let Some(path) = current {
        if let Some(raw) = read_optional_cgroup_file(&path.join("cpu.max"))? {
            if let Some(quota) = parse_cgroup_cpu_max(&raw)? {
                cpu_quota_cores = Some(cpu_quota_cores.map_or(quota, |old| old.min(quota)));
                let usage = sample_cgroup_cpu_percent(path, quota)?;
                cpu_utilization_percent =
                    Some(cpu_utilization_percent.map_or(usage, |old| old.max(usage)));
            }
        }
        if let Some(raw) = read_optional_cgroup_file(&path.join("memory.max"))? {
            if let Some(limit) = parse_cgroup_memory_limit(&raw)? {
                memory_limit_bytes = Some(memory_limit_bytes.map_or(limit, |old| old.min(limit)));
                let current_bytes = read_optional_cgroup_file(&path.join("memory.current"))?
                    .ok_or(ResourcePressureError::Parse)?
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| ResourcePressureError::Parse)?;
                let available = limit.saturating_sub(current_bytes);
                memory_available_bytes =
                    Some(memory_available_bytes.map_or(available, |old| old.min(available)));
            }
        }
        if path == mount_root {
            break;
        }
        current = path.parent().filter(|parent| parent.starts_with(mount_root));
    }
    Ok(CgroupV2Limits {
        cpu_quota_cores,
        cpu_utilization_percent,
        memory_limit_bytes,
        memory_available_bytes,
    })
}

/// Estimate capacity for Adaptive Resource Management independently of
/// the scheduler's scale-up pressure gate. The caller defers runner creation
/// if capacity cannot be measured without guessing. All visible cgroup-v2
/// ancestor limits are applied as upper bounds on host capacity.
pub fn available_capacity() -> Result<ResourceCapacity, ResourcePressureError> {
    let mut total_cpu_cores = std::thread::available_parallelism()
        .map(|cores| cores.get() as f64)
        .unwrap_or(0.0);
    let mut cpu_percent = f64::from(sample_cpu_percent()?);
    let cgroup = read_cgroup_v2_limits()?;
    if let Some(quota) = cgroup.cpu_quota_cores {
        total_cpu_cores = total_cpu_cores.min(quota);
    }
    if let Some(cgroup_cpu_percent) = cgroup.cpu_utilization_percent {
        cpu_percent = cpu_percent.max(cgroup_cpu_percent);
    }

    let meminfo = fs::read_to_string("/proc/meminfo")?;
    let mut total_kib = None;
    let mut available_kib = None;
    for line in meminfo.lines() {
        if let Some(value) = line.strip_prefix("MemTotal:") {
            total_kib = value.split_whitespace().next().and_then(|v| v.parse::<u64>().ok());
        } else if let Some(value) = line.strip_prefix("MemAvailable:") {
            available_kib = value.split_whitespace().next().and_then(|v| v.parse::<u64>().ok());
        }
    }
    let mut total_bytes = total_kib.and_then(|v| v.checked_mul(1024))
        .ok_or(ResourcePressureError::Parse)?;
    let mut available_bytes = available_kib.and_then(|v| v.checked_mul(1024))
        .ok_or(ResourcePressureError::Parse)?;
    if let Some(limit) = cgroup.memory_limit_bytes {
        total_bytes = total_bytes.min(limit);
    }
    if let Some(available) = cgroup.memory_available_bytes {
        available_bytes = available_bytes.min(available);
    }
    ResourceCapacity::from_measurements(total_cpu_cores, cpu_percent, total_bytes, available_bytes)
        .ok_or(ResourcePressureError::Parse)
}

fn sample_cpu_percent() -> Result<f32, ResourcePressureError> {
    fn read_total_idle() -> Result<(u64, u64), ResourcePressureError> {
        let stat = fs::read_to_string("/proc/stat")?;
        let line = stat
            .lines()
            .find(|line| line.starts_with("cpu "))
            .ok_or(ResourcePressureError::Parse)?;
        let mut fields = line
            .split_whitespace()
            .skip(1)
            .filter_map(|v| v.parse::<u64>().ok());
        let user = fields.next().ok_or(ResourcePressureError::Parse)?;
        let nice = fields.next().ok_or(ResourcePressureError::Parse)?;
        let system = fields.next().ok_or(ResourcePressureError::Parse)?;
        let idle = fields.next().ok_or(ResourcePressureError::Parse)?;
        let iowait = fields.next().unwrap_or(0);
        let irq = fields.next().unwrap_or(0);
        let softirq = fields.next().unwrap_or(0);
        let steal = fields.next().unwrap_or(0);
        let total = user
            .saturating_add(nice)
            .saturating_add(system)
            .saturating_add(idle)
            .saturating_add(iowait)
            .saturating_add(irq)
            .saturating_add(softirq)
            .saturating_add(steal);
        let idle = idle.saturating_add(iowait);
        Ok((total, idle))
    }

    let (total_a, idle_a) = read_total_idle()?;
    std::thread::sleep(std::time::Duration::from_millis(100));
    let (total_b, idle_b) = read_total_idle()?;
    let total_delta = total_b.saturating_sub(total_a);
    let idle_delta = idle_b.saturating_sub(idle_a);
    if total_delta == 0 {
        // A zero-width sample is not evidence of an idle host. Treat it as
        // unavailable so the scheduler's caller can fail closed.
        return Err(ResourcePressureError::Parse);
    }
    Ok(
        ((total_delta.saturating_sub(idle_delta) as f32 / total_delta as f32) * 100.0)
            .clamp(0.0, 100.0),
    )
}

fn sample_disk_percent(config: &Config) -> Result<f32, ResourcePressureError> {
    let mut max_usage = 0.0_f32;
    let mut found = false;
    for raw_path in config.resource_pressure_paths.split(';') {
        let raw_path = raw_path.trim();
        if raw_path.is_empty() {
            continue;
        }
        let path = Path::new(raw_path);
        if !path.exists() {
            return Err(ResourcePressureError::Disk {
                path: path.display().to_string(),
                detail: "configured critical filesystem path does not exist".into(),
            });
        }
        found = true;
        let output = Command::new("df")
            .arg("-P")
            .arg(path)
            .output()
            .map_err(|e| ResourcePressureError::Disk {
                path: path.display().to_string(),
                detail: e.to_string(),
            })?;
        if !output.status.success() {
            return Err(ResourcePressureError::Disk {
                path: path.display().to_string(),
                detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        let usage = String::from_utf8_lossy(&output.stdout)
            .lines()
            .nth(1)
            .and_then(|line| line.split_whitespace().nth(4))
            .and_then(|v| v.strip_suffix('%'))
            .and_then(|v| v.parse::<f32>().ok())
            .ok_or_else(|| ResourcePressureError::Disk {
                path: path.display().to_string(),
                detail: "could not parse filesystem usage percentage from df output".into(),
            })?;
        max_usage = max_usage.max(usage);
    }
    if !found {
        return Err(ResourcePressureError::Parse);
    }
    Ok(max_usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn cgroup_v2_cpu_and_memory_limits_are_parsed_strictly() {
        assert_eq!(parse_cgroup_cpu_max("150000 100000").unwrap(), Some(1.5));
        assert_eq!(parse_cgroup_cpu_max("max 100000").unwrap(), None);
        assert!(parse_cgroup_cpu_max("0 100000").is_err());
        assert!(parse_cgroup_cpu_max("200000 0").is_err());
        assert_eq!(parse_cgroup_memory_limit("1073741824").unwrap(), Some(1_073_741_824));
        assert_eq!(parse_cgroup_memory_limit("max").unwrap(), None);
        assert!(parse_cgroup_memory_limit("0").is_err());
        assert!(parse_cgroup_memory_limit("not-a-number").is_err());
    }

    #[test]
    fn missing_configured_disk_path_is_an_error() {
        let missing = std::env::temp_dir().join(format!(
            "gitrun-missing-critical-fs-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config = Config {
            resource_pressure_paths: missing.display().to_string(),
            ..Config::default()
        };

        let error = sample_disk_percent(&config).unwrap_err();
        assert!(matches!(
            error,
            ResourcePressureError::Disk { path, .. }
                if path == missing.display().to_string()
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_critical_path_is_not_ignored_when_another_path_exists() {
        let existing = std::env::temp_dir().join(format!(
            "gitrun-critical-fs-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&existing).unwrap();
        let missing = existing.join("missing-critical-path");
        let config = Config {
            resource_pressure_paths: format!("{};{}", existing.display(), missing.display()),
            ..Config::default()
        };

        let result = sample_disk_percent(&config);
        std::fs::remove_dir_all(&existing).unwrap();
        let error = result.unwrap_err();
        assert!(matches!(
            error,
            ResourcePressureError::Disk { path, .. }
                if path == missing.display().to_string()
        ));
    }

    #[test]
    fn adaptive_capacity_keeps_host_reserves_and_never_exceeds_available_ram() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let capacity = ResourceCapacity::from_measurements(4.0, 0.0, 16 * GIB, 12 * GIB).unwrap();
        assert!(capacity.cpu_cores <= 3.2);
        assert_eq!(capacity.memory_bytes, 12 * GIB - (16 * GIB * 15 / 100));

        let pressured = ResourceCapacity::from_measurements(4.0, 100.0, 16 * GIB, 12 * GIB).unwrap();
        assert_eq!(pressured.cpu_cores, 0.0);
        assert!(ResourceCapacity::from_measurements(0.0, 20.0, 16 * GIB, 12 * GIB).is_none());
        assert!(ResourceCapacity::from_measurements(4.0, 20.0, 16 * GIB, 20 * GIB).is_none());
    }

    #[test]
    fn pressure_is_triggered_by_any_threshold() {
        let c = Config {
            resource_pressure_cpu_percent: 80,
            resource_pressure_memory_percent: 90,
            resource_pressure_disk_percent: 95,
            ..Config::default()
        };
        assert!(ResourceSnapshot {
            cpu_percent: 80.0,
            memory_percent: 1.0,
            disk_percent: 1.0
        }
        .is_pressured(&c));
        assert!(ResourceSnapshot {
            cpu_percent: 1.0,
            memory_percent: 90.0,
            disk_percent: 1.0
        }
        .is_pressured(&c));
        assert!(ResourceSnapshot {
            cpu_percent: 1.0,
            memory_percent: 1.0,
            disk_percent: 95.0
        }
        .is_pressured(&c));
    }
}
