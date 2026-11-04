#![forbid(unsafe_code)]

//! Host platform detection shared by GitRun entry points.
//!
//! Detection is deliberately conservative: unknown libc ABIs and unknown
//! service/package managers remain unknown instead of being guessed.

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

pub const ANDROID_UNSUPPORTED_MESSAGE: &str =
    "GitRun cannot run on Android. Android, Termux and Android-based proot environments are not supported.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxAbi {
    Gnu,
    Musl,
}

impl LinuxAbi {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gnu => "gnu",
            Self::Musl => "musl",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Apt,
    Dnf,
    Zypper,
    Pacman,
    Apk,
    Xbps,
    Emerge,
    Rpm,
}

impl PackageManager {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Apt => "apt/dpkg",
            Self::Dnf => "dnf/rpm",
            Self::Zypper => "zypper/rpm",
            Self::Pacman => "pacman",
            Self::Apk => "apk",
            Self::Xbps => "xbps",
            Self::Emerge => "emerge",
            Self::Rpm => "rpm",
        }
    }

    const fn command(self) -> &'static str {
        match self {
            Self::Apt => "apt-get",
            Self::Dnf => "dnf",
            Self::Zypper => "zypper",
            Self::Pacman => "pacman",
            Self::Apk => "apk",
            Self::Xbps => "xbps-install",
            Self::Emerge => "emerge",
            Self::Rpm => "rpm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceManager {
    Systemd,
    OpenRc,
    Runit,
}

impl ServiceManager {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::OpenRc => "OpenRC",
            Self::Runit => "runit",
        }
    }
}

/// Signals used to distinguish Android/Termux/proot from ordinary Linux.
/// Exposed as a small value type so the detection rules can be tested without
/// depending on the machine on which the tests happen to run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AndroidRuntimeSignals {
    pub target_is_android: bool,
    pub termux_version: Option<String>,
    pub termux_app_pid: Option<String>,
    pub prefix: Option<String>,
    pub android_root: Option<String>,
    pub android_data: Option<String>,
    pub proot_marker_present: bool,
    pub system_getprop_present: bool,
    pub system_build_prop_present: bool,
}

pub fn is_android_runtime(signals: &AndroidRuntimeSignals) -> bool {
    if signals.target_is_android
        || signals.termux_version.as_deref().is_some_and(non_empty)
        || signals.termux_app_pid.as_deref().is_some_and(non_empty)
    {
        return true;
    }

    let termux_prefix = signals.prefix.as_deref().is_some_and(|prefix| {
        prefix.starts_with("/data/data/com.termux/files/usr")
            || prefix.starts_with("/data/user/0/com.termux/files/usr")
    });
    if termux_prefix {
        return true;
    }

    // Avoid false positives when Linux guests inherit unrelated Android environment variables.
    let android_system_files = signals.system_getprop_present || signals.system_build_prop_present;
    let android_root = signals.android_root.as_deref() == Some("/system");
    let android_data = signals.android_data.as_deref() == Some("/data");

    // In a proot distribution the guest reports Linux, but Android host
    // paths/environment can remain visible. Require Android-specific system
    // evidence; PROOT_* alone is not enough to reject an ordinary Linux proot.
    (android_system_files && (android_root || android_data || signals.proot_marker_present))
        || (android_root && android_data && signals.proot_marker_present)
}

fn non_empty(value: &str) -> bool {
    !value.trim().is_empty()
}

fn current_android_signals() -> AndroidRuntimeSignals {
    let value = |name: &str| env::var(name).ok();
    AndroidRuntimeSignals {
        target_is_android: env::consts::OS == "android",
        termux_version: value("TERMUX_VERSION"),
        termux_app_pid: value("TERMUX_APP_PID"),
        prefix: value("PREFIX"),
        android_root: value("ANDROID_ROOT"),
        android_data: value("ANDROID_DATA"),
        proot_marker_present: value("PROOT_TMP_DIR").is_some() || value("PROOT_LOADER").is_some(),
        system_getprop_present: Path::new("/system/bin/getprop").is_file(),
        system_build_prop_present: Path::new("/system/build.prop").is_file(),
    }
}

pub fn is_android() -> bool {
    is_android_runtime(&current_android_signals())
}

pub fn ensure_supported_runtime() -> Result<(), &'static str> {
    if is_android() {
        Err(ANDROID_UNSUPPORTED_MESSAGE)
    } else {
        Ok(())
    }
}

