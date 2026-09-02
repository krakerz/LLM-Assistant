use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::process::Command;

fn main() {
    // No `cargo:rerun-if-changed` directives on purpose -- omitting them
    // entirely is what tells Cargo to always re-run this script, which is
    // exactly what's needed here: the hash below must reflect the working
    // tree's actual state *at build time*, uncommitted changes included, not
    // just whenever a tracked file's git-index entry changes.
    println!("cargo:rustc-env=BUILD_HASH={}", build_hash());

    tauri_build::build()
}

/// A short, content-addressed identifier for this specific build -- distinct
/// from `CARGO_PKG_VERSION` (which only changes on a deliberate version
/// bump) and shown alongside it in the Settings UI and the startup log, so
/// "which exact build is this app actually running" never has to be
/// guessed from a stale binary's mtime again. Not a security hash, just
/// short and stable: the git commit short SHA when the working tree is
/// clean, or that SHA plus a hash of the actual uncommitted diff (tracked
/// changes and untracked file contents both) when it isn't -- so two
/// in-progress builds only ever share a hash if their real content is
/// identical, the same property a real git hash has.
fn build_hash() -> String {
    let commit = run_git(&["rev-parse", "--short=8", "HEAD"]).unwrap_or_else(|| "nogit".into());

    let diff = run_git(&["diff", "HEAD"]).unwrap_or_default();
    let untracked_paths =
        run_git(&["ls-files", "--others", "--exclude-standard"]).unwrap_or_default();
    let untracked_content: String = untracked_paths
        .lines()
        .map(|p| std::fs::read_to_string(p).unwrap_or_default())
        .collect();

    if diff.is_empty() && untracked_content.is_empty() {
        return commit;
    }

    let mut hasher = DefaultHasher::new();
    diff.hash(&mut hasher);
    untracked_content.hash(&mut hasher);
    format!("{commit}-dirty-{:08x}", hasher.finish() as u32)
}

fn run_git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
