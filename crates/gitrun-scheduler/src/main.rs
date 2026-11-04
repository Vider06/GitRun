fn main() {
    if let Err(reason) = gitrun_platform::ensure_supported_runtime() {
        eprintln!("gitrun-autoscaler: {reason}");
        std::process::exit(2);
    }
    gitrun_scheduler::run();
}
