use crate::finding::{human, run, run_any, xdg, Finding, Severity};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const CHECKS: &[fn() -> Vec<Finding>] = &[
    disks, smart, failed_units, reboot_required, crate::security::kernel, crate::security::updates, oom,
    crate::health::memory_pressure, ntp, firewall, ssh_password, crate::security::permissions,
    crate::security::history_secrets, crate::security::expiries, crate::health::backup, battery,
    crate::health::throttling, crate::health::firmware, crate::health::journal_digest, plugins,
];

/// User checks in $XDG_CONFIG_HOME/sysdoctor/checks.d, Nagios-style:
/// exit 0 ok / 1 warn / 2 crit / other = skipped; stdout line 1 = title, rest = detail.
fn plugins() -> Vec<Finding> {
    let Some(dir) = xdg("XDG_CONFIG_HOME", ".config").map(|d| d.join("sysdoctor/checks.d")) else { return vec![] };
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.metadata().is_ok_and(|m| {
                let mode = m.permissions().mode();
                // executable, and not group/world-writable (someone else could inject code)
                m.is_file() && mode & 0o111 != 0 && mode & 0o022 == 0
            })
        })
        .map(|e| e.path())
        .collect();
    files.sort();
    files.iter().map(|p| run_plugin(p, Duration::from_secs(10))).collect()
}

pub fn run_plugin(path: &Path, timeout: Duration) -> Finding {
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let Ok(mut child) = Command::new(path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else {
        return Finding::skipped("plugin", format!("{name}: cannot execute"));
    };
    // ponytail: poll loop; a plugin printing >64K before exiting blocks on the pipe and times out.
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Finding::new("plugin", Severity::Warn, format!("{name}: timed out after {}s", timeout.as_secs()));
            }
        }
    };
    let mut out = String::new();
    let _ = child.stdout.take().map(|mut s| s.read_to_string(&mut out));
    let mut lines = out.lines();
    let title = lines.next().filter(|l| !l.trim().is_empty()).unwrap_or("no output");
    let detail = lines.collect::<Vec<_>>().join("\n");
    let sev = match status.code() {
        Some(0) => Severity::Ok,
        Some(1) => Severity::Warn,
        Some(2) => Severity::Crit,
        _ => return Finding::skipped("plugin", format!("{name}: {title}")),
    };
    Finding::new("plugin", sev, format!("{name}: {title}")).detail(detail)
}

const REAL_FS: &[&str] = &["ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "vfat", "exfat", "ntfs3", "zfs", "bcachefs"];

/// Real (device, mountpoint) pairs, one per device.
pub fn mounts() -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    fs::read_to_string("/proc/self/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() > 2 && REAL_FS.contains(&f[2])).then(|| (f[0].to_string(), f[1].replace("\\040", " ")))
        })
        .filter(|(dev, _)| seen.insert(dev.clone()))
        .collect()
}

fn pct(used: u64, total: u64) -> f64 {
    if total == 0 { 0.0 } else { used as f64 * 100.0 / total as f64 }
}

fn level(p: f64) -> Severity {
    let c = crate::config::get();
    match p {
        p if p >= c.disk_crit => Severity::Crit,
        p if p >= c.disk_warn => Severity::Warn,
        _ => Severity::Ok,
    }
}

/// Disk space + inode exhaustion.
fn disks() -> Vec<Finding> {
    let mut out = vec![];
    for (dev, mp) in mounts() {
        let Ok(s) = nix::sys::statvfs::statvfs(mp.as_str()) else { continue };
        let frag = s.fragment_size();
        let used = s.blocks() - s.blocks_free();
        let avail = s.blocks_available();
        let p = pct(used, used + avail); // same formula as df
        let sev = level(p);
        if sev != Severity::Ok {
            out.push(
                Finding::new("disk-full", sev, format!("{mp} is {p:.0}% full"))
                    .detail(format!("{dev}: {} free", human(avail * frag)))
                    .cmd("sysdoctor clean"),
            );
        }
        let files = s.files();
        if files > 0 {
            let ip = pct(files - s.files_free(), files);
            let isev = level(ip);
            if isev != Severity::Ok {
                out.push(
                    Finding::new("inodes", isev, format!("{mp} inodes {ip:.0}% used"))
                        .detail("Too many small files; writes fail even with free space")
                        .cmd(format!("sudo du --inodes -x {mp} | sort -n | tail")),
                );
            }
        }
    }
    if out.is_empty() {
        out.push(Finding::ok("disk-full", "Disk space and inodes OK"));
    }
    out
}

