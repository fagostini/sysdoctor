//! Backups, journal error digest, hardware (throttling, memory pressure, firmware).
use crate::config;
use crate::finding::{run, run_any, Finding, Severity};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Parse any date `date -d` understands into a Unix timestamp.
pub fn epoch(s: &str) -> Option<u64> {
    run("date", &["-d", s.trim(), "+%s"])?.trim().parse().ok()
}

fn mtime(p: &Path) -> Option<u64> {
    fs::metadata(p).ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

/// Where a filesystem with this UUID is mounted, if it is.
fn mount_of_uuid(uuid: &str) -> Option<PathBuf> {
    let dev = fs::canonicalize(format!("/dev/disk/by-uuid/{uuid}")).ok()?;
    let mounts = fs::read_to_string("/proc/self/mounts").ok()?;
    mounts.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let d = f.next()?;
        let mp = f.next()?;
        (fs::canonicalize(d).ok()? == dev).then(|| PathBuf::from(mp.replace("\\040", " ")))
    })
}

/// (tool, newest backup timestamp, note) for each backup tool found.
fn backups() -> Vec<(&'static str, Option<u64>, String)> {
    let mut out = vec![];
    // Timeshift: snapshot dirs on the configured device (rsync or btrfs mode).
    if let Ok(cfg) = fs::read_to_string("/etc/timeshift/timeshift.json") {
        let v: serde_json::Value = serde_json::from_str(&cfg).unwrap_or_default();
        let uuid = v["backup_device_uuid"].as_str().unwrap_or("");
        let btrfs = v["btrfs_mode"].as_str() == Some("true");
        match (!uuid.is_empty()).then(|| mount_of_uuid(uuid)).flatten() {
            Some(mp) => {
                let dir = mp.join(if btrfs { "timeshift-btrfs/snapshots" } else { "timeshift/snapshots" });
                let newest = fs::read_dir(&dir).into_iter().flatten().flatten().filter_map(|e| mtime(&e.path())).max();
                let root_dev = fs::metadata("/").ok().map(|m| std::os::unix::fs::MetadataExt::dev(&m));
                let same_disk = fs::metadata(&mp).ok().map(|m| std::os::unix::fs::MetadataExt::dev(&m)) == root_dev;
                let note = if same_disk { "snapshots are on the system disk: they won't survive a disk failure".into() } else { String::new() };
                out.push(("Timeshift", newest, note));
            }
            // Device not mounted (normal for rsync mode): fall back to Timeshift's own run logs.
            None if !uuid.is_empty() => out.push(("Timeshift", timeshift_last_log(), "backup device not mounted; using last Timeshift run log".into())),
            None => {}
        }
    }
    // Kopia (CLI or the KopiaUI bundle): newest snapshot end time across all sources.
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let kcfg = home.join(".config/kopia/repository.config");
    if kcfg.exists() {
        let bin = ["kopia", "/opt/KopiaUI/resources/server/kopia"]
            .into_iter()
            .find(|b| run(b, &["--version"]).is_some());
        // timeout: a remote repository on a slow or absent network must not stall the check
        let list = bin.and_then(|b| run("timeout", &["30", b, "snapshot", "list", "--all", "--json", "--config-file", &kcfg.to_string_lossy()]));
        match list {
            Some(j) => {
                let v: serde_json::Value = serde_json::from_str(&j).unwrap_or_default();
                let newest = v.as_array().into_iter().flatten().filter_map(|s| s["endTime"].as_str()).max().and_then(epoch);
                out.push(("Kopia", newest, String::new()));
            }
            None => out.push(("Kopia", None, "repository not reachable (offline, or password not stored)".into())),
        }
    }
    out.extend(restic());
    out.extend(borg(&home));
    out.extend(snapper());
    // Déjà Dup records its last run in gsettings.
    if let Some(s) = run("gsettings", &["get", "org.gnome.DejaDup", "last-backup"]) {
        let s = s.trim().trim_matches('\'');
        if !s.is_empty() {
            out.push(("Déjà Dup", epoch(s), String::new()));
        }
    }
    out
}

