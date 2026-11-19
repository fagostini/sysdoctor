//! `sysdoctor tray`: StatusNotifierItem icon (KDE, Cinnamon via xapp-sn-watcher, XFCE; GNOME needs AppIndicator).
use crate::finding::{xdg, Finding, Severity};
use crate::watch::{open_in_terminal, snooze};
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};
use std::fs;
use std::io;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::time::{Duration, SystemTime};

const REFRESH: Duration = Duration::from_secs(15 * 60);

struct SysTray {
    problems: Vec<(Severity, String)>,
    ignored: usize,
    checked: Option<SystemTime>,
    busy: bool,
    refresh: Sender<()>,
}

impl SysTray {
    fn worst(&self) -> Severity {
        self.problems.iter().map(|p| p.0).max().unwrap_or(Severity::Ok)
    }
    fn summary(&self) -> String {
        match (self.checked, self.problems.len()) {
            (None, _) => "Checking…".into(),
            (_, 0) => "All checks OK".into(),
            (_, 1) => "1 problem".into(),
            (_, n) => format!("{n} problems"),
        }
    }
}

/// 32x32 ARGB32 (network byte order) filled circle with a darker rim.
pub fn dot(rgb: [u8; 3]) -> ksni::Icon {
    let n = 32i32;
    let mut data = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let r = (dx * dx + dy * dy).sqrt();
            let px = if r <= 11.0 {
                [255, rgb[0], rgb[1], rgb[2]]
            } else if r <= 13.0 {
                [255, rgb[0] / 2, rgb[1] / 2, rgb[2] / 2]
            } else {
                [0, 0, 0, 0]
            };
            data.extend_from_slice(&px);
        }
    }
    ksni::Icon { width: n, height: n, data }
}

fn hhmm(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0).to_string();
    crate::finding::run("date", &["-d", &format!("@{secs}"), "+%H:%M"]).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn item(label: impl Into<String>, enabled: bool, f: impl Fn(&mut SysTray) + Send + Sync + 'static) -> MenuItem<SysTray> {
    StandardItem { label: label.into(), enabled, activate: Box::new(f), ..Default::default() }.into()
}

fn terminal(args: &'static str) -> impl Fn(&mut SysTray) + Send + Sync + 'static {
    // own thread: the terminal stays open until the user closes it
    move |_| {
        std::thread::spawn(move || open_in_terminal(args));
    }
}

impl ksni::Tray for SysTray {
    fn id(&self) -> String {
        "sysdoctor".into()
    }
    fn title(&self) -> String {
        format!("sysdoctor: {}", self.summary())
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![match (self.checked, self.worst()) {
            (None, _) => dot([0x9a, 0x99, 0x96]),
            (_, Severity::Ok) => dot([0x33, 0xd1, 0x7a]),
            (_, Severity::Warn) => dot([0xf6, 0xd3, 0x2d]),
            (_, Severity::Crit) => dot([0xe0, 0x1b, 0x24]),
        }]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        let body = self.problems.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join("\n");
        ksni::ToolTip { title: format!("sysdoctor: {}", self.summary()), description: body, ..Default::default() }
    }
    /// Left click: full report.
    fn activate(&mut self, _x: i32, _y: i32) {
        std::thread::spawn(|| open_in_terminal("check"));
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut m = vec![item(self.summary(), false, |_| {})];
        for (sev, title) in &self.problems {
            let mark = if *sev == Severity::Crit { "✖" } else { "⚠" };
            m.push(item(format!("{mark}  {title}"), true, terminal("check")));
        }
        m.push(MenuItem::Separator);
        m.push(item("Open full report", true, terminal("check")));
        m.push(item("Fix problems…", true, terminal("check --fix")));
        m.push(item("Clean up (preview)", true, terminal("clean")));
        m.push(item(if self.busy { "Checking…" } else { "Check now" }, !self.busy, |t| {
            let _ = t.refresh.send(());
        }));
        m.push(item("Snooze notifications 24h", true, |_| {
            let _ = snooze(24);
        }));
        m.push(MenuItem::Separator);
        let mut status = self.checked.map(|t| format!("Last checked {}", hhmm(t))).unwrap_or_default();
        if self.ignored > 0 {
            status += &format!(" · {} ignored", self.ignored);
        }
        if !status.is_empty() {
            m.push(item(status, false, |_| {}));
        }
        m.push(item("Quit", true, |_| std::process::exit(0)));
        m
    }
}

/// Run the tray until Quit. Refuses to start a second instance.
pub fn run(checks: fn() -> (Vec<Finding>, usize)) -> io::Result<()> {
    let state = xdg("XDG_STATE_HOME", ".local/state").ok_or(io::ErrorKind::NotFound)?.join("sysdoctor");
    fs::create_dir_all(&state)?;
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(state.join("tray.lock"))?;
    let _lock = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusiveNonblock)
        .map_err(|_| io::Error::other("sysdoctor tray is already running"))?;

    let (tx, rx) = channel();
    let tray = SysTray { problems: vec![], ignored: 0, checked: None, busy: true, refresh: tx };
    let handle = tray.spawn().map_err(|e| io::Error::other(format!("no system tray available: {e}")))?;
    loop {
        handle.update(|t| t.busy = true);
        let (f, ignored) = checks();
        let problems: Vec<(Severity, String)> = f.into_iter().filter(|f| f.severity != Severity::Ok).map(|f| (f.severity, f.title)).collect();
        if handle
            .update(|t| {
                t.problems = problems;
                t.ignored = ignored;
                t.checked = Some(SystemTime::now());
                t.busy = false;
            })
            .is_none()
        {
            return Ok(()); // tray service ended
        }
        match rx.recv_timeout(REFRESH) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// Start the tray at login (XDG autostart) and right now, detached from this terminal.
pub fn install() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    if exe.components().any(|c| c.as_os_str() == "target") {
        eprintln!("warning: {} is a build artifact; run `cargo install --path .` and install from there", exe.display());
    }
    let dir = xdg("XDG_CONFIG_HOME", ".config").ok_or(io::ErrorKind::NotFound)?.join("autostart");
    fs::create_dir_all(&dir)?;
    let file = dir.join("sysdoctor-tray.desktop");
    fs::write(
        &file,
        format!(
            "[Desktop Entry]\nType=Application\nName=sysdoctor\nComment=System health status icon\nExec=\"{}\" tray\nIcon=utilities-system-monitor\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            exe.display()
        ),
    )?;
    println!("Installed {}", file.display());
    std::process::Command::new("setsid").arg("-f").arg(&exe).arg("tray").spawn()?;
    println!("Tray started. It will also start at every login.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_pixels() {
        let i = dot([10, 20, 30]);
        assert_eq!((i.width, i.height, i.data.len()), (32, 32, 32 * 32 * 4));
        let px = |x: usize, y: usize| &i.data[(y * 32 + x) * 4..][..4];
        assert_eq!(px(16, 16), [255, 10, 20, 30]); // centre: colour
        assert_eq!(px(16, 3), [255, 5, 10, 15]); // rim: darker
        assert_eq!(px(0, 0), [0, 0, 0, 0]); // corner: transparent
    }
}