/// Physical disks: /dev/<name> for sd*, nvme*, vd*, mmcblk*.
pub fn block_disks() -> Vec<String> {
    fs::read_dir("/sys/block")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !["loop", "ram", "zram", "dm-", "md", "sr", "fd"].iter().any(|p| n.starts_with(p)))
        .collect()
}

fn smart() -> Vec<Finding> {
    if run("smartctl", &["--version"]).is_none() {
        return vec![Finding::skipped("smart", "smartctl not installed").cmd(crate::pkg::install("smartmontools"))];
    }
    let disks = block_disks();
    let mut out = vec![];
    for d in disks {
        let dev = format!("/dev/{d}");
        let Some(j) = run_any("smartctl", &["-j", "-H", &dev]) else { continue };
        let v: serde_json::Value = serde_json::from_str(&j).unwrap_or_default();
        match v["smart_status"]["passed"].as_bool() {
            Some(false) => out.push(
                Finding::new("smart", Severity::Crit, format!("{dev} SMART health FAILED"))
                    .detail("Disk predicts its own failure. Back up now.")
                    .cmd(format!("sudo smartctl -a {dev}")),
            ),
            Some(true) => {}
            None => {
                return vec![Finding::skipped("smart", "needs root").cmd("sudo sysdoctor check")];
            }
        }
    }
    if out.is_empty() {
        out.push(Finding::ok("smart", "SMART health OK"));
    }
    out
}

fn failed_units() -> Vec<Finding> {
    let mut out = vec![];
    for (scope, args) in [("system", vec![]), ("user", vec!["--user"])] {
        let mut a = args.clone();
        a.extend(["--failed", "--plain", "--no-legend", "--no-pager"]);
        let Some(s) = run("systemctl", &a) else { continue };
        for unit in s.lines().filter_map(|l| l.split_whitespace().next()) {
            let flag = if scope == "user" { "--user " } else { "" };
            out.push(
                Finding::new("failed-unit", Severity::Warn, format!("{scope} unit {unit} failed"))
                    .cmd(format!("journalctl {flag}-u {unit} -b -n 50")),
            );
        }
    }
    if out.is_empty() {
        out.push(Finding::ok("failed-unit", "No failed units"));
    }
    out
}

fn reboot_required() -> Vec<Finding> {
    match crate::pkg::reboot_needed() {
        None => return vec![Finding::skipped("reboot", "can't tell if a reboot is pending on this distro")],
        Some(false) => return vec![Finding::ok("reboot", "No reboot pending")],
        Some(true) => {}
    }
    // Debian family lists the packages that asked for it
    let pkgs = fs::read_to_string("/var/run/reboot-required.pkgs").unwrap_or_default();
    let pkgs: Vec<&str> = pkgs.lines().collect();
    let f = Finding::new("reboot", Severity::Warn, "Reboot required");
    let f = if pkgs.is_empty() { f } else { f.detail(format!("Updated: {}", pkgs.join(", "))) };
    vec![f
        .cmd("sudo reboot")]
}

fn oom() -> Vec<Finding> {
    let Some(s) = run_any("journalctl", &["-k", "-b", "-g", "Out of memory: Killed process", "-o", "cat", "--no-pager"]) else {
        return vec![Finding::skipped("oom", "journalctl unavailable")];
    };
    let kills: Vec<&str> = s.lines().filter(|l| l.contains("Killed process")).collect();
    if kills.is_empty() {
        return vec![Finding::ok("oom", "No OOM kills this boot")];
    }
    vec![Finding::new("oom", Severity::Warn, format!("{} OOM kill(s) this boot", kills.len()))
        .detail(kills.last().unwrap().to_string())
        .cmd("journalctl -k -b -g 'Out of memory'")]
}

