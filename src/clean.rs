use crate::finding::{human, parse_size, run, sh_quote, Finding, Fix, Severity};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

pub const SCANNERS: &[fn(&Path) -> Vec<Finding>] =
    &[projects, pkg_caches, ai_caches, stale_caches, user_trash, pkg_cache, old_kernels, containers, conda, journal, snap, flatpak];

const MIN: u64 = 50 << 20; // ignore anything smaller than 50M


/// Real disk usage: st_blocks (handles sparse), hardlinks once, no symlink follow, no mount crossing.
pub fn du(root: &Path) -> u64 {
    du_newest(root).0
}

/// (disk usage, newest mtime in Unix seconds) of a tree.
fn du_newest(root: &Path) -> (u64, i64) {
    let Ok(meta) = fs::symlink_metadata(root) else { return (0, 0) };
    let dev = meta.dev();
    let mut seen = HashSet::new();
    let mut total = 0;
    let mut newest = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(m) = fs::symlink_metadata(&p) else { continue };
        if m.dev() != dev || (m.nlink() > 1 && !m.is_dir() && !seen.insert((m.dev(), m.ino()))) {
            continue;
        }
        total += m.blocks() * 512;
        newest = newest.max(m.mtime());
        if m.is_dir() {
            stack.extend(fs::read_dir(&p).into_iter().flatten().flatten().map(|e| e.path()));
        }
    }
    (total, newest)
}

/// Measure several trees in parallel (they are usually on one SSD, so threads help).
fn du_many(paths: &[PathBuf]) -> Vec<(u64, i64)> {
    std::thread::scope(|s| {
        let hs: Vec<_> = paths.iter().map(|p| s.spawn(move || du_newest(p))).collect();
        hs.into_iter().map(|h| h.join().unwrap_or((0, 0))).collect()
    })
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

fn is_stale(project: &Path, artifact: &str, now: SystemTime) -> bool {
    last_touched(project, artifact)
        .is_some_and(|t| now.duration_since(t).unwrap_or_default() > Duration::from_secs(crate::config::get().stale_days * 86400))
}

/// Uncommitted changes. The repo's own config is untrusted (any extracted tarball can carry a .git):
/// core.fsmonitor, hooks and filter drivers would run its commands. fsmonitor and hooks are
/// overridden; filters can't be switched off wholesale, so a repo defining one counts as dirty
/// and is never offered. Reading config runs nothing.
fn git_dirty(dir: &Path) -> bool {
    let d = dir.to_string_lossy();
    let keys = run("git", &["-C", &d, "config", "--local", "--includes", "--name-only", "--list"]).unwrap_or_default();
    if keys.lines().any(|k| k.starts_with("filter.") || k == "extensions.worktreeconfig") {
        return true;
    }
    let args = ["-C", &d, "-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "--no-optional-locks",
        "status", "--porcelain", "--ignore-submodules=all"];
    run("git", &args).is_some_and(|s| !s.trim().is_empty())
}

/// Find stale build artifacts under home: (artifact dir, marker file that proves it's a project).
fn find_artifacts(home: &Path, now: SystemTime) -> Vec<(&'static str, PathBuf)> {
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
        let id = if kind == "target" { "rust-target" } else { "node-modules" };
        let mut f = Finding::ok(id, format!("{label} in {} projects untouched >30d (regenerable)", paths.len()))
            .detail(detail)
            .bytes(sizes.iter().sum());
        f.fix = Some(Fix::Trash(paths));
        out.push(f);
    }
    out
}

/// Package manager download caches: always regenerable, so --apply may trash them.
const PKG_CACHES: &[(&str, &str)] = &[
    (".cache/pip", "pip cache"),
    (".cargo/registry/cache", "cargo crate downloads"),
    (".cargo/registry/src", "cargo unpacked crate sources"),
    (".npm/_cacache", "npm cache"),
    (".cache/yarn", "yarn cache"),
    (".local/share/pnpm/store", "pnpm store"),
    (".cache/go-build", "Go build cache"),
    (".gradle/caches", "Gradle cache"),
    (".m2/repository", "Maven repository"),
    (".cache/ms-playwright", "Playwright browsers"),
    (".cache/thumbnails", "thumbnail cache"),
];

