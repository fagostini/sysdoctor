mod check;
mod clean;
mod finding;
mod watch;

use clap::{Parser, Subcommand};
use finding::{human, Finding, Fix, Severity};
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

/// System health check and cleanup advisor.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Machine-readable output
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Health report. Exit code: 0 ok, 1 warn, 2 crit.
    Check {
        /// One-line JSON for status bars (waybar format: text, tooltip, class); always exits 0
        #[arg(long)]
        bar: bool,
    },
    /// List reclaimable space (dry run by default)
    Clean {
        /// Move listed regenerable user paths to the trash (asks first)
        #[arg(long)]
        apply: bool,
    },
    /// Run checks, desktop-notify only new problems, record daily size snapshot (for timers)
    Watch {
        /// Install and enable an hourly systemd user timer
        #[arg(long)]
        install: bool,
        /// Silence notifications for this many hours (0 = resume)
        #[arg(long, value_name = "HOURS")]
        snooze: Option<u64>,
    },
    /// Show what grew on disk
    Growth {
        /// Compare against a snapshot at least this many days old
        #[arg(long, default_value_t = 7)]
        days: u64,
    },
    /// Internal: show a notification with buttons and handle the click
    #[command(hide = true)]
    Notify {
        #[arg(long)]
        urgency: String,
        title: String,
        body: String,
    },
}

/// Status bar summary: icon + problem count, problems as tooltip, worst severity as CSS class.
fn bar(findings: &[Finding]) -> serde_json::Value {
    let problems: Vec<&Finding> = findings.iter().filter(|f| f.severity != Severity::Ok).collect();
    let worst = problems.iter().map(|f| f.severity).max().unwrap_or(Severity::Ok);
    let (icon, class) = match worst {
        Severity::Ok => ("✓", "ok"),
        Severity::Warn => ("⚠", "warn"),
        Severity::Crit => ("✖", "crit"),
    };
    let text = if problems.is_empty() { icon.to_string() } else { format!("{icon} {}", problems.len()) };
    let tooltip = if problems.is_empty() {
        "sysdoctor: all checks ok".to_string()
    } else {
        problems.iter().map(|f| f.title.as_str()).collect::<Vec<_>>().join("\n")
    };
    serde_json::json!({ "text": text, "tooltip": tooltip, "class": class, "alt": class })
}

fn paint(sev: Severity, color: bool) -> &'static str {
    match (sev, color) {
        (Severity::Ok, true) => "\x1b[32m  ok\x1b[0m",
        (Severity::Warn, true) => "\x1b[33mwarn\x1b[0m",
        (Severity::Crit, true) => "\x1b[1;31mcrit\x1b[0m",
        (Severity::Ok, false) => "  ok",
        (Severity::Warn, false) => "warn",
        (Severity::Crit, false) => "crit",
    }
}

fn render(findings: &[Finding]) {
    let color = std::io::stdout().is_terminal();
    for f in findings {
        let size = f.bytes.map(|b| format!("{:>8}  ", human(b))).unwrap_or_default();
        println!("[{}] {size}{}", paint(f.severity, color), f.title);
        for l in f.detail.lines() {
            println!("         {l}");
        }
        match &f.fix {
            Some(Fix::Command(c)) => println!("         fix: {c}"),
            Some(Fix::Trash(_)) => println!("         fix: sysdoctor clean --apply"),
            None => {}
        }
    }
}

fn confirm(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim().eq_ignore_ascii_case("y")
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        eprintln!("HOME not set");
        return ExitCode::from(2);
    };

    let findings: Vec<Finding> = match cli.cmd {
        Cmd::Check { bar: true } => {
            let f: Vec<Finding> = check::CHECKS.iter().flat_map(|c| c()).collect();
            println!("{}", bar(&f));
            return ExitCode::SUCCESS; // bars treat non-zero exit as module failure
        }
        Cmd::Check { bar: false } => check::CHECKS.iter().flat_map(|c| c()).collect(),
        Cmd::Clean { .. } => {
            let mut v: Vec<Finding> = clean::SCANNERS.iter().flat_map(|s| s(&home)).collect();
            v.sort_by_key(|f| std::cmp::Reverse(f.bytes.unwrap_or(0)));
            v
        }
        Cmd::Notify { ref urgency, ref title, ref body } => {
            watch::notify_interactive(urgency, title, body);
            return ExitCode::SUCCESS;
        }
        Cmd::Watch { install, snooze } => {
            let r = if install {
                watch::install()
            } else if let Some(h) = snooze {
                watch::snooze(h)
            } else {
                let f: Vec<Finding> = check::CHECKS.iter().flat_map(|c| c()).collect();
                let w = watch::watch(&f);
                // growth snapshot runs even if notification state failed
                w.and(watch::snapshot(&home).map(|_| ()))
            };
            // Always exit 0: a failing unit would report itself as a "failed unit" finding.
            if let Err(e) = r {
                eprintln!("sysdoctor watch: {e}");
            }
            return ExitCode::SUCCESS;
        }
        Cmd::Growth { days } => {
            return match watch::growth(&home, days) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("growth: {e}");
                    ExitCode::from(2)
                }
            };
        }
    };

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&findings).unwrap());
    } else {
        render(&findings);
    }

    match cli.cmd {
        Cmd::Watch { .. } | Cmd::Growth { .. } | Cmd::Notify { .. } | Cmd::Check { bar: true } => unreachable!("returned above"),
        Cmd::Check { bar: false } => ExitCode::from(findings.iter().map(|f| f.severity).max().unwrap_or(Severity::Ok) as u8),
        Cmd::Clean { apply } => {
            if !cli.json {
                let total: u64 = findings.iter().filter_map(|f| f.bytes).sum();
                println!("\nreclaimable: {}", human(total));
            }
            if !apply {
                return ExitCode::SUCCESS;
            }
            // Only regenerable user paths are auto-trashed; "review" items and system commands never are.
            let paths: Vec<&PathBuf> = findings
                .iter()
                .filter(|f| f.severity == Severity::Ok)
                .filter_map(|f| match &f.fix {
                    Some(Fix::Trash(p)) => Some(p),
                    _ => None,
                })
                .flatten()
                .collect();
            if paths.is_empty() {
                println!("Nothing to trash automatically. Run the fix commands above yourself.");
                return ExitCode::SUCCESS;
            }
            println!("\nWill move to trash:");
            paths.iter().for_each(|p| println!("  {}", p.display()));
            if !confirm("Proceed?") {
                return ExitCode::SUCCESS;
            }
            match trash::delete_all(&paths) {
                Ok(()) => {
                    println!("Trashed {} paths. Restore from trash if needed.", paths.len());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("trash failed: {e}");
                    ExitCode::from(2)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_output() {
        let ok = [Finding::ok("ntp", "Clock synced")];
        assert_eq!(bar(&ok)["text"], "✓");
        assert_eq!(bar(&ok)["class"], "ok");
        let mixed = [
            Finding::ok("ntp", "Clock synced"),
            Finding::new("battery", Severity::Warn, "BAT1 health 79%"),
            Finding::new("smart", Severity::Crit, "/dev/sda SMART health FAILED"),
        ];
        let b = bar(&mixed);
        assert_eq!((b["text"].as_str(), b["class"].as_str()), (Some("✖ 2"), Some("crit")));
        assert_eq!(b["tooltip"], "BAT1 health 79%\n/dev/sda SMART health FAILED");
    }
}
