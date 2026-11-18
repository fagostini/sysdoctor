use crate::finding::{human, parse_size, run, Finding, Fix, Severity};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

pub const SCANNERS: &[fn(&Path) -> Vec<Finding>] = &[projects, ai_caches, user_trash, docker, journal, snap, flatpak];

const STALE: Duration = Duration::from_secs(30 * 86400);

/// Real disk usage: st_blocks (handles sparse), hardlinks once, no symlink follow, no mount crossing.
pub fn du(root: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(root) else { return 0 };
    let dev = meta.dev();
    let mut seen = HashSet::new();
    let mut total = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(m) = fs::symlink_metadata(&p) else { continue };
        if m.dev() != dev || (m.nlink() > 1 && !m.is_dir() && !seen.insert((m.dev(), m.ino()))) {
            continue;
        }
        total += m.blocks() * 512;
        if m.is_dir() {
            stack.extend(fs::read_dir(&p).into_iter().flatten().flatten().map(|e| e.path()));
        }
    }
    total
}

/// Newest mtime of a project's direct children, ignoring the artifact dir and .git.
fn last_touched(project: &Path, artifact: &str) -> Option<SystemTime> {
    fs::read_dir(project)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            n != artifact && n != ".git"
        })
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .max()
}

pub fn is_stale(project: &Path, artifact: &str, now: SystemTime) -> bool {
    last_touched(project, artifact)
        .is_some_and(|t| now.duration_since(t).unwrap_or_default() > STALE)
}

fn git_dirty(dir: &Path) -> bool {
    run("git", &["-C", &dir.to_string_lossy(), "status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty())
}

/// Find stale build artifacts under home: (artifact dir, marker file that proves it's a project).
pub fn find_artifacts(home: &Path, now: SystemTime) -> Vec<(&'static str, PathBuf)> {
    const KINDS: &[(&str, &str)] = &[("target", "Cargo.toml"), ("node_modules", "package.json")];
    let dev = fs::symlink_metadata(home).map(|m| m.dev()).unwrap_or(0);
    let mut out = vec![];
    let mut stack = vec![home.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            // ponytail: hidden dirs skipped (caches, toolchains, Trash); projects rarely live there.
            if name.starts_with('.') {
                continue;
            }
            if let Some((kind, marker)) = KINDS.iter().find(|(k, _)| *k == name) {
                if dir.join(marker).exists() && is_stale(&dir, kind, now) && !git_dirty(&dir) {
                    out.push((*kind, e.path()));
                }
                continue; // never descend into artifacts
            }
            if e.metadata().is_ok_and(|m| m.dev() == dev) {
                stack.push(e.path());
            }
        }
    }
    out
}

fn projects(home: &Path) -> Vec<Finding> {
    let found = find_artifacts(home, SystemTime::now());
    let mut out = vec![];
    for (kind, label) in [("target", "Rust target/ dirs"), ("node_modules", "node_modules dirs")] {
        let paths: Vec<PathBuf> = found.iter().filter(|(k, _)| *k == kind).map(|(_, p)| p.clone()).collect();
        if paths.is_empty() {
            continue;
        }
        let sizes: Vec<u64> = paths.iter().map(|p| du(p)).collect();
        let detail = paths.iter().zip(&sizes).map(|(p, s)| format!("{:>8}  {}", human(*s), p.display())).collect::<Vec<_>>().join("\n");
        out.push(Finding {
            id: if kind == "target" { "rust-target" } else { "node-modules" },
            severity: Severity::Ok,
            title: format!("{label} in {} projects untouched >30d (regenerable)", paths.len()),
            detail,
            bytes: Some(sizes.iter().sum()),
            fix: Some(Fix::Trash(paths)),
        });
    }
    out
}

fn ai_caches(home: &Path) -> Vec<Finding> {
    [".cache/huggingface", ".ollama/models", ".cache/torch"]
        .iter()
        .map(|r| home.join(r))
        .filter(|p| p.exists())
        .map(|p| (du(&p), p))
        .filter(|(b, _)| *b > 100 << 20)
        .map(|(b, p)| Finding {
            id: "ai-cache",
            severity: Severity::Warn,
            title: format!("AI model cache {} (review: re-download is slow)", p.display()),
            detail: String::new(),
            bytes: Some(b),
            fix: Some(Fix::Command(format!("gio trash '{}'", p.display()))),
        })
        .collect()
}

fn user_trash(home: &Path) -> Vec<Finding> {
    let b = du(&home.join(".local/share/Trash"));
    if b < 10 << 20 {
        return vec![];
    }
    let mut f = Finding::new("trash", Severity::Ok, "Trash").cmd("gio trash --empty");
    f.bytes = Some(b);
    vec![f]
}

fn docker(_: &Path) -> Vec<Finding> {
    let Some(s) = run("docker", &["system", "df", "--format", "{{.Type}}\t{{.Reclaimable}}"]) else { return vec![] };
    // Reclaimable looks like "1.2GB (30%)"
    let b: u64 = s.lines().filter_map(|l| parse_size(l.split('\t').nth(1)?.split_whitespace().next()?)).sum();
    if b == 0 {
        return vec![];
    }
    let mut f = Finding::new("docker", Severity::Ok, "Docker unused images/containers/cache").cmd("docker system prune");
    f.bytes = Some(b);
    vec![f]
}

fn journal(_: &Path) -> Vec<Finding> {
    // "Archived and active journals take up 1.0G in the file system."
    let Some(s) = run("journalctl", &["--disk-usage"]) else { return vec![] };
    let Some(b) = s.split("take up ").nth(1).and_then(|r| parse_size(r.split_whitespace().next()?)) else { return vec![] };
    let keep = 500u64 << 20;
    if b <= keep {
        return vec![];
    }
    let mut f = Finding::new("journal", Severity::Ok, "systemd journal over 500M").cmd("sudo journalctl --vacuum-size=500M");
    f.bytes = Some(b - keep);
    vec![f]
}

fn snap(_: &Path) -> Vec<Finding> {
    let Some(s) = run("snap", &["list", "--all"]) else { return vec![] };
    let revs: Vec<(String, String)> = s
        .lines()
        .skip(1)
        .filter(|l| l.contains("disabled"))
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            Some((f.first()?.to_string(), f.get(2)?.to_string()))
        })
        .collect();
    if revs.is_empty() {
        return vec![];
    }
    let b = revs
        .iter()
        .filter_map(|(n, r)| fs::metadata(format!("/var/lib/snapd/snaps/{n}_{r}.snap")).ok())
        .map(|m| m.len())
        .sum();
    let cmd = revs.iter().map(|(n, r)| format!("sudo snap remove {n} --revision {r}")).collect::<Vec<_>>().join(" && ");
    let mut f = Finding::new("snap", Severity::Ok, format!("{} disabled snap revisions", revs.len())).cmd(cmd);
    f.bytes = Some(b);
    vec![f]
}