/// Largest string value under `key` anywhere in a JSON tree (restic "time", borg "time", snapper "date").
/// ponytail: string max assumes one UTC offset across snapshots; parse each if mixed offsets ever matter.
pub fn newest(v: &serde_json::Value, key: &str) -> Option<String> {
    use serde_json::Value;
    match v {
        Value::Array(a) => a.iter().filter_map(|x| newest(x, key)).max(),
        Value::Object(o) => {
            let own = o.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
            o.values().filter_map(|x| newest(x, key)).chain(own).max()
        }
        _ => None,
    }
}

/// Newest mtime of the entries directly in `dir`.
fn newest_mtime(dir: &Path) -> Option<u64> {
    fs::read_dir(dir).ok()?.flatten().filter_map(|e| mtime(&e.path())).max()
}

/// Local repo path from a restic/borg repository string, if it is local.
fn local_repo(repo: &str) -> Option<PathBuf> {
    let p = repo.strip_prefix("local:").or_else(|| repo.strip_prefix("file://")).unwrap_or(repo);
    p.starts_with('/').then(|| PathBuf::from(p))
}

/// restic via $RESTIC_REPOSITORY(_FILE). Local repos: newest file in snapshots/ (no password needed).
fn restic() -> Option<(&'static str, Option<u64>, String)> {
    let repo = std::env::var("RESTIC_REPOSITORY")
        .ok()
        .or_else(|| fs::read_to_string(std::env::var_os("RESTIC_REPOSITORY_FILE")?).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    if let Some(dir) = local_repo(&repo) {
        return Some(("restic", newest_mtime(&dir.join("snapshots")), String::new()));
    }
    let has_pw = ["RESTIC_PASSWORD", "RESTIC_PASSWORD_FILE", "RESTIC_PASSWORD_COMMAND"].iter().any(|k| std::env::var_os(k).is_some());
    let list = has_pw.then(|| run("timeout", &["30", "restic", "snapshots", "--json", "--no-lock"])).flatten();
    Some(match list {
        Some(j) => ("restic", newest(&serde_json::from_str(&j).unwrap_or_default(), "time").and_then(|t| epoch(&t)), String::new()),
        None => ("restic", None, "repository not reachable (offline, or no password in the environment)".into()),
    })
}

/// borg via borgmatic config or $BORG_REPO. Local repos: borg rewrites index.* on every commit.
fn borg(home: &Path) -> Option<(&'static str, Option<u64>, String)> {
    let borgmatic = [home.join(".config/borgmatic/config.yaml"), PathBuf::from("/etc/borgmatic/config.yaml")].into_iter().any(|p| p.exists());
    if borgmatic {
        let j = run("timeout", &["60", "borgmatic", "list", "--last", "1", "--json"]);
        let t = j.and_then(|j| newest(&serde_json::from_str(&j).ok()?, "time")).and_then(|t| epoch(&t));
        let note = if t.is_none() { "borgmatic list failed (repository offline, or config needs root)" } else { "" };
        return Some(("borgmatic", t, note.into()));
    }
    let repo = std::env::var("BORG_REPO").ok().filter(|s| !s.is_empty())?;
    if let Some(dir) = local_repo(&repo) {
        let t = fs::read_dir(&dir).ok().map(|d| d.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("index.")).filter_map(|e| mtime(&e.path())).max());
        return Some(("borg", t.flatten(), String::new()));
    }
    let j = run("timeout", &["30", "borg", "list", "--last", "1", "--json"]);
    Some(match j.and_then(|j| newest(&serde_json::from_str(&j).ok()?, "time")).and_then(|t| epoch(&t)) {
        Some(t) => ("borg", Some(t), String::new()),
        None => ("borg", None, "repository not reachable (offline, or no passphrase in the environment)".into()),
    })
}

/// snapper: newest snapshot across configs. These live on the same filesystem.
fn snapper() -> Vec<(&'static str, Option<u64>, String)> {
    let configs: Vec<String> = fs::read_dir("/etc/snapper/configs").into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    configs
        .into_iter()
        .map(|c| match run("snapper", &["--jsonout", "-c", &c, "list"]) {
            Some(j) => {
                let t = newest(&serde_json::from_str(&j).unwrap_or_default(), "date").and_then(|t| epoch(&t));
                ("snapper", t, format!("config {c}; snapshots are on the same disk: they won't survive a disk failure"))
            }
            None => ("snapper", None, format!("can't list config {c}: add your user to ALLOW_USERS in /etc/snapper/configs/{c}")),
        })
        .collect()
}