pub fn parse_os_release(contents: &str, key: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let (name, raw_value) = line.split_once('=')?;
        if name.trim() != key {
            return None;
        }
        let value = raw_value.trim();
        let unquoted = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or(value);
        Some(unquoted.to_owned())
    })
}

fn os_release_value(key: &str) -> Option<String> {
    let contents = fs::read_to_string("/etc/os-release").ok()?;
    parse_os_release(&contents, key)
}

fn command_on_path(command: &str) -> bool {
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .any(|directory| directory.join(command).is_file())
}

fn push_unique(values: &mut Vec<PackageManager>, value: PackageManager) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn preferred_package_managers(distro_id: &str, id_like: &str) -> Vec<PackageManager> {
    let mut ordered = Vec::new();
    for id in std::iter::once(distro_id).chain(id_like.split_ascii_whitespace()) {
        match id.to_ascii_lowercase().as_str() {
            "alpine" => push_unique(&mut ordered, PackageManager::Apk),
            "void" => push_unique(&mut ordered, PackageManager::Xbps),
            "arch" | "archlinux" | "manjaro" | "endeavouros" => {
                push_unique(&mut ordered, PackageManager::Pacman)
            }
            "debian" | "ubuntu" | "linuxmint" | "pop" | "neon" => {
                push_unique(&mut ordered, PackageManager::Apt)
            }
            "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => {
                push_unique(&mut ordered, PackageManager::Dnf);
                push_unique(&mut ordered, PackageManager::Rpm);
            }
            "opensuse" | "opensuse-leap" | "opensuse-tumbleweed" | "suse" => {
                push_unique(&mut ordered, PackageManager::Zypper);
                push_unique(&mut ordered, PackageManager::Rpm);
            }
            "gentoo" => push_unique(&mut ordered, PackageManager::Emerge),
            _ => {}
        }
    }

    ordered
}

pub fn detect_package_manager(distro_id: &str, id_like: &str) -> Option<PackageManager> {
    preferred_package_managers(distro_id, id_like)
        .into_iter()
        .find(|manager| command_on_path(manager.command()))
}

fn detect_service_manager_from_signals(
    init_name: &str,
    systemd_runtime: bool,
    openrc_runtime: bool,
    runit_runtime: bool,
) -> Option<ServiceManager> {
    // An identified PID 1 is stronger evidence than leftover runtime mounts
    // exposed by a container or compatibility layer.
    match init_name {
        "systemd" => return Some(ServiceManager::Systemd),
        "openrc-init" => return Some(ServiceManager::OpenRc),
        "runsvdir" | "runit" => return Some(ServiceManager::Runit),
        _ => {}
    }

    if openrc_runtime {
        Some(ServiceManager::OpenRc)
    } else if runit_runtime {
        Some(ServiceManager::Runit)
    } else if systemd_runtime {
        Some(ServiceManager::Systemd)
    } else {
        None
    }
}
//cat
pub fn detect_service_manager() -> Option<ServiceManager> {
    // Prefer the active PID 1/init runtime over installed management commands.
    // A host may have systemctl, rc-service or sv installed without running
    // the corresponding init system.
    let init_name = fs::read_to_string("/proc/1/comm")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    let systemd_runtime = Path::new("/run/systemd/system").is_dir() || init_name == "systemd";
    let openrc_runtime = Path::new("/run/openrc/softlevel").is_file() || init_name == "openrc-init";
    let runit_runtime =
        Path::new("/run/runit/service").is_dir() || init_name == "runsvdir" || init_name == "runit";

    detect_service_manager_from_signals(&init_name, systemd_runtime, openrc_runtime, runit_runtime)
}

fn contains_musl_loader(directory: &Path) -> bool {
    fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().starts_with("ld-musl-"))
}

fn has_glibc_loader() -> bool {
    [
        "/lib64/ld-linux-x86-64.so.2",
        "/usr/lib64/ld-linux-x86-64.so.2",
        "/lib/ld-linux-x86-64.so.2",
        "/usr/lib/ld-linux-x86-64.so.2",
        "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
        "/lib/ld-linux-aarch64.so.1",
        "/usr/lib/ld-linux-aarch64.so.1",
        "/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1",
        "/lib/ld-linux-armhf.so.3",
        "/usr/lib/ld-linux-armhf.so.3",
    ]
    .iter()
    .any(|path| Path::new(path).is_file())
}

