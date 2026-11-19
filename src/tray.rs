//! `sysdoctor tray`: StatusNotifierItem icon (KDE, Cinnamon via xapp-sn-watcher, XFCE; GNOME needs AppIndicator).
use crate::finding::{xdg, Finding, Severity};
use crate::watch::{key, new_problems, open_in_terminal, snooze};
use std::collections::HashSet;
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
    /// New problem since the user last touched the icon: pulse until they do.
    attention: bool,
    blink: bool,
    mono: bool,
    dot: bool,
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

const GLYPHS: [&str; 11] = [
    "111101101101111", "010110010010111", "111001111100111", "111001111001111", "101101111001001",
    "111100111001111", "111100111101111", "111001010010010", "111101111101111", "111101111001111",
    "000010111010000", // "+" for 10 or more
];

fn seg_dist((px, py): (f32, f32), (ax, ay): (f32, f32), (bx, by): (f32, f32)) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((px - ax - t * dx).powi(2) + (py - ay - t * dy).powi(2)).sqrt()
}

#[derive(Clone, Copy)]
enum Badge {
    None,
    Count(usize),
    Dot,
}

/// Colour of point (x, y) on a 32x32 design grid; None = transparent.
fn shade(x: f32, y: f32, rgb: [u8; 3], mono: bool, badge: Badge) -> Option<[u8; 3]> {
    let heart = |u: f32, v: f32| (u * u + v * v - 1.0).powi(3) - u * u * v * v * v <= 0.0;
    let pulse = [(0.0, 17.0), (9.0, 17.0), (12.0, 9.0), (16.0, 24.0), (19.0, 17.0), (32.0, 17.0)];
    let (u, v) = ((x - 16.0) / 13.0, (17.5 - y) / 13.0);
    let r = ((x - 24.5).powi(2) + (y - 24.5).powi(2)).sqrt();
    match badge {
        Badge::Count(n) if r <= 7.5 => {
            let (gx, gy) = ((x - 21.5).floor() as i32, (y - 19.5).floor() as i32); // 3x5 glyph scaled x2
            let digit = (0..6).contains(&gx) && (0..10).contains(&gy) && GLYPHS[n.min(10)].as_bytes()[(gy / 2 * 3 + gx / 2) as usize] == b'1';
            return Some(if r > 6.5 || digit { [255; 3] } else { [0x24, 0x1f, 0x31] });
        }
        Badge::Dot if r <= 6.0 => return Some(if r > 4.8 { [255; 3] } else { [0xe0, 0x1b, 0x24] }),
        _ => {}
    }
    if !heart(u, v) {
        return None;
    }
    let on_line = pulse.windows(2).any(|w| seg_dist((x, y), w[0], w[1]) <= 1.4);
    match (mono, on_line) {
        (true, true) => None, // cut-out, like symbolic icons
        (true, false) => Some(rgb),
        (false, true) => Some([255; 3]),
        (false, false) if !heart(u * 1.12, v * 1.12 + 0.02) => Some(rgb.map(|c| c / 2)), // rim
        (false, false) => Some(rgb),
    }
}

/// `size`x`size` ARGB32 (network byte order), 4x4 supersampled for smooth edges.
fn heart(size: i32, rgb: [u8; 3], mono: bool, badge: Badge) -> ksni::Icon {
    const SS: i32 = 4;
    let k = 32.0 / (size * SS) as f32;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut a, mut sum) = (0.0, [0.0f32; 3]);
            for sy in 0..SS {
                for sx in 0..SS {
                    let (x, y) = (((px * SS + sx) as f32 + 0.5) * k, ((py * SS + sy) as f32 + 0.5) * k);
                    if let Some(c) = shade(x, y, rgb, mono, badge) {
                        a += 1.0;
                        (0..3).for_each(|i| sum[i] += c[i] as f32);
                    }
                }
            }
            let avg = |i: usize| if a > 0.0 { (sum[i] / a).round() as u8 } else { 0 };
            data.extend_from_slice(&[(a * 255.0 / (SS * SS) as f32).round() as u8, avg(0), avg(1), avg(2)]);
        }
    }
    ksni::Icon { width: size, height: size, data }
}

/// Icon style saved in the state dir: words "mono" and/or "dot".
fn style_path() -> Option<std::path::PathBuf> {
    Some(xdg("XDG_STATE_HOME", ".local/state")?.join("sysdoctor/tray-style"))
}

fn save_style(t: &SysTray) {
    let words = [(t.mono, "mono"), (t.dot, "dot")].iter().filter(|w| w.0).map(|w| w.1).collect::<Vec<_>>().join(" ");
    if let Some(p) = style_path() {
        let _ = fs::write(p, words);
    }
}

fn toggle(label: &str, checked: bool, f: impl Fn(&mut SysTray) + Send + Sync + 'static) -> MenuItem<SysTray> {
    let activate = move |t: &mut SysTray| {
        f(t);
        save_style(t);
    };
    ksni::menu::CheckmarkItem { label: label.into(), checked, activate: Box::new(activate), ..Default::default() }.into()
}

fn hhmm(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0).to_string();
    crate::finding::run("date", &["-d", &format!("@{secs}"), "+%H:%M"]).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn item(label: impl Into<String>, enabled: bool, f: impl Fn(&mut SysTray) + Send + Sync + 'static) -> MenuItem<SysTray> {
    let activate = move |t: &mut SysTray| {
        t.attention = false; // any interaction acknowledges
        f(t)
    };
    StandardItem { label: label.into(), enabled, activate: Box::new(activate), ..Default::default() }.into()
}

