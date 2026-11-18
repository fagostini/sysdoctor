use crate::check::mounts;
use crate::clean::du;
use crate::finding::{human, run, xdg, Finding, Severity};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn state_dir() -> io::Result<PathBuf> {
    let d = xdg("XDG_STATE_HOME", ".local/state").ok_or(io::ErrorKind::NotFound)?.join("sysdoctor");
    fs::create_dir_all(&d)?;
    Ok(d)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Identity of a problem across runs. Digits are stripped so "91% full" -> "92% full" isn't "new";
/// severity is kept so warn -> crit re-notifies.
// ponytail: digit stripping also merges e.g. /mnt/disk2 and /mnt/disk3; add a per-finding key if that bites.
pub fn key(f: &Finding) -> String {
    let t: String = f.title.chars().filter(|c| !c.is_ascii_digit()).collect();
    format!("{}|{:?}|{t}", f.id, f.severity)
}

pub fn new_problems<'a>(prev: &HashSet<String>, findings: &'a [Finding]) -> Vec<&'a Finding> {
    findings.iter().filter(|f| f.severity != Severity::Ok && !prev.contains(&key(f))).collect()
}

fn notify(f: &Finding) {
    let urgency = if f.severity == Severity::Crit { "critical" } else { "normal" };
    let title = format!("sysdoctor: {}", f.title);
    if run("notify-send", &["-a", "sysdoctor", "-u", urgency, &title, &f.detail]).is_none() {
        println!("{title}"); // lands in the journal when run from the timer
    }
}

/// Notify about problems not present last run, then save current state.
pub fn watch(findings: &[Finding]) -> io::Result<()> {
    let path = state_dir()?.join("last.json");
    let prev: HashSet<String> = fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    for f in new_problems(&prev, findings) {
        notify(f);
    }
    let cur: Vec<String> = findings.iter().filter(|f| f.severity != Severity::Ok).map(key).collect();
    fs::write(&path, serde_json::to_string(&cur)?)
}

pub fn install() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    if exe.components().any(|c| c.as_os_str() == "target") {
        eprintln!("warning: {} is a build artifact; run `cargo install --path .` and install from there", exe.display());
    }
    let dir = xdg("XDG_CONFIG_HOME", ".config").ok_or(io::ErrorKind::NotFound)?.join("systemd/user");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("sysdoctor.service"),
        format!("[Unit]\nDescription=sysdoctor health watch\n\n[Service]\nType=oneshot\nNice=19\nIOSchedulingClass=idle\nExecStart={} watch\n", exe.display()),
    )?;
    fs::write(
        dir.join("sysdoctor.timer"),
        "[Unit]\nDescription=Run sysdoctor watch hourly\n\n[Timer]\nOnCalendar=hourly\nPersistent=true\nRandomizedDelaySec=5m\n\n[Install]\nWantedBy=timers.target\n",
    )?;
    run("systemctl", &["--user", "daemon-reload"]).ok_or(io::Error::other("systemctl --user daemon-reload failed"))?;
    run("systemctl", &["--user", "enable", "--now", "sysdoctor.timer"]).ok_or(io::Error::other("enabling timer failed"))?;
    println!("Installed {}/sysdoctor.{{service,timer}}; runs hourly.", dir.display());
    Ok(())
}

// ---- growth tracking ----

#[derive(Serialize, Deserialize)]
pub struct Snap {
    pub ts: u64,
    pub sizes: BTreeMap<String, u64>,
}

/// Each top-level entry of $HOME (hidden included) + used bytes per mount.
/// Slow on big homes (minutes cold), so it runs at most daily from the timer.
fn measure(home: &Path) -> BTreeMap<String, u64> {
    let paths: Vec<PathBuf> = fs::read_dir(home).into_iter().flatten().flatten().map(|e| e.path()).collect();
    // ponytail: one thread per top-level entry; a pool if homes with thousands of entries show up.
    let mut m: BTreeMap<String, u64> = std::thread::scope(|s| {
        let hs: Vec<_> = paths.iter().map(|p| s.spawn(move || du(p))).collect();
        paths
            .iter()
            .zip(hs)
            .map(|(p, h)| (format!("~/{}", p.file_name().unwrap_or_default().to_string_lossy()), h.join().unwrap_or(0)))
            .collect()
    });
    for (_, mp) in mounts() {
        if let Ok(s) = nix::sys::statvfs::statvfs(mp.as_str()) {
            m.insert(format!("mount {mp}"), (s.blocks() - s.blocks_free()) * s.fragment_size());
        }
    }
    m
}