pub fn detect_linux_abi() -> Result<LinuxAbi, String> {
    if env::consts::OS != "linux" {
        return Err(format!(
            "libc ABI detection is only available on Linux (current OS: {})",
            env::consts::OS
        ));
    }

    if contains_musl_loader(Path::new("/lib"))
        || contains_musl_loader(Path::new("/usr/lib"))
        || contains_musl_loader(Path::new("/lib64"))
    {
        return Ok(LinuxAbi::Musl);
    }

    if let Ok(output) = Command::new("ldd").arg("--version").output() {
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .to_ascii_lowercase();
        if combined.contains("musl") {
            return Ok(LinuxAbi::Musl);
        }
        if combined.contains("gnu libc") || combined.contains("glibc") {
            return Ok(LinuxAbi::Gnu);
        }
    }

    if let Ok(output) = Command::new("getconf").arg("GNU_LIBC_VERSION").output() {
        if output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .trim()
                .starts_with("glibc ")
        {
            return Ok(LinuxAbi::Gnu);
        }
    }

    if has_glibc_loader() {
        return Ok(LinuxAbi::Gnu);
    }

    Err("unable to identify the Linux libc ABI; refusing to assume glibc or musl".into())
}

pub fn linux_target_triple_for(architecture: &str, abi: LinuxAbi) -> Result<String, String> {
    let environment = abi.as_str();
    match (architecture, abi) {
        ("x86_64", LinuxAbi::Gnu) => Ok("x86_64-unknown-linux-gnu".into()),
        ("aarch64", LinuxAbi::Gnu) => Ok("aarch64-unknown-linux-gnu".into()),
        ("x86_64", LinuxAbi::Musl) => Ok("x86_64-unknown-linux-musl".into()),
        ("aarch64", LinuxAbi::Musl) => Ok("aarch64-unknown-linux-musl".into()),
        _ => Err(format!(
            "GitRun has no declared Linux target for {architecture}-{environment}"
        )),
    }
}

