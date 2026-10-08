//! Host-wide scheduler backpressure based on CPU utilization, available memory and critical filesystem usage.
use gitrun_core::Config;
use std::{fs, path::Path, process::Command};
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


fn sample_cpu_percent() -> Result<f32, ResourcePressureError> {
    fn read_total_idle() -> Result<(u64, u64), ResourcePressureError> {
        let stat = fs::read_to_string("/proc/stat")?;
        let line = stat
            .lines()
            .find(|line| line.starts_with("cpu "))
            .ok_or(ResourcePressureError::Parse)?;
        let mut fields = line.split_whitespace().skip(1).filter_map(|v| v.parse::<u64>().ok());
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
        return Ok(0.0);
    }
    Ok(((total_delta.saturating_sub(idle_delta) as f32 / total_delta as f32) * 100.0)
        .clamp(0.0, 100.0))
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
            continue;
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
            .ok_or(ResourcePressureError::Parse)?;
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

    #[test]
    fn pressure_is_triggered_by_any_threshold() {
        let mut c = Config::default();
        c.resource_pressure_cpu_percent = 80;
        c.resource_pressure_memory_percent = 90;
        c.resource_pressure_disk_percent = 95;
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
