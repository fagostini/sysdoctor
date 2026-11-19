use crate::check::mounts;
use crate::clean::du;
use crate::finding::{human, run, xdg, Finding, Severity};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
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

/// One grouped notification for a batch of new problems: (urgency, title, body).
pub fn summary(new: &[&Finding]) -> (&'static str, String, String) {
    let crit = new.iter().any(|f| f.severity == Severity::Crit);
    let urgency = if crit { "critical" } else { "normal" };
    match new {
        [f] => (urgency, format!("sysdoctor: {}", f.title), f.detail.clone()),
        _ => (
            urgency,
            format!("sysdoctor: {} new problems", new.len()),
            new.iter().map(|f| format!("• {}", f.title)).collect::<Vec<_>>().join("\n"),
        ),
    }
}

fn notify(new: &[&Finding]) {
    if new.is_empty() {
        return;
    }
    let (urgency, title, body) = summary(new);
    // Waiting for a button click must outlive this process: the timer's oneshot cgroup is killed
    // when `watch` exits, so the waiter runs in its own transient user unit.
    let launched = crate::finding::exe().is_ok_and(|exe| {
        Command::new("systemd-run")
            .args(["--user", "--collect", "--quiet"])
            .arg(exe)
            .args(["notify", "--urgency", urgency, &title, &body])
            .status()
            .is_ok_and(|s| s.success())
    });
    if !launched && run("notify-send", &["-a", "sysdoctor", "-u", urgency, &title, &body]).is_none() {
        println!("{title}\n{body}"); // lands in the journal when run from the timer
    }
}

/// Show a notification with buttons and act on the click. Blocks until clicked or dismissed.
pub fn notify_interactive(urgency: &str, title: &str, body: &str) {
    let out = Command::new("notify-send")
        .args(["-a", "sysdoctor", "-u", urgency, "--action=report=Show report", "--action=snooze=Snooze 24h", title, body])
        .output();
    match out.as_ref().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).as_deref() {
        Ok("report") => open_report(),
        Ok("snooze") => {
            let _ = snooze(24);
        }
        _ => {}
    }
}

fn open_report() {
    open_in_terminal("check");
}

/// Run `sysdoctor <args>` in a new terminal window, which waits for Enter before closing.
pub fn open_in_terminal(args: &str) {
    let Ok(exe) = crate::finding::exe() else { return };
    run_in_terminal(&format!("\"{}\" {args}", exe.display()));
}

/// Run a shell command in a new terminal window, which waits for Enter before closing.
pub fn run_in_terminal(script: &str) {
    // One string after -e: understood by terminator, gnome-terminal and xterm alike.
    let script = script.replace('\'', "'\\''");
    let cmd = format!("sh -c '{script}; printf \"\\nPress Enter to close\"; read _'");
    // ponytail: Debian/Ubuntu/Mint alternatives link; add a config knob if other distros need it.
    // wait: when called from the notification's transient unit, exiting early would kill the terminal
    let _ = Command::new("x-terminal-emulator").args(["-e", &cmd]).status();
}

/// Silence notifications for `hours` (0 = unsnooze).
pub fn snooze(hours: u64) -> io::Result<()> {
    fs::write(state_dir()?.join("snooze-until"), (now() + hours * 3600).to_string())
}