fn pkg_caches(home: &Path) -> Vec<Finding> {
    let paths: Vec<PathBuf> = PKG_CACHES.iter().map(|(r, _)| home.join(r)).collect();
    let sizes = du_many(&paths);
    PKG_CACHES
        .iter()
        .zip(paths)
        .zip(sizes)
        .filter(|(_, (b, _))| *b >= MIN)
        .map(|(((rel, label), p), (b, _))| Finding {
            id: "pkg-cache",
            severity: Severity::Ok,
            title: format!("{label} ~/{rel} (regenerable)"),
            detail: String::new(),
            bytes: Some(b),
            fix: Some(Fix::Trash(vec![p])),
        })
        .collect()
}

/// ~/.cache subfolders nothing has written to in `cache_stale_days`. Review items: apps sometimes keep state there.
fn stale_caches(home: &Path) -> Vec<Finding> {
    let cache = home.join(".cache");
    let covered = ["pip", "yarn", "go-build", "thumbnails", "huggingface", "torch", "ms-playwright"];
    let paths: Vec<PathBuf> = fs::read_dir(&cache)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && !covered.iter().any(|c| e.file_name() == *c))
        .map(|e| e.path())
        .collect();
    let days = crate::config::get().cache_stale_days;
    let cutoff = SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) - days as i64 * 86400;
    let mut stale: Vec<(PathBuf, u64)> =
        paths.iter().zip(du_many(&paths)).filter(|(_, (b, newest))| *b >= MIN && *newest < cutoff).map(|(p, (b, _))| (p.clone(), b)).collect();
    if stale.is_empty() {
        return vec![];
    }
    stale.sort_by_key(|(_, b)| std::cmp::Reverse(*b));
    let total = stale.iter().map(|(_, b)| b).sum();
    let detail = stale.iter().map(|(p, b)| format!("{:>8}  {}", human(*b), p.display())).collect::<Vec<_>>().join("\n");
    let cmd = format!("gio trash {}", stale.iter().map(|(p, _)| sh_quote(&p.to_string_lossy())).collect::<Vec<_>>().join(" "));
    vec![Finding::new("stale-cache", Severity::Warn, format!("{} ~/.cache folders unused for {days}+ days (review)", stale.len()))
        .detail(detail)
        .bytes(total)
        .cmd(cmd)]
}

fn pkg_cache(_: &Path) -> Vec<Finding> {
    let Some((dir, cmd)) = crate::pkg::cache() else { return vec![] };
    let b = du(Path::new(dir));
    if b < MIN {
        return vec![];
    }
    let mut f = Finding::new("pkg-downloads", Severity::Ok, format!("Downloaded packages in {dir}")).cmd(cmd);
    f.bytes = Some(b);
    vec![f]
}

/// Installed packages built for a kernel version, from `dpkg-query -W -f='${db:Status-Abbrev}${Package}\n'` output.
fn kernel_packages<'a>(dpkg: &'a str, version: &str) -> Vec<&'a str> {
    let base = version.trim_end_matches("-generic").trim_end_matches("-lowlatency");
    dpkg.lines()
        .filter_map(|l| l.strip_prefix("ii "))
        .map(str::trim)
        .filter(|p| p.starts_with("linux-") && (p.ends_with(&format!("-{base}")) || p.contains(&format!("-{base}-"))))
        .collect()
}

/// Kernels to remove from a version-sorted list: everything except the newest two and the running one.
/// Two, so a bad new kernel always leaves a known-good fallback in the boot menu.
fn removable_kernels(sorted: &[String], running: &str) -> Vec<String> {
    let keep_from = sorted.len().saturating_sub(2);
    sorted[..keep_from].iter().filter(|k| *k != running).cloned().collect()
}

