//! Updates, kernel, file permissions, secrets in shell history, key/cert expiry.
use crate::config;
use crate::finding::{run, run_any, Finding, Severity};
use crate::health::{epoch, now};
use std::cmp::Ordering;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

// ---- updates and kernel ----

/// (total upgradable, security) from `apt-get -s upgrade` output.
pub fn count_updates(sim: &str) -> (usize, usize) {
    let inst: Vec<&str> = sim.lines().filter(|l| l.starts_with("Inst ")).collect();
    (inst.len(), inst.iter().filter(|l| l.contains("-security")).count())
}

pub fn updates() -> Vec<Finding> {
    let Some((n, sec, cmd)) = crate::pkg::updates() else { return vec![] };
    match (n, sec) {
        (0, _) => vec![Finding::ok("updates", "System up to date")],
        (n, Some(0)) => vec![Finding::ok("updates", format!("{n} updates available (none security)"))],
        // ponytail: pacman has no security metadata; arch-audit could add it
        (n, None) => vec![Finding::ok("updates", format!("{n} updates available"))],
        (n, Some(s)) => vec![Finding::new("updates", Severity::Warn, format!("{s} security updates pending ({n} total)"))
            .detail("Based on the last metadata refresh; may be stale.")
            .cmd(cmd)],
    }
}

/// Version-aware ordering for kernel strings like "6.8.0-149-generic".
fn vercmp(a: &str, b: &str) -> Ordering {
    fn parts(s: &str) -> Vec<u64> {
        s.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect()
    }
    parts(a).cmp(&parts(b))
}

/// Kernel versions with an image in /boot.
pub fn installed_kernels() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir("/boot")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix("vmlinuz-").map(String::from))
        .collect();
    v.sort_by(|a, b| vercmp(a, b));
    v
}

pub fn kernel() -> Vec<Finding> {
    let Some(running) = run("uname", &["-r"]).map(|s| s.trim().to_string()) else { return vec![] };
    let Some(newest) = installed_kernels().pop() else { return vec![] };
    if vercmp(&newest, &running) == Ordering::Greater && crate::pkg::reboot_needed() != Some(true) {
        return vec![Finding::new("kernel", Severity::Warn, format!("Running kernel {running}, newer {newest} installed"))
            .detail("Security fixes in the new kernel apply only after a reboot.")
            .cmd("sudo reboot")];
    }
    vec![]
}

// ---- permissions ----

fn mode(p: &Path) -> Option<u32> {
    fs::metadata(p).ok().map(|m| m.permissions().mode() & 0o777)
}

fn is_private_key(p: &Path) -> bool {
    let mut head = [0u8; 64];
    let n = fs::File::open(p).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    String::from_utf8_lossy(&head[..n]).contains("PRIVATE KEY")
}

pub fn permissions() -> Vec<Finding> {
    let h = home();
    let mut out = vec![];
    for (dir, name) in [(h.join(".ssh"), "~/.ssh"), (h.join(".gnupg"), "~/.gnupg")] {
        if let Some(m) = mode(&dir).filter(|m| m & 0o077 != 0) {
            out.push(Finding::new("permissions", Severity::Warn, format!("{name} is accessible by others (mode {m:o})"))
                .cmd(format!("chmod 700 {name}")));
        }
    }
    for e in fs::read_dir(h.join(".ssh")).into_iter().flatten().flatten() {
        let p = e.path();
        if let Some(m) = mode(&p).filter(|m| m & 0o077 != 0 && p.is_file() && is_private_key(&p)) {
            let name = format!("~/.ssh/{}", e.file_name().to_string_lossy());
            out.push(Finding::new("permissions", Severity::Crit, format!("Private key {name} readable by others (mode {m:o})"))
                .detail("Other local users can copy it. ssh may also refuse to use it.")
                .cmd(format!("chmod 600 ~/.ssh/{}", crate::finding::sh_quote(&e.file_name().to_string_lossy()))));
        }
    }
    if out.is_empty() {
        out.push(Finding::ok("permissions", "SSH/GPG permissions OK"));
    }
    out
}

// ---- secrets in shell history ----

/// (prefix, minimum token chars after prefix, label). Only well-known, high-confidence formats.
const TOKENS: &[(&str, usize, &str)] = &[
    ("ghp_", 36, "GitHub token"),
    ("gho_", 36, "GitHub token"),
    ("github_pat_", 50, "GitHub token"),
    ("glpat-", 20, "GitLab token"),
    ("AKIA", 16, "AWS access key"),
    ("xoxb-", 20, "Slack token"),
    ("xoxp-", 20, "Slack token"),
    ("sk-ant-", 30, "Anthropic API key"),
    ("sk-proj-", 30, "OpenAI API key"),
    ("hf_", 30, "Hugging Face token"),
    ("npm_", 36, "npm token"),
    ("AIza", 35, "Google API key"),
];