fn flatpak(_: &Path) -> Vec<Finding> {
    // No dry-run flag exists; answer "n" to the prompt and count the listed refs.
    let Ok(mut child) = Command::new("flatpak")
        .args(["uninstall", "--unused"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return vec![];
    };
    let _ = child.stdin.take().map(|mut i| i.write_all(b"n\n"));
    let Ok(out) = child.wait_with_output() else { return vec![] };
    let s = String::from_utf8_lossy(&out.stdout);
    // rows look like " 1.     org.gnome.Platform    44    r"
    let n = s.lines().filter(|l| l.trim_start().split('.').next().is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))).count();
    if n == 0 {
        return vec![];
    }
    vec![Finding::new("flatpak", Severity::Ok, format!("{n} unused flatpak runtimes"))
        .detail("Size not reported by flatpak")
        .cmd("flatpak uninstall --unused")]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn set_old(p: &Path) {
        assert!(Command::new("touch").args(["-d", "60 days ago"]).arg(p).status().unwrap().success());
    }

    #[test]
    fn du_counts_hardlinks_once_and_skips_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        fs::write(&a, vec![1u8; 64 * 1024]).unwrap();
        let one = du(&a);
        assert!(one >= 64 * 1024);
        fs::hard_link(&a, d.path().join("b")).unwrap();
        std::os::unix::fs::symlink("/usr", d.path().join("link")).unwrap();
        let dir_only = fs::symlink_metadata(d.path()).unwrap().blocks() * 512;
        let link = fs::symlink_metadata(d.path().join("link")).unwrap().blocks() * 512;
        assert_eq!(du(d.path()), dir_only + one + link);
    }

    #[test]
    fn stale_detection() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        // stale rust project
        let old = home.join("old");
        fs::create_dir_all(old.join("target/debug")).unwrap();
        fs::write(old.join("Cargo.toml"), "").unwrap();
        set_old(&old.join("Cargo.toml"));
        // fresh rust project
        let new = home.join("new");
        fs::create_dir_all(new.join("target")).unwrap();
        fs::write(new.join("Cargo.toml"), "").unwrap();
        // target without marker: not a project
        let other = home.join("other");
        fs::create_dir_all(other.join("target")).unwrap();
        fs::write(other.join("x"), "").unwrap();
        set_old(&other.join("x"));
        // nested node_modules inside stale node project must not be reported separately
        let js = home.join("js");
        fs::create_dir_all(js.join("node_modules/dep/node_modules")).unwrap();
        fs::write(js.join("node_modules/dep/package.json"), "").unwrap();
        fs::write(js.join("package.json"), "").unwrap();
        set_old(&js.join("package.json"));

        let mut got: Vec<_> = find_artifacts(home, SystemTime::now())
            .into_iter()
            .map(|(k, p)| (k, p.strip_prefix(home).unwrap().to_path_buf()))
            .collect();
        got.sort();
        assert_eq!(got, vec![("node_modules", PathBuf::from("js/node_modules")), ("target", PathBuf::from("old/target"))]);
    }
}
