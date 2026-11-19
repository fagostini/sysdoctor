mod check;
mod clean;
mod config;
mod finding;
#[cfg(feature = "gui")]
mod gui;
mod health;
mod pkg;
mod report;
mod security;
mod tray;
mod watch;

use clap::{CommandFactory, Parser, Subcommand};
use finding::{human, printable, Finding, Fix, Severity};
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

/// System health check and cleanup advisor.
#[derive(Parser)]
#[command(version = finding::VERSION)]
struct Cli {
    /// Machine-readable output (check, clean, startup)
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
        /// Offer to run each problem's fix command, one by one
        #[arg(long, conflicts_with = "bar")]
        fix: bool,
    },
    /// List reclaimable space (dry run by default)
    Clean {
        /// Trash regenerable user paths, then offer each cleanup command (asks before every step)
        #[arg(long)]
        apply: bool,
    },
    /// Run checks, desktop-notify only new problems, record daily snapshots (for timers)
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
    /// Show battery health and disk wear over time
    Trends,
    /// Privacy-scrubbed Markdown summary to paste into a forum or bug report
    Report,
    /// What slows boot, login and new terminals
    Startup,
    /// Mute findings whose id or title contains PATTERN (no argument: list)
    Ignore {
        pattern: Option<String>,
        /// Remove PATTERN from the ignore list instead
        #[arg(long, requires = "pattern")]
        remove: bool,
    },
    /// Show effective configuration and where to override it
    Config,
    /// Status icon in the panel tray, refreshed every 15 minutes
    Tray {
        /// Start it at every login (XDG autostart) and launch it now
        #[arg(long)]
        install: bool,
    },
    /// Window with the full report, fix buttons and cleanup (needs the `gui` build feature)
    Gui,
    /// Print shell completions
    Completions { shell: clap_complete::Shell },
    /// Print the man page (roff)
    #[command(hide = true)]
    Man,
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
            Some(Fix::Command(c)) => println!("         fix: {}", printable(c)),
            Some(Fix::Trash(_)) => println!("         fix: sysdoctor clean --apply"),
            None => {}
        }
    }
}

fn output(findings: &[Finding], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(findings).unwrap());
    } else {
        render(findings);
    }
}

fn confirm(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim().eq_ignore_ascii_case("y")
}

/// Offer each fix command, one confirmation per command; runs through `sh -c` so sudo can prompt.
fn run_fixes<'a>(findings: impl Iterator<Item = &'a Finding>) {
    if !std::io::stdin().is_terminal() {
        eprintln!("Fixes need an interactive terminal to confirm each command.");
        return;
    }
    for f in findings {
        let Some(Fix::Command(cmd)) = &f.fix else { continue };
        println!("\n{}", f.title);
        if !confirm(&format!("  Run `{}`?", printable(cmd))) {
            continue;
        }
        match Command::new("sh").args(["-c", cmd]).status() {
            Ok(s) if s.success() => println!("  done"),
            Ok(s) => println!("  exited with {s}"),
            Err(e) => println!("  failed to start: {e}"),
        }
    }
}

/// All checks, split into (active, ignored).
fn checks() -> (Vec<Finding>, Vec<Finding>) {
    config::split_ignored(check::CHECKS.iter().flat_map(|c| c()).collect())
}