fn ntp() -> Vec<Finding> {
    match run("timedatectl", &["show", "-p", "NTPSynchronized", "--value"]).as_deref().map(str::trim) {
        Some("yes") => vec![Finding::ok("ntp", "Clock synced")],
        Some(_) => vec![Finding::new("ntp", Severity::Warn, "Clock not NTP-synced")
            .detail("TLS, Kerberos and logs break with drifting time")
            .cmd("sudo timedatectl set-ntp true")],
        None => vec![Finding::skipped("ntp", "timedatectl unavailable")],
    }
}

fn active(unit: &str) -> bool {
    run_any("systemctl", &["is-active", unit]).is_some_and(|s| s.trim() == "active")
}

fn firewall() -> Vec<Finding> {
    // ufw.service stays "active" even when ufw is disabled; the real switch is ENABLED= in ufw.conf.
    let ufw = fs::read_to_string("/etc/ufw/ufw.conf").is_ok_and(|s| s.lines().any(|l| l.trim() == "ENABLED=yes"));
    if ufw || active("firewalld") || active("nftables") {
        return vec![Finding::ok("firewall", "Firewall active")];
    }
    vec![Finding::new("firewall", Severity::Warn, "No firewall active")
        .detail("Checked ufw, firewalld, nftables")
        .cmd("sudo ufw enable")]
}

/// Effective global PasswordAuthentication, sshd semantics: first value wins, Include expands
/// inline, lines after a top-level Match are conditional and ignored. Default yes.
pub fn sshd_password_auth(path: &Path) -> bool {
    fn walk(path: &Path, depth: u8) -> Option<bool> {
        if depth > 8 {
            return None;
        }
        let text = fs::read_to_string(path).ok()?;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.splitn(2, |c: char| c.is_whitespace() || c == '=');
            let key = it.next()?.to_ascii_lowercase();
            let val = it.next().unwrap_or("").trim().trim_start_matches('=').trim();
            match key.as_str() {
                "match" => return None, // rest of file is conditional
                "include" => {
                    for pat in val.split_whitespace() {
                        for f in glob(pat) {
                            if let Some(v) = walk(&f, depth + 1) {
                                return Some(v);
                            }
                        }
                    }
                }
                "passwordauthentication" => return Some(val.eq_ignore_ascii_case("yes")),
                _ => {}
            }
        }
        None
    }
    walk(path, 0).unwrap_or(true)
}

/// Minimal glob: supports a single '*' in the file name. Relative paths resolve against /etc/ssh.
fn glob(pat: &str) -> Vec<PathBuf> {
    let p = if Path::new(pat).is_absolute() { PathBuf::from(pat) } else { Path::new("/etc/ssh").join(pat) };
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Some((pre, suf)) = name.split_once('*') else { return vec![p] };
    let dir = p.parent().unwrap_or(Path::new("/"));
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|f| {
            let n = f.file_name().unwrap().to_string_lossy();
            n.starts_with(pre) && n.ends_with(suf) && n.len() >= pre.len() + suf.len()
        })
        .collect();
    v.sort();
    v
}

fn ssh_password() -> Vec<Finding> {
    let cfg = Path::new("/etc/ssh/sshd_config");
    if !cfg.exists() {
        return vec![Finding::ok("ssh", "sshd not installed")];
    }
    if !["ssh", "sshd", "ssh.socket"].iter().any(|u| active(u)) {
        return vec![Finding::ok("ssh", "sshd not running")];
    }
    if !sshd_password_auth(cfg) {
        return vec![Finding::ok("ssh", "SSH password login disabled")];
    }
    vec![Finding::new("ssh", Severity::Warn, "SSH accepts password logins")
        .detail("Brute-force target. Use keys, then disable passwords.")
        .cmd("echo 'PasswordAuthentication no' | sudo tee /etc/ssh/sshd_config.d/00-nopass.conf && sudo systemctl reload ssh")]
}

