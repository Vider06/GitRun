use gitrun_core::{Config, Runner};

fn help() {
    println!("GitRun Rust control plane");
    println!("Usage: gitrun-rs <version|config|desired|doctor|help>");
}

fn parse_u32_arg(args: &[String], index: usize, name: &str) -> Result<u32, String> {
    args.get(index)
        .ok_or_else(|| format!("missing {name}"))
        .and_then(|value| value.parse::<u32>().map_err(|_| format!("invalid {name}: {value}")))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("help") {
        "version" if args.len() == 1 => println!("GitRun Rust core 0.2.0"),
        "config" if args.len() == 1 => match Config::from_env() {
            Ok(config) => println!("{}", serde_json::to_string_pretty(&config).unwrap()),
            Err(error) => { eprintln!("configuration error: {error}"); std::process::exit(2); }
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
            println!("{}", Runner::desired_count(
                values[0].as_ref().unwrap().to_owned(),
                values[1].as_ref().unwrap().to_owned(),
                values[2].as_ref().unwrap().to_owned(),
                values[3].as_ref().unwrap().to_owned(),
            ));
        },
        "doctor" if args.len() == 1 => match Config::from_env() {
            Ok(config) => println!("GitRun doctor: PASS ({} repositories, pool {}..{})", config.repositories.len(), config.min_runners, config.max_runners),
            Err(error) => { eprintln!("GitRun doctor: FAIL — {error}"); std::process::exit(1); }
        },
        "help" if args.len() == 1 => help(),
        _ => { help(); std::process::exit(2); }
    }
}
