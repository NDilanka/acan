//! Stamps the commit this build came from into `ACAN_COMMIT`, which `release::commit` reads. The
//! release workflow sets the variable itself, so this only does something for a local build made
//! in a git checkout. Without git, the build keeps `unknown`.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn main() {
    println!("cargo:rerun-if-env-changed=ACAN_COMMIT");
    if std::env::var_os("ACAN_COMMIT").is_some() {
        return;
    }
    // The reflog moves with every commit and checkout, so a new commit rebuilds this crate.
    if let Some(log) = git(&["rev-parse", "--git-path", "logs/HEAD"]) {
        println!("cargo:rerun-if-changed={log}");
    }
    if let Some(sha) = git(&["rev-parse", "HEAD"]) {
        println!("cargo:rustc-env=ACAN_COMMIT={sha}");
    }
}
