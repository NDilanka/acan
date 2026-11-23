//! Command-line entry for `acan-hook`. Droid runs it with the hook JSON on stdin.
//!
//! Both exit paths go through [`acan_hook::report`]. It writes stderr on a helper thread and
//! waits only briefly, so a stalled stderr cannot hold the exit, and a panic still ends with
//! exit code 2.

use std::time::Instant;

fn main() {
    // Taken first, so the deadline counts from process entry.
    let started = Instant::now();
    std::panic::set_hook(Box::new(|info| {
        acan_hook::report(&format!("acan hook: internal error: {info}"));
        std::process::exit(acan_hook::panic_exit_code());
    }));
    if let Err(err) = acan_hook::run_from_process(started) {
        acan_hook::report(&format!("acan hook: {err}"));
        std::process::exit(acan_hook::BLOCK_EXIT_CODE);
    }
}