/// Cleanup findings minus ignored ones, biggest first.
fn cleanups() -> Vec<Finding> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return vec![] };
    // scanners are I/O bound and independent: run them concurrently
    let mut f: Vec<Finding> = std::thread::scope(|sc| {
        let hs: Vec<_> = clean::SCANNERS.iter().map(|s| sc.spawn(|| s(&home))).collect();
        hs.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    config::apply_ignores(&mut f);
    f.sort_by_key(|f| std::cmp::Reverse(f.bytes.unwrap_or(0)));
    f
}

fn fail(what: &str, e: impl std::fmt::Display) -> ExitCode {
    eprintln!("{what}: {e}");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        eprintln!("HOME not set");
        return ExitCode::from(2);
    };

    match cli.cmd {
        Cmd::Check { bar: true, .. } => {
            println!("{}", bar(&checks().0));
            ExitCode::SUCCESS // bars treat non-zero exit as module failure
        }
        Cmd::Check { bar: false, fix } => {
            let (f, ignored) = checks();
            output(&f, cli.json);
            if !ignored.is_empty() && !cli.json {
                println!("\n({} ignored, see `sysdoctor ignore`)", ignored.len());
            }
            if fix {
                run_fixes(f.iter().filter(|f| f.severity != Severity::Ok));
            }
            ExitCode::from(f.iter().map(|f| f.severity).max().unwrap_or(Severity::Ok) as u8)
        }
        Cmd::Clean { apply } => {
            let f = cleanups();
            output(&f, cli.json);
            if !cli.json {
                println!("\nreclaimable: {}", human(f.iter().filter_map(|f| f.bytes).sum()));
            }
            if apply { clean_apply(&f) } else { ExitCode::SUCCESS }
        }
        Cmd::Notify { ref urgency, ref title, ref body } => {
            watch::notify_interactive(urgency, title, body);
            ExitCode::SUCCESS
        }
        Cmd::Watch { install, snooze } => {
            let r = if install {
                watch::install()
            } else if let Some(h) = snooze {
                watch::snooze(h)
            } else {
                // each step runs even if an earlier one failed
                let w = watch::watch(&checks().0);
                let g = watch::snapshot(&home).map(|_| ());
                let t = watch::record_metrics();
                w.and(g).and(t)
            };
            // Always exit 0: a failing unit would report itself as a "failed unit" finding.
            if let Err(e) = r {
                eprintln!("sysdoctor watch: {e}");
            }
            ExitCode::SUCCESS
        }
        Cmd::Growth { days } => watch::growth(&home, days).map_or_else(|e| fail("growth", e), |_| ExitCode::SUCCESS),
        Cmd::Trends => watch::trends().map_or_else(|e| fail("trends", e), |_| ExitCode::SUCCESS),
        Cmd::Report => {
            print!("{}", report::report(&checks().0));
            ExitCode::SUCCESS
        }
        Cmd::Startup => {
            output(&report::startup(), cli.json);
            ExitCode::SUCCESS
        }
        Cmd::Ignore { pattern: None, .. } => {
            let pats = config::ignores();
            if pats.is_empty() {
                println!("Nothing ignored. Add with `sysdoctor ignore <pattern>`.");
            }
            pats.iter().for_each(|p| println!("{p}"));
            ExitCode::SUCCESS
        }
        Cmd::Ignore { pattern: Some(p), remove: false } => match config::add_ignore(&p) {
            Ok(()) => {
                println!("Ignoring findings matching \"{p}\".");
                ExitCode::SUCCESS
            }
            Err(e) => fail("ignore", e),
        },
        Cmd::Ignore { pattern: Some(p), remove: true } => match config::remove_ignore(&p) {
            Ok(true) => {
                println!("No longer ignoring \"{p}\".");
                ExitCode::SUCCESS
            }
            Ok(false) => fail("ignore", format!("\"{p}\" is not in the ignore list")),
            Err(e) => fail("ignore", e),
        },
        Cmd::Config => {
            let path = config::path().unwrap_or_default();
            let state = if path.exists() { "loaded from" } else { "defaults; override in" };
            println!("# {state} {}\n", path.display());
            print!("{}", toml::to_string(config::get()).unwrap_or_default());
            ExitCode::SUCCESS
        }
        #[cfg(feature = "gui")]
        Cmd::Gui => ExitCode::from(gui::run(checks, cleanups) as u8),
        #[cfg(not(feature = "gui"))]
        Cmd::Gui => fail("gui", "built without the `gui` feature; reinstall with `cargo install --path . --features gui`"),
        Cmd::Tray { install } => {
            let r = if install { tray::install() } else { tray::run(checks) };
            r.map_or_else(|e| fail("tray", e), |_| ExitCode::SUCCESS)
        }
        Cmd::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "sysdoctor", &mut std::io::stdout());
            ExitCode::SUCCESS
        }
        Cmd::Man => clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout()).map_or_else(|e| fail("man", e), |_| ExitCode::SUCCESS),
    }
}

/// Step 1: trash regenerable user paths (one confirmation). Step 2: offer each cleanup command.
fn clean_apply(findings: &[Finding]) -> ExitCode {
    // "review" items (Warn) are never auto-trashed; their commands are offered in step 2.
    let paths: Vec<&PathBuf> = findings
        .iter()
        .filter(|f| f.severity == Severity::Ok)
        .filter_map(|f| match &f.fix {
            Some(Fix::Trash(p)) => Some(p),
            _ => None,
        })
        .flatten()
        .collect();
    if !paths.is_empty() {
        println!("\nWill move to trash:");
        paths.iter().for_each(|p| println!("  {}", printable(&p.to_string_lossy())));
        if confirm("Proceed?") {
            match trash::delete_all(&paths) {
                Ok(()) => println!("Trashed {} paths. Empty the trash to actually free the space.", paths.len()),
                Err(e) => return fail("trash failed", e),
            }
        }
    }
    run_fixes(findings.iter());
    ExitCode::SUCCESS
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
