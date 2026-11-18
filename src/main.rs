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
    Check,
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
    },
    /// Show what grew on disk
    Growth {
        /// Compare against a snapshot at least this many days old
        #[arg(long, default_value_t = 7)]
        days: u64,
    },
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
        Cmd::Check => check::CHECKS.iter().flat_map(|c| c()).collect(),
        Cmd::Clean { .. } => {
            let mut v: Vec<Finding> = clean::SCANNERS.iter().flat_map(|s| s(&home)).collect();
            v.sort_by_key(|f| std::cmp::Reverse(f.bytes.unwrap_or(0)));
            v
        }
        Cmd::Watch { install } => {
            let r = if install {
                watch::install()
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
        Cmd::Watch { .. } | Cmd::Growth { .. } => unreachable!("returned above"),
        Cmd::Check => ExitCode::from(findings.iter().map(|f| f.severity).max().unwrap_or(Severity::Ok) as u8),
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
