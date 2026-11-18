use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Ok,
    Warn,
    Crit,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "lowercase", tag = "kind", content = "value")]
pub enum Fix {
    Command(String),
    Trash(Vec<PathBuf>),
}

#[derive(Serialize, Debug)]
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<Fix>,
}

impl Finding {
    pub fn new(id: &'static str, severity: Severity, title: impl Into<String>) -> Self {
        Finding { id, severity, title: title.into(), detail: String::new(), bytes: None, fix: None }
    }
    pub fn ok(id: &'static str, title: impl Into<String>) -> Self {
        Self::new(id, Severity::Ok, title)
    }
    pub fn skipped(id: &'static str, reason: impl Into<String>) -> Self {
        Self::new(id, Severity::Ok, format!("skipped: {}", reason.into()))
    }
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = d.into();
        self
    }
    pub fn cmd(mut self, c: impl Into<String>) -> Self {
        self.fix = Some(Fix::Command(c.into()));
        self
    }
}

/// Run a command; stdout on success, None if missing or failed.
pub fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Like `run` but returns stdout regardless of exit status (for tools that exit non-zero on "inactive").
pub fn run_any(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// XDG base dir: $var if set and absolute, else $HOME/fallback.
pub fn xdg(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(fallback)))
}

pub fn human(bytes: u64) -> String {
    let units = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{bytes}B") } else { format!("{v:.1}{}", units[i]) }
}

/// Parse sizes like "1.2G", "512.0M", "3.4GB", "800 kB" into bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult = match unit.trim().chars().next()?.to_ascii_uppercase() {
        'B' => 1.0,
        'K' => 1024.0,
        'M' => 1024.0f64.powi(2),
        'G' => 1024.0f64.powi(3),
        'T' => 1024.0f64.powi(4),
        _ => return None,
    };
    Some((n * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sizes() {
        assert_eq!(parse_size("1.5G"), Some(1610612736));
        assert_eq!(parse_size("512M"), Some(536870912));
        assert_eq!(parse_size("3.4GB"), Some(3650722201));
        assert_eq!(parse_size("10B"), Some(10));
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("abc"), None);
        assert_eq!(human(1536), "1.5K");
        assert_eq!(human(10), "10B");
    }
}