/// First secret-looking token in a line: (label, sed ERE matching lines with that token format).
/// Never returns the secret itself.
fn find_secret(line: &str) -> Option<(&'static str, String)> {
    if line.contains("PRIVATE KEY-----") {
        return Some(("private key", "PRIVATE KEY-----".into()));
    }
    let is_tok = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    for &(prefix, min, label) in TOKENS {
        for (i, _) in line.match_indices(prefix) {
            let boundary = line[..i].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric());
            let tail = line[i + prefix.len()..].chars().take_while(|&c| is_tok(c)).count();
            if boundary && tail >= min {
                // same rule as above: no letter/digit before the prefix, at least `min` token chars after
                return Some((label, format!("(^|[^A-Za-z0-9]){prefix}[A-Za-z0-9_-]{{{min}}}")));
            }
        }
    }
    None
}

pub fn history_secrets() -> Vec<Finding> {
    let h = home();
    let files = [".bash_history", ".zsh_history", ".local/share/fish/fish_history", ".python_history", ".psql_history", ".mysql_history"];
    let mut out = vec![];
    for rel in files {
        let p = h.join(rel);
        let Ok(bytes) = fs::read(&p) else { continue };
        let text = String::from_utf8_lossy(&bytes); // zsh history can contain non-UTF8 metafied bytes
        let hits: Vec<(usize, (&str, String))> = text.lines().enumerate().filter_map(|(i, l)| Some((i + 1, find_secret(l)?))).collect();
        if hits.is_empty() {
            continue;
        }
        let mut labels: Vec<&str> = hits.iter().map(|(_, (l, _))| *l).collect();
        labels.sort();
        labels.dedup();
        let mut res: Vec<&str> = hits.iter().map(|(_, (_, r))| r.as_str()).collect();
        res.sort();
        res.dedup();
        let lines: Vec<String> = hits.iter().map(|(n, _)| n.to_string()).collect();
        out.push(
            Finding::new("secrets", Severity::Warn, format!("~/{rel}: {} secret(s) in history ({})", hits.len(), labels.join(", ")))
                .detail(format!(
                    "Lines {}. Revoke/rotate them first: history files get backed up and synced. Open shells may write their history back on exit.",
                    lines.join(", ")
                ))
                .cmd(delete_lines_cmd(&format!("~/{rel}"), &res)),
        );
    }
    if out.is_empty() {
        out.push(Finding::ok("secrets", "No known token formats in shell history"));
    }
    out
}

/// Delete every line matching one of these EREs. By pattern, not line number: shells rewrite
/// history files (dedup, shared history) between the scan and the click. LC_ALL=C: zsh history
/// holds non-UTF-8 bytes. The patterns describe token formats and never contain a secret.
fn delete_lines_cmd(file: &str, res: &[&str]) -> String {
    format!("LC_ALL=C sed -i -E {} {file}", res.iter().map(|r| format!("-e '/{r}/d'")).collect::<Vec<_>>().join(" "))
}

// ---- expiry ----

fn expiry(what: String, at: u64) -> Option<Finding> {
    let days = (at as i64 - now() as i64) / 86400;
    let warn = config::get().expiry_warn_days as i64;
    match days {
        d if d < -30 => None, // long expired: presumably intentional
        d if d < 0 => Some(Finding::new("expiry", Severity::Crit, format!("{what} expired {} days ago", -d))),
        d if d <= warn => Some(Finding::new("expiry", Severity::Warn, format!("{what} expires in {d} days"))),
        _ => None,
    }
}

/// (description, expiry epoch) for own GPG keys from `gpg --list-secret-keys --with-colons`.
fn gpg_expiries(colons: &str) -> Vec<(String, u64)> {
    let mut out = vec![];
    let mut uid = String::new();
    let mut pending: Vec<(String, u64)> = vec![];
    for l in colons.lines() {
        let f: Vec<&str> = l.split(':').collect();
        match f.first() {
            Some(&"sec") | Some(&"ssb") => {
                if f.first() == Some(&"sec") {
                    out.extend(pending.drain(..).map(|(k, t)| (format!("GPG key {k} ({uid})"), t)));
                    uid.clear();
                }
                if f.get(1) == Some(&"r") {
                    continue; // revoked
                }
                if let Some(t) = f.get(6).and_then(|s| s.parse::<u64>().ok()) {
                    pending.push((f.get(4).unwrap_or(&"?").to_string(), t));
                }
            }
            Some(&"uid") if uid.is_empty() => uid = f.get(9).unwrap_or(&"").to_string(),
            _ => {}
        }
    }
    out.extend(pending.drain(..).map(|(k, t)| (format!("GPG key {k} ({uid})"), t)));
    out
}

