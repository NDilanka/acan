//! Command-line entry for `acan-hook`. Droid runs it with the hook JSON on stdin.

fn main() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("acan hook: internal error: {info}");
        std::process::exit(acan_hook::panic_exit_code());
    }));
    if let Err(err) = acan_hook::run_from_process() {
        eprintln!("acan hook: {err}");
        std::process::exit(acan_hook::BLOCK_EXIT_CODE);
    }
}
