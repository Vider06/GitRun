use gitrun_core::{Config, Runner};
use gitrun_setup::prepare_directories;

fn help() {
    println!("GitRun Rust control plane");
    println!("Usage: gitrun-rs <version|config|desired|doctor|setup|help>");
}

fn parse_u32_arg(args: &[String], index: usize, name: &str) -> Result<u32, String> {
    args.get(index)
        .ok_or_else(|| format!("missing {name}"))
        .and_then(|value| value.parse::<u32>().map_err(|_| format!("invalid {name}: {value}")))
}

fn load_config() -> Result<Config, gitrun_core::ConfigError> {
    if let Ok(path) = std::env::var("GITRUN_CONFIG_FILE") {
        return Config::from_env_file(path);
    }
    Config::from_env()
}

fn setup_config_dir() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("GITRUN_CONFIG_DIR") {
        return path.into();
    }
    if let Ok(path) = std::env::var("GITRUN_CONFIG_FILE") {
        if let Some(parent) = std::path::Path::new(&path).parent() {
            return parent.to_path_buf();
        }
    }
    "config".into()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("help") {
        "version" if args.len() == 1 => println!("GitRun Rust core 0.2.0"),
        "config" if args.len() == 1 => match load_config() {
            Ok(config) => println!("{}", serde_json::to_string_pretty(&config).unwrap()),
            Err(error) => {
                eprintln!("configuration error: {error}");
                std::process::exit(2);
            }
        },
        "desired" if args.len() == 5 => {
            let values = [
                parse_u32_arg(&args, 1, "min"),
                parse_u32_arg(&args, 2, "max"),
                parse_u32_arg(&args, 3, "busy"),
                parse_u32_arg(&args, 4, "queued"),
            ];
            if let Some(error) = values.iter().find_map(|v| v.as_ref().err()) {
                eprintln!("{error}");
                std::process::exit(2);
            }
            println!(
                "{}",
                Runner::desired_count(
                    *values[0].as_ref().unwrap(),
                    *values[1].as_ref().unwrap(),
                    *values[2].as_ref().unwrap(),
                    *values[3].as_ref().unwrap(),
                )
            );
        }
        "setup" if args.len() == 1 => match load_config() {
            Ok(config) => {
                let config_dir = setup_config_dir();
                match prepare_directories(&config, config_dir) {
                    Ok(report) => {
                        let failed = report
                            .dependencies
                            .iter()
                            .filter(|dependency| !dependency.available)
                            .count();

                        for dependency in &report.dependencies {
                            let state = if dependency.available {
                                "available"
                            } else {
                                "missing"
                            };
                            println!("{}: {state}", dependency.name);
                        }
                        println!("config: {}", report.config_dir.display());
                        println!("state: {}", report.state_dir.display());
                        println!("logs: {}", report.log_dir.display());

                        if failed != 0 {
                            eprintln!("GitRun setup: FAIL — {failed} dependency check(s) failed");
                            std::process::exit(1);
                        }
                        println!("GitRun setup: PASS");
                    }
                    Err(error) => {
                        eprintln!("GitRun setup: FAIL — {error}");
                        std::process::exit(1);
                    }
                }
            }
            Err(error) => {
                eprintln!("GitRun setup: FAIL — {error}");
                std::process::exit(1);
            }
        },
        "doctor" if args.len() == 1 => match load_config() {
            Ok(config) => println!(
                "GitRun doctor: PASS ({} repositories, pool {}..{})",
                config.repositories.len(),
                config.min_runners,
                config.max_runners
            ),
            Err(error) => {
                eprintln!("GitRun doctor: FAIL — {error}");
                std::process::exit(1);
            }
        },
        "help" if args.len() == 1 => help(),
        _ => {
            help();
            std::process::exit(2);
        }
    }
}
