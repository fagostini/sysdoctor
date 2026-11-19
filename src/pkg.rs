//! Package manager detection: apt (Debian/Ubuntu/Mint), dnf (Fedora/RHEL), pacman (Arch), zypper (openSUSE).
use crate::finding::run;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pm {
    Apt,
    Dnf,
    Pacman,
    Zypper,
}

/// First package manager found in PATH, checked once.
pub fn detect() -> Option<Pm> {
    static PM: OnceLock<Option<Pm>> = OnceLock::new();
    *PM.get_or_init(|| {
        [("apt-get", Pm::Apt), ("dnf", Pm::Dnf), ("pacman", Pm::Pacman), ("zypper", Pm::Zypper)]
            .into_iter()
            .find(|(bin, _)| Command::new(bin).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok())
            .map(|(_, pm)| pm)
    })
}

/// Install hint; package names are the same across distros except where mapped.
pub fn install(pkg: &str) -> String {
    match (detect(), pkg) {
        (Some(Pm::Zypper), "linux-firmware") => "sudo zypper install kernel-firmware-all".into(),
        (Some(Pm::Dnf), p) => format!("sudo dnf install {p}"),
        (Some(Pm::Pacman), p) => format!("sudo pacman -S {p}"),
        (Some(Pm::Zypper), p) => format!("sudo zypper install {p}"),
        (_, p) => format!("sudo apt install {p}"),
    }
}

/// Lines of `dnf check-update` that name a package ("name.arch  version  repo").
fn count_dnf(list: &str) -> usize {
    list.lines()
        .filter(|l| !l.starts_with(char::is_whitespace))
        .filter(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            f.len() == 3 && f[0].contains('.')
        })
        .count()
}

/// Rows of a zypper table whose columns include `marker` (e.g. "v" for list-updates, "security" for list-patches).
fn count_zypper(table: &str, marker: &str) -> usize {
    table.lines().filter(|l| l.split('|').map(str::trim).any(|c| c == marker)).count()
}

/// (upgradable, security or None if this distro doesn't say, upgrade command). None: unknown.
/// Uses cached metadata only: a health check must not hit the network or need root.
pub fn updates() -> Option<(usize, Option<usize>, &'static str)> {
    match detect()? {
        Pm::Apt => {
            let sim = run("apt-get", &["-s", "-o", "Debug::NoLocking=1", "upgrade"])?;
            let (n, s) = crate::security::count_updates(&sim);
            Some((n, Some(s), "sudo apt update && sudo apt upgrade"))
        }
        Pm::Dnf => {
            // check-update exits 100 when updates exist, so read stdout regardless
            let list = crate::finding::run_any("dnf", &["-q", "-C", "check-update"])?;
            let sec = crate::finding::run_any("dnf", &["-q", "-C", "updateinfo", "list", "--security"]).map(|s| s.lines().filter(|l| !l.trim().is_empty()).count());
            Some((count_dnf(&list), sec, "sudo dnf upgrade"))
        }
        Pm::Pacman => {
            // local db only, as of the last `pacman -Sy`. Not checkupdates: it syncs from the mirrors (network).
            // -Qu exits 1 when nothing is upgradable, so read stdout regardless.
            let list = crate::finding::run_any("pacman", &["-Qu"])?;
            Some((list.lines().filter(|l| !l.trim().is_empty()).count(), None, "sudo pacman -Syu"))
        }
        Pm::Zypper => {
            let lu = run("zypper", &["--no-refresh", "-q", "list-updates"])?;
            let sec = run("zypper", &["--no-refresh", "-q", "list-patches", "--category", "security"]).map(|t| count_zypper(&t, "security"));
            Some((count_zypper(&lu, "v"), sec, "sudo zypper update"))
        }
    }
}

/// Some(true) if the distro says a reboot is pending, None if it can't tell.
pub fn reboot_needed() -> Option<bool> {
    match detect()? {
        Pm::Apt => Some(Path::new("/var/run/reboot-required").exists()),
        // needs-restarting -r (dnf-plugins-core / dnf5 plugin): exit 0 = no reboot, 1 = reboot. Without
        // the plugin dnf also exits 1 ("No such command"), so 1 counts only with the plugin's message.
        Pm::Dnf => {
            let out = Command::new("dnf").args(["needs-restarting", "-r"]).stdin(Stdio::null()).output().ok()?;
            match out.status.code()? {
                0 => Some(false),
                1 => String::from_utf8_lossy(&out.stdout).to_lowercase().contains("reboot is required").then_some(true),
                _ => None,
            }
        }
        // Arch removes the running kernel's modules on upgrade
        // (no modules at all = container or foreign kernel: can't tell)
        Pm::Pacman => {
            let r = run("uname", &["-r"])?;
            let any = std::fs::read_dir("/usr/lib/modules").ok()?.next().is_some();
            any.then(|| !Path::new("/usr/lib/modules").join(r.trim()).exists())
        }
        Pm::Zypper => Some(Path::new("/run/reboot-needed").exists()),
    }
}

/// (cache dir, clean command) for downloaded packages.
pub fn cache() -> Option<(&'static str, &'static str)> {
    Some(match detect()? {
        Pm::Apt => ("/var/cache/apt/archives", "sudo apt clean"),
        Pm::Dnf if Path::new("/var/cache/libdnf5").exists() => ("/var/cache/libdnf5", "sudo dnf clean packages"),
        Pm::Dnf => ("/var/cache/dnf", "sudo dnf clean packages"),
        Pm::Pacman if run("paccache", &["--version"]).is_some() => ("/var/cache/pacman/pkg", "sudo paccache -rk2"),
        Pm::Pacman => ("/var/cache/pacman/pkg", "sudo pacman -Sc"),
        Pm::Zypper => ("/var/cache/zypp/packages", "sudo zypper clean --all"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers() {
        let dnf = "\nfirefox.x86_64   131.0-1.fc40   updates\nkernel.x86_64  6.11.3-200.fc40  updates\nObsoleting Packages\ngrub2.noarch  1:2.12-1.fc40  updates\n    grub2-tools.noarch 1:2.06 @anaconda\n";
        assert_eq!(count_dnf(dnf), 3);
        let lu = "S | Repository | Name | Current | Available | Arch\n--+---\nv | Main       | curl | 8.9   | 8.10      | x86_64\nv | Main       | vim  | 9.1   | 9.1.1     | x86_64\n";
        assert_eq!(count_zypper(lu, "v"), 2);
        let lp = "Repository | Name | Category | Severity\n---\nUpdate | openSUSE-2024-1 | security | important\nUpdate | openSUSE-2024-2 | recommended | low\n";
        assert_eq!(count_zypper(lp, "security"), 1);
    }
}