fn load(path: &Path) -> Vec<Snap> {
    fs::File::open(path)
        .map(|f| io::BufReader::new(f).lines().map_while(Result::ok).filter_map(|l| serde_json::from_str(&l).ok()).collect())
        .unwrap_or_default()
}

/// Measure and append a snapshot unless one was taken in the last 20h (checked before measuring).
pub fn snapshot(home: &Path) -> io::Result<Vec<Snap>> {
    let path = state_dir()?.join("growth.jsonl");
    let mut snaps = load(&path);
    if snaps.last().is_some_and(|s| now().saturating_sub(s.ts) < 20 * 3600) {
        return Ok(snaps);
    }
    let cur = Snap { ts: now(), sizes: measure(home) };
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", serde_json::to_string(&cur)?)?;
    snaps.push(cur);
    Ok(snaps)
}

/// Size change per key, biggest growth first. Keys missing on one side count as 0.
pub fn deltas(old: &BTreeMap<String, u64>, new: &BTreeMap<String, u64>) -> Vec<(String, i64)> {
    let keys: HashSet<&String> = old.keys().chain(new.keys()).collect();
    let mut v: Vec<(String, i64)> = keys
        .into_iter()
        .map(|k| (k.clone(), *new.get(k).unwrap_or(&0) as i64 - *old.get(k).unwrap_or(&0) as i64))
        .filter(|(_, d)| *d != 0)
        .collect();
    v.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
    v
}

/// Compare the latest snapshot to one at least `days` older. Measures only if no snapshot today.
pub fn growth(home: &Path, days: u64) -> io::Result<()> {
    let path = state_dir()?.join("growth.jsonl");
    if load(&path).last().is_none_or(|s| now().saturating_sub(s.ts) >= 20 * 3600) {
        eprintln!("Measuring {} (can take minutes on a cold cache)...", home.display());
    }
    let snaps = snapshot(home)?;
    let cur = snaps.last().expect("snapshot appends when empty");
    let cutoff = cur.ts.saturating_sub(days * 86400);
    // newest snapshot at least `days` older, else the oldest we have
    let base = snaps.iter().rev().find(|s| s.ts <= cutoff).unwrap_or(&snaps[0]);
    if std::ptr::eq(base, cur) {
        println!("Only one snapshot so far. The timer (`sysdoctor watch --install`) records one daily.");
        return Ok(());
    }
    let age = (cur.ts - base.ts) as f64 / 86400.0;
    println!("Changes over the last {age:.1} days:");
    let d = deltas(&base.sizes, &cur.sizes);
    for (k, b) in d.iter().filter(|(_, b)| b.unsigned_abs() >= 1 << 20).take(15) {
        let sign = if *b > 0 { "+" } else { "-" };
        println!("  {sign}{:>8}  {k}", human(b.unsigned_abs()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_problem_detection() {
        let f = |id, sev, t: &str| Finding::new(id, sev, t);
        let old = [f("disk-full", Severity::Warn, "/ is 91% full")];
        let prev: HashSet<String> = old.iter().map(key).collect();
        let cur = [
            f("disk-full", Severity::Warn, "/ is 92% full"), // same problem, number moved
            f("battery", Severity::Ok, "BAT0 health 95%"),   // ok never notifies
            f("ntp", Severity::Warn, "Clock not NTP-synced"), // new
        ];
        let got: Vec<&str> = new_problems(&prev, &cur).iter().map(|f| f.id).collect();
        assert_eq!(got, ["ntp"]);
        let esc = [f("disk-full", Severity::Crit, "/ is 98% full")]; // escalation re-notifies
        assert_eq!(new_problems(&prev, &esc).len(), 1);
    }

    #[test]
    fn growth_deltas() {
        let m = |v: &[(&str, u64)]| v.iter().map(|(k, b)| (k.to_string(), *b)).collect::<BTreeMap<_, _>>();
        let old = m(&[("a", 100), ("b", 50), ("gone", 10)]);
        let new = m(&[("a", 300), ("b", 50), ("fresh", 20)]);
        assert_eq!(deltas(&old, &new), [("a".into(), 200), ("fresh".into(), 20), ("gone".into(), -10)]);
    }
}
