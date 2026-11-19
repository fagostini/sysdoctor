//! Embed the git commit in the version string: "0.1.1 (50bd183)", or plain "0.1.1" outside a git checkout.
use std::process::Command;

fn main() {
    let git = |args: &[&str]| {
        let out = Command::new("git").args(args).output().ok().filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let version = env!("CARGO_PKG_VERSION");
    let full = match git(&["rev-parse", "--short=7", "HEAD"]) {
        Some(hash) => format!("{version} ({hash})"),
        None => version.to_string(),
    };
    println!("cargo:rustc-env=SYSDOCTOR_VERSION={full}");
    // rebuild when HEAD moves: a checkout changes HEAD, a commit changes the branch ref
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={dir}/{r}");
        }
        println!("cargo:rerun-if-changed={dir}/packed-refs");
    }
}
