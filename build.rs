use std::process::Command;

fn main() {
    // No `cargo:rerun-if-changed` directive on purpose -- omitting it
    // entirely is what tells Cargo to always re-run this script, needed so
    // a commit made since the last build is picked up without a `touch`.
    println!("cargo:rustc-env=BUILD_HASH={}", build_hash());

    tauri_build::build()
}

/// A short, readable identifier for roughly which commit this build is
/// from -- distinct from `CARGO_PKG_VERSION` (which only changes on a
/// deliberate version bump) and shown alongside it in the Settings UI and
/// the startup log. Just the git commit's own short SHA -- previously also
/// hashed the uncommitted diff and appended it as `{commit}-dirty-{hash}`
/// to distinguish two different in-progress builds off the same commit,
/// but that read as confusing double-hash noise for what's meant to be a
/// quick glance, not exact content-addressing; a dev build is close enough
/// to "which commit" without it.
fn build_hash() -> String {
    run_git(&["rev-parse", "--short=8", "HEAD"]).unwrap_or_else(|| "nogit".into())
}

fn run_git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