/// Newest /var/log/timeshift/YYYY-MM-DD_HH-MM-SS_backup.log, as a timestamp.
fn timeshift_last_log() -> Option<u64> {
    let newest = fs::read_dir("/var/log/timeshift")
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix("_backup.log").map(String::from))
        .max()?;
    let (d, t) = newest.split_once('_')?;
    epoch(&format!("{d} {}", t.replace('-', ":")))
}

pub fn backup() -> Vec<Finding> {
    let max = config::get().backup_max_days;
    let found = backups();
    if found.is_empty() {
        return vec![Finding::new("backup", Severity::Warn, "No backups detected")
            .detail("Checked Timeshift, Kopia, restic ($RESTIC_REPOSITORY), borg ($BORG_REPO or borgmatic), snapper and Déjà Dup. Something else? Add a checks.d plugin, then `sysdoctor ignore \"no backups\"`.")];
    }
    found
        .into_iter()
        .map(|(tool, ts, note)| match ts {
            Some(t) => {
                let days = now().saturating_sub(t) as f64 / 86400.0;
                let f = if days > max as f64 {
                    Finding::new("backup", Severity::Warn, format!("{tool}: last backup {days:.0} days ago"))
                } else {
                    Finding::ok("backup", format!("{tool}: last backup {days:.1} days ago"))
                };
                f.detail(note)
            }
            None => Finding::new("backup", Severity::Warn, format!("{tool}: no snapshot found")).detail(note),
        })
        .collect()
}

// ---- journal digest ----

/// Collapse variable parts so repeats of the same error group together.
fn normalize(msg: &str) -> String {
    msg.split_whitespace()
        .map(|t| if t.chars().any(|c| c.is_ascii_digit()) { "#" } else { t })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Error-priority journal entries for a boot, grouped: pattern -> count.
fn error_patterns(boot: &str) -> Option<HashMap<String, usize>> {
    let out = run("journalctl", &["-b", boot, "-p", "err", "-n", "5000", "--no-pager", "-o", "json", "--output-fields=MESSAGE,SYSLOG_IDENTIFIER"])?;
    let mut m = HashMap::new();
    for l in out.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else { continue };
        let Some(msg) = v["MESSAGE"].as_str() else { continue }; // binary messages come as arrays
        let ident = v["SYSLOG_IDENTIFIER"].as_str().unwrap_or("?");
        *m.entry(format!("{ident}: {}", normalize(msg))).or_insert(0) += 1;
    }
    Some(m)
}

pub fn journal_digest() -> Vec<Finding> {
    let Some(cur) = error_patterns("0") else { return vec![Finding::skipped("journal-digest", "journalctl unavailable")] };
    if cur.is_empty() {
        return vec![Finding::ok("journal-digest", "No errors logged this boot")];
    }
    let prev: Option<HashSet<String>> = error_patterns("-1").map(|m| m.into_keys().collect());
    let is_new = |k: &String| prev.as_ref().is_some_and(|p| !p.contains(k));
    let new = cur.keys().filter(|k| is_new(k)).count();
    let mut top: Vec<(&String, &usize)> = cur.iter().collect();
    // new patterns first, then by count
    top.sort_by_key(|(k, n)| (!is_new(k), std::cmp::Reverse(**n)));
    let detail = top
        .iter()
        .take(6)
        .map(|(k, n)| {
            let tag = if is_new(k) { "  (new)" } else { "" };
            let k: String = k.chars().take(110).collect();
            format!("{n:>5}x  {k}{tag}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let title = match &prev {
        Some(_) => format!("{} error patterns this boot, {new} new since last boot", cur.len()),
        None => format!("{} error patterns this boot", cur.len()),
    };
    let sev = if new > 0 { Severity::Warn } else { Severity::Ok };
    vec![Finding::new("journal-digest", sev, title).detail(detail).cmd("journalctl -b -p err")]
}

// ---- hardware ----

pub fn throttling() -> Vec<Finding> {
    let total: u64 = fs::read_dir("/sys/devices/system/cpu")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path().join("thermal_throttle/core_throttle_count")).ok())
        .filter_map(|s| s.trim().parse::<u64>().ok())
        .sum();
    // ponytail: fixed threshold; a few throttle events under load are normal. Rate per hour if this is noisy.
    if total >= 100 {
        vec![Finding::new("throttle", Severity::Warn, format!("CPU thermal throttling: {total} events since boot"))
            .detail("Check fans, dust, and thermal paste. `sensors` shows temperatures.")]
    } else {
        vec![]
    }
}

