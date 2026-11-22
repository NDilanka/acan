//! Command-line entry for `acan-hook`. Droid runs it with the hook JSON on stdin.
//!
//! Both exit paths write to stderr with `writeln!` and ignore the result. `eprintln!` panics
//! when stderr is closed, and a panic must still end with exit code 2.

use std::io::Write;
use std::time::Instant;

fn main() {
    // Taken first, so the deadline counts from process entry.
    let started = Instant::now();
    std::panic::set_hook(Box::new(|info| {
        let _ = writeln!(std::io::stderr(), "acan hook: internal error: {info}");
        std::process::exit(acan_hook::panic_exit_code());
    }));
    if let Err(err) = acan_hook::run_from_process(started) {
        let _ = writeln!(std::io::stderr(), "acan hook: {err}");
        std::process::exit(acan_hook::BLOCK_EXIT_CODE);
    }
}