fn old_kernels(_: &Path) -> Vec<Finding> {
    let running = run("uname", &["-r"]).unwrap_or_default().trim().to_string();
    let old = removable_kernels(&crate::security::installed_kernels(), &running);
    if old.is_empty() {
        return vec![];
    }
    use crate::pkg::Pm;
    let cmd = match crate::pkg::detect() {
        Some(Pm::Apt) => {
            let Some(dpkg) = run("dpkg-query", &["-W", "-f=${db:Status-Abbrev}${Package}\n", "linux-*"]) else { return vec![] };
            let pkgs: Vec<&str> = old.iter().flat_map(|k| kernel_packages(&dpkg, k)).collect();
            if pkgs.is_empty() {
                return vec![];
            }
            format!("sudo apt purge {}", pkgs.join(" "))
        }
        Some(Pm::Dnf) => "sudo dnf remove --oldinstallonly --setopt installonly_limit=2 kernel".into(),
        Some(Pm::Zypper) => "sudo zypper purge-kernels".into(),
        // Arch keeps one kernel per package (vmlinuz-linux has no version): nothing to remove
        Some(Pm::Pacman) | None => return vec![],
    };
    let bytes: u64 = old
        .iter()
        .map(|k| {
            let boot: u64 = fs::read_dir("/boot")
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(k.as_str()))
                .map(|e| du(&e.path()))
                .sum();
            boot + du(&Path::new("/lib/modules").join(k))
        })
        .sum();
    let mut f = Finding::new("old-kernels", Severity::Ok, format!("{} old kernels (keeping the newest two and running {running})", old.len()))
        .detail(old.join(", "))
        .cmd(cmd);
    f.bytes = Some(bytes);
    vec![f]
}