/// (some avg300, full avg300) from a /proc/pressure/memory text.
fn psi(text: &str) -> Option<(f64, f64)> {
    let get = |kind: &str| -> Option<f64> {
        let line = text.lines().find(|l| l.starts_with(kind))?;
        line.split_whitespace().find_map(|t| t.strip_prefix("avg300="))?.parse().ok()
    };
    Some((get("some")?, get("full")?))
}

pub fn memory_pressure() -> Vec<Finding> {
    let Some((some, full)) = fs::read_to_string("/proc/pressure/memory").ok().as_deref().and_then(psi) else { return vec![] };
    if full >= 5.0 || some >= 20.0 {
        vec![Finding::new("memory-pressure", Severity::Warn, format!("Memory pressure high: tasks stalled {some:.0}% of the last 5 min"))
            .detail("Not enough RAM for the workload. Close apps, add swap/zram, or add RAM.")
            .cmd("ps -eo pid,rss,comm --sort=-rss | head")]
    } else {
        vec![Finding::ok("memory-pressure", "Memory pressure low")]
    }
}

pub fn firmware() -> Vec<Finding> {
    let Some(log) = run_any("journalctl", &["-k", "-b", "-o", "cat", "--no-pager", "-g", "firmware"]) else { return vec![] };
    let mut missing: Vec<&str> = log
        .lines()
        .filter_map(|l| l.split("Direct firmware load for ").nth(1)?.split(" failed").next())
        .collect();
    missing.sort();
    missing.dedup();
    if missing.is_empty() {
        return vec![];
    }
    vec![Finding::new("firmware", Severity::Warn, format!("{} firmware file(s) failed to load", missing.len()))
        .detail(missing.join("\n"))
        .cmd(crate::pkg::install("linux-firmware"))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_json() {
        let restic: serde_json::Value = serde_json::from_str(r#"[{"time":"2024-05-01T10:00:00Z"},{"time":"2024-06-02T09:00:00Z"}]"#).unwrap();
        assert_eq!(newest(&restic, "time").as_deref(), Some("2024-06-02T09:00:00Z"));
        let borgmatic: serde_json::Value = serde_json::from_str(r#"[{"repository":{"last_modified":"x"},"archives":[{"name":"a","time":"2024-07-01T03:00:00.000000"}]}]"#).unwrap();
        assert_eq!(newest(&borgmatic, "time").as_deref(), Some("2024-07-01T03:00:00.000000"));
        let snapper: serde_json::Value = serde_json::from_str(r#"{"root":[{"number":0,"date":""},{"number":5,"date":"2024-08-01 12:00:00"}]}"#).unwrap();
        assert_eq!(newest(&snapper, "date").as_deref(), Some("2024-08-01 12:00:00"));
        assert_eq!(local_repo("local:/mnt/b"), Some(PathBuf::from("/mnt/b")));
        assert_eq!(local_repo("sftp:host:/b"), None);
        assert_eq!(local_repo("ssh://u@h/./repo"), None);
    }

    #[test]
    fn normalize_groups_variants() {
        assert_eq!(normalize("usb 1-2: device descriptor read/64, error -71"), "usb # device descriptor # error #");
        assert_eq!(normalize("usb 3-4: device descriptor read/64, error -110"), normalize("usb 1-2: device descriptor read/64, error -71"));
    }

    #[test]
    fn psi_parse() {
        let t = "some avg10=0.00 avg60=1.50 avg300=12.25 total=1\nfull avg10=0.00 avg60=0.00 avg300=3.00 total=2\n";
        assert_eq!(psi(t), Some((12.25, 3.0)));
        assert_eq!(psi("garbage"), None);
    }
}