pub fn linux_target_triple() -> Result<String, String> {
    if env::consts::OS != "linux" {
        return Err(format!(
            "Linux target detection is unavailable on {}",
            env::consts::OS
        ));
    }
    let abi = detect_linux_abi()?;
    linux_target_triple_for(env::consts::ARCH, abi)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxPlatformInfo {
    pub distribution: Option<String>,
    pub architecture: String,
    pub abi: Option<LinuxAbi>,
    pub target_triple: Option<String>,
    pub package_manager: Option<PackageManager>,
    pub service_manager: Option<ServiceManager>,
}

pub fn inspect_linux() -> Option<LinuxPlatformInfo> {
    if env::consts::OS != "linux" {
        return None;
    }

    let distro_id = os_release_value("ID").unwrap_or_default();
    let id_like = os_release_value("ID_LIKE").unwrap_or_default();
    let abi = detect_linux_abi().ok();
    let package_manager = detect_package_manager(&distro_id, &id_like);

    Some(LinuxPlatformInfo {
        distribution: if distro_id.is_empty() {
            None
        } else {
            Some(distro_id)
        },
        architecture: env::consts::ARCH.to_owned(),
        abi,
        target_triple: abi.and_then(|abi| linux_target_triple_for(env::consts::ARCH, abi).ok()),
        package_manager,
        service_manager: detect_service_manager(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_target_is_rejected() {
        let signals = AndroidRuntimeSignals {
            target_is_android: true,
            ..Default::default()
        };
        assert!(is_android_runtime(&signals));
    }

    #[test]
    fn termux_environment_is_rejected() {
        let signals = AndroidRuntimeSignals {
            termux_version: Some("0.118.0".into()),
            ..Default::default()
        };
        assert!(is_android_runtime(&signals));
    }

    #[test]
    fn termux_prefix_is_rejected_without_relying_on_android_home() {
        let signals = AndroidRuntimeSignals {
            prefix: Some("/data/data/com.termux/files/usr".into()),
            ..Default::default()
        };
        assert!(is_android_runtime(&signals));

        let ordinary_linux = AndroidRuntimeSignals {
            prefix: Some("/usr".into()),
            ..Default::default()
        };
        assert!(!is_android_runtime(&ordinary_linux));
    }

    #[test]
    fn android_proot_is_rejected_when_android_host_evidence_is_visible() {
        let signals = AndroidRuntimeSignals {
            android_root: Some("/system".into()),
            android_data: Some("/data".into()),
            proot_marker_present: true,
            system_getprop_present: true,
            ..Default::default()
        };
        assert!(is_android_runtime(&signals));
    }

    #[test]
    fn ordinary_linux_and_android_sdk_variables_are_not_rejected() {
        let signals = AndroidRuntimeSignals {
            ..Default::default()
        };
        assert!(!is_android_runtime(&signals));
    }

    #[test]
    fn android_detection_does_not_treat_proot_alone_as_android() {
        let signals = AndroidRuntimeSignals {
            proot_marker_present: true,
            ..Default::default()
        };
        assert!(!is_android_runtime(&signals));
    }

    #[test]
    fn os_release_values_support_single_and_double_quotes() {
        let release = "ID=alpine\nID_LIKE='debian'\nNAME=\"Alpine Linux\"";
        assert_eq!(parse_os_release(release, "ID").as_deref(), Some("alpine"));
        assert_eq!(
            parse_os_release(release, "ID_LIKE").as_deref(),
            Some("debian")
        );
        assert_eq!(
            parse_os_release(release, "NAME").as_deref(),
            Some("Alpine Linux")
        );
    }

    #[test]
    fn distribution_preference_keeps_native_package_formats_first() {
        assert_eq!(
            preferred_package_managers("alpine", ""),
            vec![PackageManager::Apk]
        );
        assert_eq!(
            preferred_package_managers("void", ""),
            vec![PackageManager::Xbps]
        );
        assert_eq!(
            preferred_package_managers("unknown-linux", ""),
            Vec::<PackageManager>::new()
        );
        assert_eq!(
            preferred_package_managers("unknown", "fedora"),
            vec![PackageManager::Dnf, PackageManager::Rpm]
        );
    }

    #[test]
    fn known_distributions_prefer_their_native_package_manager() {
        let cases = [
            ("debian", "", vec![PackageManager::Apt]),
            ("ubuntu", "debian", vec![PackageManager::Apt]),
            ("alpine", "", vec![PackageManager::Apk]),
            ("void", "", vec![PackageManager::Xbps]),
            ("arch", "", vec![PackageManager::Pacman]),
            ("manjaro", "arch", vec![PackageManager::Pacman]),
            ("fedora", "", vec![PackageManager::Dnf, PackageManager::Rpm]),
            (
                "opensuse-tumbleweed",
                "",
                vec![PackageManager::Zypper, PackageManager::Rpm],
            ),
            ("gentoo", "", vec![PackageManager::Emerge]),
        ];

        for (distribution, id_like, expected) in cases {
            assert_eq!(
                preferred_package_managers(distribution, id_like),
                expected,
                "unexpected package-manager preference for {distribution}"
            );
        }
    }

    #[test]
    fn systemd_openrc_and_runit_pid1_names_are_detected_independently() {
        assert_eq!(
            detect_service_manager_from_signals("systemd", false, false, false),
            Some(ServiceManager::Systemd)
        );
        assert_eq!(
            detect_service_manager_from_signals("openrc-init", false, false, false),
            Some(ServiceManager::OpenRc)
        );
        assert_eq!(
            detect_service_manager_from_signals("runsvdir", false, false, false),
            Some(ServiceManager::Runit)
        );
        assert_eq!(
            detect_service_manager_from_signals("runit", false, false, false),
            Some(ServiceManager::Runit)
        );
    }

    #[test]
    fn service_manager_runtime_markers_are_fallbacks_not_command_presence() {
        assert_eq!(
            detect_service_manager_from_signals("init", false, true, false),
            Some(ServiceManager::OpenRc)
        );
        assert_eq!(
            detect_service_manager_from_signals("init", false, false, true),
            Some(ServiceManager::Runit)
        );
        assert_eq!(
            detect_service_manager_from_signals("init", true, false, false),
            Some(ServiceManager::Systemd)
        );
        assert_eq!(
            detect_service_manager_from_signals("init", false, false, false),
            None
        );
    }

    #[test]
    fn active_pid1_wins_over_stale_service_manager_runtime_markers() {
        assert_eq!(
            detect_service_manager_from_signals("runsvdir", true, true, false),
            Some(ServiceManager::Runit)
        );
    }

    #[test]
    fn target_triples_preserve_architecture_and_libc_abi() {
        assert_eq!(
            linux_target_triple_for("x86_64", LinuxAbi::Gnu).unwrap(),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            linux_target_triple_for("x86_64", LinuxAbi::Musl).unwrap(),
            "x86_64-unknown-linux-musl"
        );
        assert_eq!(
            linux_target_triple_for("aarch64", LinuxAbi::Gnu).unwrap(),
            "aarch64-unknown-linux-gnu"
        );
        assert_eq!(
            linux_target_triple_for("aarch64", LinuxAbi::Musl).unwrap(),
            "aarch64-unknown-linux-musl"
        );
        assert!(linux_target_triple_for("arm", LinuxAbi::Musl).is_err());
    }
}