pub fn battery_ratio(full: &str, design: &str) -> Option<f64> {
    let (f, d): (f64, f64) = (full.trim().parse().ok()?, design.trim().parse().ok()?);
    (d > 0.0).then(|| f * 100.0 / d)
}

/// (name, health %) for each battery.
pub fn battery_levels() -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = fs::read_dir("/sys/class/power_supply")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let p = e.path();
            let read = |f: &str| fs::read_to_string(p.join(f)).ok();
            let r = ["energy", "charge"]
                .iter()
                .find_map(|k| battery_ratio(&read(&format!("{k}_full"))?, &read(&format!("{k}_full_design"))?))?;
            name.starts_with("BAT").then_some((name, r))
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn battery() -> Vec<Finding> {
    let c = crate::config::get();
    battery_levels()
        .into_iter()
        .map(|(name, r)| {
            if r < c.battery_crit {
                Finding::new("battery", Severity::Crit, format!("{name} health {r:.0}%")).detail("Consider replacing")
            } else if r < c.battery_warn {
                Finding::new("battery", Severity::Warn, format!("{name} health {r:.0}%"))
                    .detail(format!("Worn; capacity below {:.0}% of design", c.battery_warn))
            } else {
                Finding::ok("battery", format!("{name} health {r:.0}%"))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_protocol() {
        let d = tempfile::tempdir().unwrap();
        let mk = |name: &str, body: &str| {
            let p = d.path().join(name);
            fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let t = Duration::from_secs(2);
        let f = run_plugin(&mk("ok", "echo fine"), t);
        assert_eq!((f.severity, f.title.as_str()), (Severity::Ok, "ok: fine"));
        let f = run_plugin(&mk("bk", "echo 'backup old'; echo 'last: 9d'; exit 2"), t);
        assert_eq!((f.severity, f.title.as_str(), f.detail.as_str()), (Severity::Crit, "bk: backup old", "last: 9d"));
        assert!(run_plugin(&mk("unk", "exit 3"), t).title.starts_with("skipped"));
        let f = run_plugin(&mk("slow", "sleep 5"), Duration::from_millis(300));
        assert_eq!(f.severity, Severity::Warn);
        assert!(f.title.contains("timed out"));
    }

    #[test]
    fn battery_math() {
        assert_eq!(battery_ratio("40000000\n", "50000000\n"), Some(80.0));
        assert_eq!(battery_ratio("1", "0"), None);
        assert_eq!(battery_ratio("x", "1"), None);
    }

    #[test]
    fn sshd_semantics() {
        let d = tempfile::tempdir().unwrap();
        let inc = d.path().join("conf.d");
        fs::create_dir(&inc).unwrap();
        let main = d.path().join("sshd_config");
        let w = |p: &Path, s: &str| fs::write(p, s).unwrap();

        // default
        w(&main, "# nothing\nPort 22\n");
        assert!(sshd_password_auth(&main));
        // explicit no, case-insensitive key, '=' form
        w(&main, "passwordauthentication=no\n");
        assert!(!sshd_password_auth(&main));
        // first wins: include sets no before later yes
        w(&inc.join("10-a.conf"), "# c\nPasswordAuthentication no\n");
        w(&inc.join("20-b.conf"), "PasswordAuthentication yes\n");
        w(&main, &format!("Include {}/*.conf\nPasswordAuthentication yes\n", inc.display()));
        assert!(!sshd_password_auth(&main));
        // non-matching glob files ignored
        fs::remove_file(inc.join("10-a.conf")).unwrap();
        w(&inc.join("10-a.bak"), "PasswordAuthentication no\n");
        assert!(sshd_password_auth(&main));
        // Match block is conditional, not global
        w(&main, "Match User bob\n  PasswordAuthentication no\n");
        assert!(sshd_password_auth(&main));
    }
}
