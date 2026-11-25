//! Thresholds from $XDG_CONFIG_HOME/sysdoctor/config.toml and the ignore list in .../ignore.
use crate::finding::{xdg, Finding};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Filesystem usage % that warns / is critical
    pub disk_warn: f64,
    pub disk_crit: f64,
    /// Battery health % below which to warn / go critical
    pub battery_warn: f64,
    pub battery_crit: f64,
    /// Projects untouched this many days count as stale for target/ and node_modules
    pub stale_days: u64,
    /// ~/.cache subfolders untouched this many days are listed
    pub cache_stale_days: u64,
    /// Journal size above this is reclaimable
    pub journal_max_mb: u64,
    /// Warn when the newest backup is older than this
    pub backup_max_days: u64,
    /// Warn about keys/certs expiring within this many days
    pub expiry_warn_days: u64,
    /// Extra PEM certificates to watch for expiry
    pub cert_paths: Vec<PathBuf>,
    /// Predict when filesystems fill up from the daily growth history
    pub disk_forecast: bool,
    /// Warn when a filesystem is predicted full within this many days
    pub disk_forecast_days: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            disk_warn: 90.0,
            disk_crit: 97.0,
            battery_warn: 80.0,
            battery_crit: 60.0,
            stale_days: 30,
            cache_stale_days: 90,
            journal_max_mb: 500,
            backup_max_days: 7,
            expiry_warn_days: 30,
            cert_paths: vec![],
            disk_forecast: true,
            disk_forecast_days: 30,
        }
    }
}

fn dir() -> Option<PathBuf> {
    xdg("XDG_CONFIG_HOME", ".config").map(|d| d.join("sysdoctor"))
}

pub fn path() -> Option<PathBuf> {
    dir().map(|d| d.join("config.toml"))
}

/// Loaded once. A broken config is reported on stderr and defaults are used: never block a health check.
pub fn get() -> &'static Config {
    static C: OnceLock<Config> = OnceLock::new();
    C.get_or_init(|| {
        let Some(text) = path().and_then(|p| fs::read_to_string(p).ok()) else { return Config::default() };
        toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("sysdoctor: ignoring invalid config.toml: {e}");
            Config::default()
        })
    })
}

// ---- ignore list: one case-insensitive substring per line, matched against "id title" ----

fn ignore_path() -> Option<PathBuf> {
    dir().map(|d| d.join("ignore"))
}

pub fn ignores() -> Vec<String> {
    ignore_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_lowercase)
        .collect()
}

fn is_ignored(f: &Finding, patterns: &[String]) -> bool {
    let hay = format!("{} {}", f.id, f.title).to_lowercase();
    patterns.iter().any(|p| hay.contains(p.as_str()))
}

/// (kept, ignored).
pub fn split_ignored(findings: Vec<Finding>) -> (Vec<Finding>, Vec<Finding>) {
    let pats = ignores();
    findings.into_iter().partition(|f| !is_ignored(f, &pats))
}

/// Remove every ignore pattern that matches this finding; returns how many.
#[cfg(feature = "gui")]
pub fn unignore(f: &Finding) -> io::Result<usize> {
    let mut n = 0;
    for p in ignores().iter().filter(|p| is_ignored(f, std::slice::from_ref(p))) {
        n += remove_ignore(p)? as usize;
    }
    Ok(n)
}

/// Drop ignored findings; returns how many were dropped.
pub fn apply_ignores(findings: &mut Vec<Finding>) -> usize {
    let pats = ignores();
    let before = findings.len();
    findings.retain(|f| !is_ignored(f, &pats));
    before - findings.len()
}

pub fn add_ignore(pattern: &str) -> io::Result<()> {
    let p = ignore_path().ok_or(io::ErrorKind::NotFound)?;
    fs::create_dir_all(p.parent().unwrap())?;
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&p)?;
    writeln!(f, "{}", pattern.trim())
}

/// Remove lines equal (case-insensitive) to pattern; returns whether anything was removed.
pub fn remove_ignore(pattern: &str) -> io::Result<bool> {
    let p = ignore_path().ok_or(io::ErrorKind::NotFound)?;
    let text = fs::read_to_string(&p).unwrap_or_default();
    let keep: Vec<&str> = text.lines().filter(|l| !l.trim().eq_ignore_ascii_case(pattern.trim())).collect();
    let removed = keep.len() != text.lines().count();
    if removed {
        fs::write(&p, keep.join("\n") + "\n")?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::Severity;

    #[test]
    fn ignore_matching() {
        let f = Finding::new("failed-unit", Severity::Warn, "system unit casper-md5check.service failed");
        assert!(is_ignored(&f, &["casper-md5check".into()]));
        assert!(is_ignored(&f, &["failed-unit".into()]));
        assert!(!is_ignored(&f, &["battery".into()]));
        assert!(is_ignored(&f, std::slice::from_ref(&"casper".to_string())));
    }

    #[test]
    fn config_partial_and_unknown() {
        let c: Config = toml::from_str("disk_warn = 85\n").unwrap();
        assert_eq!((c.disk_warn, c.disk_crit), (85.0, 97.0));
        assert!(toml::from_str::<Config>("disk_wrn = 85\n").is_err()); // typos are reported, not silently ignored
    }
}
