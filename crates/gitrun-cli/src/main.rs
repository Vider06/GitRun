use gitrun_core::{Config, Runner};

fn help() {
    println!("GitRun Rust control plane");
    println!("Usage: gitrun-rs <version|config|desired|doctor|help>");
}

fn main() {
    let command = std::env::args().nth(1).unwrap_or_else(|| "help".into());
    match command.as_str() {
        "version" => println!("GitRun Rust core 0.2.0"),
        "config" => match Config::from_env() {
            Ok(config) => println!("{}", serde_json::to_string_pretty(&config).unwrap()),
            Err(error) => { eprintln!("configuration error: {error}"); std::process::exit(2); }
        },
        "desired" => {
            let args: Vec<u32> = std::env::args().skip(2).filter_map(|v| v.parse().ok()).collect();
            if args.len() != 4 { eprintln!("Usage: gitrun-rs desired <min> <max> <busy> <queued>"); std::process::exit(2); }
            println!("{}", Runner::desired_count(args[0], args[1], args[2], args[3]));
        },
        "doctor" => match Config::from_env() {
            Ok(config) => println!("GitRun doctor: PASS ({} repositories, pool {}..{})", config.repositories.len(), config.min_runners, config.max_runners),
            Err(error) => { eprintln!("GitRun doctor: FAIL — {error}"); std::process::exit(1); }
        },
        _ => help(),
    }
}
