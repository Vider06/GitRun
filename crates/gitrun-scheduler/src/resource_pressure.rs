//! Host-wide scheduler backpressure based on CPU load, available memory and filesystem usage.
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
    pub cpu_percent: f32,
    pub memory_percent: f32,
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
    let load = fs::read_to_string("/proc/loadavg")?;
    let load1 = load
        .split_whitespace()
        .next()
        .and_then(|v| v.parse::<f32>().ok())
        .ok_or(ResourcePressureError::Parse)?;
    let cpus = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1) as f32;
    let cpu_percent = ((load1 / cpus) * 100.0).clamp(0.0, 100.0);

    let meminfo = fs::read_to_string("/proc/meminfo")?;
    let mut total = None;
    let mut available = None;
    for line in meminfo.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = v.split_whitespace().next().and_then(|x| x.parse::<f32>().ok());
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            available = v.split_whitespace().next().and_then(|x| x.parse::<f32>().ok());
        }
    }
    let total = total.ok_or(ResourcePressureError::Parse)?;
    let available = available.unwrap_or(0.0);
    let memory_percent = if total > 0.0 {
        ((total - available) / total * 100.0).clamp(0.0, 100.0)
    } else {
        100.0
    };

    let path = Path::new(&config.state_dir);
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
    let disk_percent = String::from_utf8_lossy(&output.stdout)
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().nth(4))
        .and_then(|v| v.strip_suffix('%'))
        .and_then(|v| v.parse::<f32>().ok())
        .ok_or(ResourcePressureError::Parse)?;

    Ok(ResourceSnapshot {
        cpu_percent,
        memory_percent,
        disk_percent,
    })
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
        assert!(ResourceSnapshot { cpu_percent: 80.0, memory_percent: 1.0, disk_percent: 1.0 }.is_pressured(&c));
        assert!(ResourceSnapshot { cpu_percent: 1.0, memory_percent: 90.0, disk_percent: 1.0 }.is_pressured(&c));
        assert!(ResourceSnapshot { cpu_percent: 1.0, memory_percent: 1.0, disk_percent: 95.0 }.is_pressured(&c));
    }
}