fn ai_caches(home: &Path) -> Vec<Finding> {
    [".cache/huggingface", ".ollama/models", ".cache/torch"]
        .iter()
        .map(|r| home.join(r))
        .filter(|p| p.exists())
        .map(|p| (du(&p), p))
        .filter(|(b, _)| *b > 100 << 20)
        .map(|(b, p)| {
            Finding::new("ai-cache", Severity::Warn, format!("AI model cache {} (review: re-download is slow)", p.display()))
                .bytes(b)
                .cmd(format!("gio trash {}", sh_quote(&p.to_string_lossy())))
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

/// Docker and Podman: same `system df` template and prune command.
fn containers(_: &Path) -> Vec<Finding> {
    [("docker", "Docker"), ("podman", "Podman")]
        .into_iter()
        .filter_map(|(bin, label)| {
            let s = run(bin, &["system", "df", "--format", "{{.Type}}\t{{.Reclaimable}}"])?;
            // Reclaimable looks like "1.2GB (30%)"
            let b: u64 = s.lines().filter_map(|l| parse_size(l.split('\t').nth(1)?.split_whitespace().next()?)).sum();
            let mut f = Finding::new(bin, Severity::Ok, format!("{label} unused images/containers/cache")).cmd(format!("{bin} system prune"));
            f.bytes = Some(b);
            (b > 0).then_some(f)
        })
        .collect()
}

/// Bytes `conda clean --all` would free, from its --dry-run --json output.
fn conda_reclaimable(json: &str) -> u64 {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    ["packages", "tarballs"].iter().filter_map(|k| v[k]["total_size"].as_u64()).sum()
}

/// conda's package cache: ask conda itself, since envs hardlink into pkgs/ and only unused packages free space.
fn conda(home: &Path) -> Vec<Finding> {
    let candidates = ["miniconda3", "anaconda3", "miniforge3", "mambaforge"].map(|d| home.join(d).join("bin/conda"));
    let Some(exe) = std::env::var_os("CONDA_EXE").map(PathBuf::from).into_iter().chain(candidates).find(|p| p.is_file()) else { return vec![] };
    let Some(j) = run(&exe.to_string_lossy(), &["clean", "--all", "--dry-run", "--json"]) else { return vec![] };
    let b = conda_reclaimable(&j);
    if b < MIN {
        return vec![];
    }
    let mut f = Finding::new("conda", Severity::Ok, "conda unused packages and tarballs").cmd(format!("{} clean --all -y", sh_quote(&exe.to_string_lossy())));
    f.bytes = Some(b);
    vec![f]
}

fn journal(_: &Path) -> Vec<Finding> {
    // "Archived and active journals take up 1.0G in the file system."
    let Some(s) = run("journalctl", &["--disk-usage"]) else { return vec![] };
    let Some(b) = s.split("take up ").nth(1).and_then(|r| parse_size(r.split_whitespace().next()?)) else { return vec![] };
    let keep = crate::config::get().journal_max_mb << 20;
    if b <= keep {
        return vec![];
    }
    let mb = crate::config::get().journal_max_mb;
    let mut f = Finding::new("journal", Severity::Ok, format!("systemd journal over {mb}M")).cmd(format!("sudo journalctl --vacuum-size={mb}M"));
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

    /// A hostile repo (as from an extracted tarball) must not get to run its commands.
    #[test]
    fn git_dirty_runs_no_repo_commands() {
        let d = tempfile::tempdir().unwrap();
        let (repo, pwned) = (d.path().join("r"), d.path().join("PWNED"));
        let git = |a: &[&str]| assert!(Command::new("git").arg("-C").arg(&repo).args(a).status().unwrap().success());
        fs::create_dir(&repo).unwrap();
        fs::write(repo.join("a.txt"), "x").unwrap();
        git(&["init", "-q"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "add", "a.txt"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "c"]);
        let touch = format!("touch {}", pwned.display());
        git(&["config", "core.fsmonitor", &format!("{touch}; false")]);
        fs::write(repo.join(".git/hooks/post-index-change"), format!("#!/bin/sh\n{touch}\n")).unwrap();
        fs::set_permissions(repo.join(".git/hooks/post-index-change"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        fs::write(repo.join("a.txt"), "y").unwrap(); // stale stat: status would re-hash and refresh the index
        assert!(git_dirty(&repo));
        assert!(!pwned.exists(), "fsmonitor or hook ran");
        // fresh repo for the filter case: this test's own git calls would trip the fsmonitor above
        fs::remove_dir_all(&repo).unwrap();
        fs::create_dir(&repo).unwrap();
        git(&["init", "-q"]);
        git(&["config", "filter.x.clean", &touch]);
        fs::write(repo.join(".gitattributes"), "* filter=x\n").unwrap();
        assert!(git_dirty(&repo)); // filter driver: treated as dirty without running git status
        assert!(!pwned.exists(), "filter ran");
    }

    #[test]
    fn conda_dry_run() {
        let j = r#"{"index_cache": null, "logfiles": [], "packages": {"pkg_sizes": {}, "total_size": 898012430}, "success": true, "tarballs": {"total_size": 68347180}}"#;
        assert_eq!(conda_reclaimable(j), 966359610);
        assert_eq!(conda_reclaimable("not json"), 0);
    }
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
    fn kernel_keep_policy() {
        let k = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let all = k(&["6.8.0-38", "6.8.0-100", "6.8.0-124", "6.8.0-149"]);
        assert_eq!(removable_kernels(&all, "6.8.0-149"), k(&["6.8.0-38", "6.8.0-100"]));
        assert_eq!(removable_kernels(&all, "6.8.0-38"), k(&["6.8.0-100"])); // booted into an old one: keep it
        assert!(removable_kernels(&k(&["6.8.0-149"]), "6.8.0-149").is_empty());
    }

    #[test]
    fn kernel_pkg_match() {
        let dpkg = "ii linux-image-6.8.0-100-generic\nii linux-modules-6.8.0-100-generic\nii linux-headers-6.8.0-100\n\
                    ii linux-headers-6.8.0-100-generic\nii linux-image-6.8.0-1000-generic\nrc linux-image-6.8.0-99-generic\nii linux-firmware\n";
        assert_eq!(
            kernel_packages(dpkg, "6.8.0-100-generic"),
            ["linux-image-6.8.0-100-generic", "linux-modules-6.8.0-100-generic", "linux-headers-6.8.0-100", "linux-headers-6.8.0-100-generic"]
        );
        assert!(kernel_packages(dpkg, "6.8.0-99-generic").is_empty()); // only config files left
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