pub fn expiries() -> Vec<Finding> {
    let mut items: Vec<(String, u64)> = vec![];
    if let Some(s) = run("gpg", &["--list-secret-keys", "--with-colons"]) {
        items.extend(gpg_expiries(&s));
    }
    for e in fs::read_dir(home().join(".ssh")).into_iter().flatten().flatten() {
        let p = e.path();
        if !p.to_string_lossy().ends_with("-cert.pub") {
            continue;
        }
        let Some(info) = run("ssh-keygen", &["-L", "-f", &p.to_string_lossy()]) else { continue };
        // "Valid: from 2025-01-01T00:00:00 to 2025-02-01T00:00:00" (or "forever")
        if let Some(t) = info.lines().find_map(|l| l.trim().strip_prefix("Valid: from ")?.split(" to ").nth(1)).and_then(epoch) {
            items.push((format!("SSH certificate {}", e.file_name().to_string_lossy()), t));
        }
    }
    let mut pems: Vec<PathBuf> = config::get().cert_paths.clone();
    pems.extend(fs::read_dir("/etc/letsencrypt/live").into_iter().flatten().flatten().map(|e| e.path().join("cert.pem")));
    for p in pems {
        let Some(out) = run_any("openssl", &["x509", "-enddate", "-noout", "-in", &p.to_string_lossy()]) else { continue };
        if let Some(t) = out.trim().strip_prefix("notAfter=").and_then(epoch) {
            items.push((format!("Certificate {}", p.display()), t));
        }
    }
    let out: Vec<Finding> = items.into_iter().filter_map(|(w, t)| expiry(w, t)).collect();
    if out.is_empty() {
        vec![Finding::ok("expiry", "No keys or certificates expiring soon")]
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_count() {
        let sim = "NOTE: sim\nInst libc6 [2.39-0ubuntu8.3] (2.39-0ubuntu8.4 Ubuntu:24.04/noble-security [amd64])\n\
                   Inst vim [2:9.1] (2:9.1.1 Ubuntu:24.04/noble-updates [amd64])\nConf libc6 (2.39)\n";
        assert_eq!(count_updates(sim), (2, 1));
    }

    #[test]
    fn kernel_versions() {
        assert_eq!(vercmp("6.8.0-149-generic", "6.8.0-38-generic"), Ordering::Greater);
        assert_eq!(vercmp("6.10.0-1", "6.9.0-99"), Ordering::Greater);
        assert_eq!(vercmp("6.8.0-1-generic", "6.8.0-1-generic"), Ordering::Equal);
    }

    #[test]
    fn secrets() {
        let label = |l: &str| find_secret(l).map(|(l, _)| l);
        let gh = format!("export GITHUB_TOKEN=ghp_{}", "a".repeat(36));
        assert_eq!(label(&gh), Some("GitHub token"));
        assert_eq!(label(&format!("curl -H 'x-api-key: sk-ant-{}'", "b".repeat(40))), Some("Anthropic API key"));
        assert_eq!(label("echo ghp_short"), None); // too short
        assert_eq!(label(&format!("myghp_{}", "a".repeat(36))), None); // no word boundary
        assert_eq!(label("git commit -m 'fix AKIA docs'"), None);
        assert_eq!(label("ls -la"), None);
    }

    /// The generated sed deletes exactly the lines the scan flags, wherever they moved to.
    #[test]
    fn secret_removal() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("hist");
        let lines = [
            format!("export T=ghp_{}", "a".repeat(36)),
            "echo ghp_short".to_string(),
            format!("myghp_{}", "a".repeat(36)),
            format!("aws AKIA{} x", "Z".repeat(16)),
            "-----BEGIN OPENSSH PRIVATE KEY-----".to_string(),
        ];
        // plus a non-UTF-8 line, as zsh's metafied history has
        fs::write(&f, [(lines.join("\n") + "\n").as_bytes(), b"ls \xff\n"].concat()).unwrap();
        let mut res: Vec<String> = lines.iter().filter_map(|l| Some(find_secret(l)?.1)).collect();
        res.dedup();
        let res: Vec<&str> = res.iter().map(String::as_str).collect();
        let cmd = delete_lines_cmd(&crate::finding::sh_quote(&f.to_string_lossy()), &res);
        assert!(std::process::Command::new("sh").args(["-c", &cmd]).status().unwrap().success());
        let want = format!("echo ghp_short\nmyghp_{}\nls \u{fffd}\n", "a".repeat(36));
        assert_eq!(String::from_utf8_lossy(&fs::read(&f).unwrap()), want);
    }

    #[test]
    fn gpg_parse() {
        let c = "sec:u:255:22:AAAA1111:1600000000:1900000000::u:::scESC:::+:::ed25519:::0:\n\
                 fpr:::::::::X:\nuid:u::::1600000000::H::Alice <a@x>::::::::::0:\n\
                 ssb:u:255:18:BBBB2222:1600000000:1950000000:::::e:::+:::cv25519::\n\
                 sec:r:255:22:CCCC3333:1500000000:1700000000::u:::sc:::+:::ed25519:::0:\n\
                 uid:r::::1500000000::H::Old <o@x>::::::::::0:\n";
        assert_eq!(
            gpg_expiries(c),
            [("GPG key AAAA1111 (Alice <a@x>)".to_string(), 1900000000), ("GPG key BBBB2222 (Alice <a@x>)".to_string(), 1950000000)]
        );
    }
}