/// The report window when built with it, else the report in a terminal.
fn report() {
    if cfg!(feature = "gui") {
        if let Ok(mut c) = crate::finding::exe().and_then(|exe| std::process::Command::new(exe).arg("gui").spawn()) {
            std::thread::spawn(move || c.wait());
            return;
        }
    }
    std::thread::spawn(|| open_in_terminal("check"));
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
        let rgb = match (self.checked, self.worst()) {
            _ if self.mono => [0xde, 0xdd, 0xda],
            (None, _) => [0x9a, 0x99, 0x96],
            (_, Severity::Ok) => [0x33, 0xd1, 0x7a],
            (_, Severity::Warn) => [0xf6, 0xd3, 0x2d],
            (_, Severity::Crit) => [0xe0, 0x1b, 0x24],
        };
        let rgb = if self.blink { rgb.map(|c| c / 3) } else { rgb };
        let badge = match self.problems.len() {
            0 => Badge::None,
            _ if self.dot => Badge::Dot,
            n => Badge::Count(n),
        };
        // every common panel size, so the host never has to rescale (rescaling blurs)
        [16, 22, 24, 32, 48, 64].map(|n| heart(n, rgb, self.mono, badge)).into()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        let body = self.problems.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join("\n");
        ksni::ToolTip { title: format!("sysdoctor: {}", self.summary()), description: body, ..Default::default() }
    }
    /// Left click: full report.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.attention = false;
        report();
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut m = vec![item(self.summary(), false, |_| {})];
        for (sev, title) in &self.problems {
            let mark = if *sev == Severity::Crit { "✖" } else { "⚠" };
            m.push(item(format!("{mark}  {title}"), true, |_| report()));
        }
        m.push(MenuItem::Separator);
        m.push(item("Open full report", true, |_| report()));
        m.push(item("Fix problems…", true, terminal("check --fix")));
        m.push(item("Clean up (preview)", true, terminal("clean")));
        m.push(item(if self.busy { "Checking…" } else { "Check now" }, !self.busy, |t| {
            let _ = t.refresh.send(());
        }));
        m.push(item("Snooze notifications 24h", true, |_| {
            let _ = snooze(24);
        }));
        m.push(toggle("Monochrome icon", self.mono, |t| t.mono = !t.mono));
        m.push(toggle("Red dot instead of count", self.dot, |t| t.dot = !t.dot));
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
pub fn run(checks: fn() -> (Vec<Finding>, Vec<Finding>)) -> io::Result<()> {
    let state = xdg("XDG_STATE_HOME", ".local/state").ok_or(io::ErrorKind::NotFound)?.join("sysdoctor");
    fs::create_dir_all(&state)?;
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(state.join("tray.lock"))?;
    let _lock = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusiveNonblock)
        .map_err(|_| io::Error::other("sysdoctor tray is already running"))?;

    let (tx, rx) = channel();
    let tray = SysTray { problems: vec![], ignored: 0, checked: None, busy: true, attention: false, blink: false, mono: false, dot: false, refresh: tx };
    let style = style_path().and_then(|p| fs::read_to_string(p).ok()).unwrap_or_default();
    let tray = SysTray { mono: style.contains("mono"), dot: style.contains("dot"), ..tray };
    let handle = tray.spawn().map_err(|e| io::Error::other(format!("no system tray available: {e}")))?;
    let pulser = handle.clone();
    std::thread::spawn(move || {
        while let Some(()) = pulser.update(|t| t.blink = t.attention && !t.blink) {
            std::thread::sleep(Duration::from_millis(700));
        }
    });
    let mut prev: Option<HashSet<String>> = None; // None: first run, problems at login aren't "new"
    loop {
        handle.update(|t| t.busy = true);
        let (f, ignored) = checks();
        let fresh = prev.as_ref().is_some_and(|p| !new_problems(p, &f).is_empty());
        prev = Some(f.iter().map(key).collect());
        let problems: Vec<(Severity, String)> = f.into_iter().filter(|f| f.severity != Severity::Ok).map(|f| (f.severity, f.title)).collect();
        if handle
            .update(|t| {
                t.attention |= fresh;
                t.problems = problems;
                t.ignored = ignored.len();
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
    let exe = crate::finding::exe()?;
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
    fn heart_pixels() {
        let px = |i: &ksni::Icon, x: usize, y: usize| i.data[(y * 32 + x) * 4..][..4].to_vec();
        let i = heart(32, [10, 20, 30], false, Badge::None);
        assert_eq!((i.width, i.height, i.data.len()), (32, 32, 32 * 32 * 4));
        assert_eq!(px(&i, 8, 10), [255, 10, 20, 30]); // left lobe: colour
        assert_eq!(px(&i, 4, 17), [255, 255, 255, 255]); // pulse line
        assert_eq!(px(&i, 0, 0), [0, 0, 0, 0]); // corner: transparent
        assert_eq!(px(&i, 16, 1), [0, 0, 0, 0]); // dip between lobes
        assert_eq!(px(&i, 28, 28), [0, 0, 0, 0]); // badge area empty
        assert_eq!(px(&heart(32, [10, 20, 30], false, Badge::Count(3)), 28, 28), [255, 0x24, 0x1f, 0x31]);
        assert_eq!(px(&heart(32, [10, 20, 30], false, Badge::Dot), 24, 24), [255, 0xe0, 0x1b, 0x24]);
        assert_eq!(px(&heart(32, [10, 20, 30], true, Badge::None), 4, 17), [0, 0, 0, 0]); // mono: line cut out
        let small = heart(16, [10, 20, 30], false, Badge::None);
        assert!(small.data.chunks(4).any(|p| p[0] > 0 && p[0] < 255)); // anti-aliased edge
    }
}