fn snoozed() -> bool {
    state_dir()
        .ok()
        .and_then(|d| fs::read_to_string(d.join("snooze-until")).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .is_some_and(|t| t > now())
}

/// Notify about problems not present last run, then save current state.
/// While snoozed, state still updates: problems that appear during a snooze stay silent.
pub fn watch(findings: &[Finding]) -> io::Result<()> {
    let path = state_dir()?.join("last.json");
    let prev: HashSet<String> = fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    if !snoozed() {
        notify(&new_problems(&prev, findings));
    }
    let cur: Vec<String> = findings.iter().filter(|f| f.severity != Severity::Ok).map(key).collect();
    fs::write(&path, serde_json::to_string(&cur)?)
}

pub fn install() -> io::Result<()> {
    let exe = crate::finding::exe()?;
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

// ---- trends: slow-moving health metrics, one sample per day ----

#[derive(Serialize, Deserialize)]
pub struct Metrics {
    pub ts: u64,
    pub values: BTreeMap<String, f64>,
}

fn metrics() -> BTreeMap<String, f64> {
    let mut m: BTreeMap<String, f64> =
        crate::check::battery_levels().into_iter().map(|(n, r)| (format!("{n} battery health %"), r)).collect();
    // SMART attributes need root; as a user this silently records nothing.
    for d in crate::check::block_disks() {
        let Some(j) = crate::finding::run_any("smartctl", &["-j", "-A", &format!("/dev/{d}")]) else { continue };
        let v: serde_json::Value = serde_json::from_str(&j).unwrap_or_default();
        if let Some(w) = v["nvme_smart_health_information_log"]["percentage_used"].as_f64() {
            m.insert(format!("{d} wear %"), w);
        }
        let realloc = v["ata_smart_attributes"]["table"].as_array().and_then(|t| t.iter().find(|a| a["id"] == 5));
        if let Some(r) = realloc.and_then(|a| a["raw"]["value"].as_f64()) {
            m.insert(format!("{d} reallocated sectors"), r);
        }
    }
    m
}

/// Append today's metrics unless already recorded in the last 20h.
pub fn record_metrics() -> io::Result<()> {
    let path = state_dir()?.join("trends.jsonl");
    let last = load_jsonl::<Metrics>(&path).pop();
    if last.is_some_and(|s| now().saturating_sub(s.ts) < 20 * 3600) {
        return Ok(());
    }
    let values = metrics();
    if values.is_empty() {
        return Ok(());
    }
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", serde_json::to_string(&Metrics { ts: now(), values })?)
}

/// Per metric: (first value, first ts, last value, last ts).
pub fn trend_table(samples: &[Metrics]) -> BTreeMap<String, (f64, u64, f64, u64)> {
    let mut t: BTreeMap<String, (f64, u64, f64, u64)> = BTreeMap::new();
    for s in samples {
        for (k, v) in &s.values {
            t.entry(k.clone()).and_modify(|e| (e.2, e.3) = (*v, s.ts)).or_insert((*v, s.ts, *v, s.ts));
        }
    }
    t
}

pub fn trends() -> io::Result<()> {
    record_metrics()?;
    let samples = load_jsonl::<Metrics>(&state_dir()?.join("trends.jsonl"));
    if samples.len() < 2 {
        println!("Not enough history yet ({} sample). The timer (`sysdoctor watch --install`) records one daily.", samples.len());
        println!("Disk wear needs root: run the timer's command once as `sudo sysdoctor trends` to include it.");
        return Ok(());
    }
    for (k, (v0, t0, v1, t1)) in trend_table(&samples) {
        let days = (t1 - t0) as f64 / 86400.0;
        let rate = if days >= 1.0 { format!("  ({:+.2} per 30 days)", (v1 - v0) / days * 30.0) } else { String::new() };
        println!("{k:<34} {v0:>7.1} -> {v1:>7.1} over {days:.0} days{rate}");
    }
    Ok(())
}

fn load_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    fs::File::open(path)
        .map(|f| io::BufReader::new(f).lines().map_while(Result::ok).filter_map(|l| serde_json::from_str(&l).ok()).collect())
        .unwrap_or_default()
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
    fn grouped_summary() {
        let a = Finding::new("ntp", Severity::Warn, "Clock not NTP-synced").detail("d");
        let b = Finding::new("smart", Severity::Crit, "/dev/sda SMART health FAILED");
        assert_eq!(summary(&[&a]), ("normal", "sysdoctor: Clock not NTP-synced".into(), "d".into()));
        let (u, t, body) = summary(&[&a, &b]);
        assert_eq!((u, t.as_str()), ("critical", "sysdoctor: 2 new problems"));
        assert_eq!(body, "• Clock not NTP-synced\n• /dev/sda SMART health FAILED");
    }

    #[test]
    fn trend_first_last() {
        let s = |ts, v: f64| Metrics { ts, values: [("BAT1 battery health %".to_string(), v)].into() };
        let t = trend_table(&[s(0, 85.0), s(86400, 84.0), s(30 * 86400, 82.0)]);
        assert_eq!(t["BAT1 battery health %"], (85.0, 0, 82.0, 30 * 86400));
    }

    #[test]
    fn growth_deltas() {
        let m = |v: &[(&str, u64)]| v.iter().map(|(k, b)| (k.to_string(), *b)).collect::<BTreeMap<_, _>>();
        let old = m(&[("a", 100), ("b", 50), ("gone", 10)]);
        let new = m(&[("a", 300), ("b", 50), ("fresh", 20)]);
        assert_eq!(deltas(&old, &new), [("a".into(), 200), ("fresh".into(), 20), ("gone".into(), -10)]);
    }
}
