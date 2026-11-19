# sysdoctor

A health check and cleanup advisor for Linux desktops and servers, in one small Rust binary.

```
$ sysdoctor check
[  ok] Disk space and inodes OK
[  ok] System up to date
[  ok] Firewall active
[  ok] Timeshift: last backup 0.2 days ago
[  ok] Kopia: last backup 0.1 days ago
[warn] BAT1 health 79%
         Worn; capacity below 80% of design
[warn] 19 error patterns this boot, 4 new since last boot
            36x  kernel: ecryptfs_write_metadata: Error writing metadata out to lower file; rc = #
         fix: journalctl -b -p err
```

It answers two everyday questions:

- **Is anything wrong with this machine?** (`sysdoctor check`)
- **What can I safely delete?** (`sysdoctor clean`)

It can also watch in the background and notify you only when something *new* goes wrong, show what has been growing on disk, track battery and disk wear over time, explain what slows your startup, and produce a privacy-scrubbed summary to paste into a forum when asking for help.

## Features

- **Health check:** about 20 checks covering disks, SMART, failed services, updates, pending reboot, memory, firewall, SSH, file permissions, leaked tokens in shell history, expiring keys and certificates, backups, battery, CPU throttling, firmware, system log errors and clock sync. Each problem says what is wrong and how to fix it.
- **Cleanup advisor:** finds reclaimable space (stale build output, package and developer caches, AI models, old kernels, journal, containers, conda, snap, flatpak) and sizes it before you decide anything.
- **Report window** (`sysdoctor gui`): the full report with a Run button per problem, a tick list for cleanup with a live total of what you selected, ignored issues kept visible with **Stop ignoring**, and an embedded terminal that shows commands running.
- **Tray icon:** a heart that turns yellow or red, counts problems, and pulses when something new appears.
- **Background watch:** an hourly timer that notifies you only about *new* problems, with snooze.
- **History:** what grew on disk, battery and disk wear over time, and what slows boot, login and your shell.
- **Forum-ready report:** a Markdown summary with your user name, hostname, home path and addresses scrubbed out.
- **Fits in:** status bar JSON (Waybar and others), `--json` output, exit codes for scripts, your own check plugins, an ignore list and a config file.
- **Most distros:** apt, dnf, pacman and zypper are detected automatically.

### You stay in control

- **Nothing changes unless you ask.** Checking, listing, the tray icon and the background watch only *read*. Something on your system changes only when you press **Run** or **Clean selected** in the window, run `check --fix` or `clean --apply`, or press a notification's **Snooze** button. **Ignore** and **Stop ignoring** only edit sysdoctor's own ignore list.
- **Every system change is confirmed.** The window shows a dialog listing each command and each folder before anything happens. On the command line you answer `y` for each command separately.
- **You see the exact commands.** Every problem and cleanup item shows its fix in full: the command under **Runs**, or the folders under **Moves to trash**. When it runs, each command is printed in the terminal before it starts, and the sequence stops at the first failure.
- **Root only through `sudo`.** Commands that need root say so (`sudo …`) and go through your system's normal password prompt. sysdoctor never stores a password, is never setuid, and runs no root process of its own.
- **Careful with deletion.** Folders sysdoctor removes itself go to the trash, so you can restore them. Cleanup *commands* (emptying the trash, `apt clean`, `docker system prune`, journal vacuum, removing old kernels) delete for good, which is why they are spelled out and confirmed first. Only regenerable items are pre-selected; anything that needs a judgement call starts unticked. Projects used in the last 30 days (configurable), projects with uncommitted git changes, the running kernel and the two newest kernels are never offered.
- **Background parts are opt-in.** The timer and the tray icon exist only after you run `watch --install` or `tray --install`, and both can be removed.

### Your data stays yours

- **No network.** No telemetry, no uploads, no update checks. Pending updates are counted from your package manager's local cache. The only connection it may make is to *your own* backup repository (Kopia, restic or borg, if you set one up), to read the date of the last snapshot. That is read-only and times out after 30–60 seconds.
- **Nothing leaves the machine on its own.** Notifications are local desktop notifications. The forum report is only printed or copied to your clipboard when you ask, and identifying details are scrubbed first.
- **Secrets are never shown.** The shell-history scan reports the file and line number of a token, never the token itself.
- **Local, plain-text state.** What it remembers (problems seen last time, daily folder sizes in your home, battery and disk wear) lives in `~/.local/state/sysdoctor/`, and your settings in `~/.config/sysdoctor/`. Delete them whenever you like.

## Why

Linux has the raw information spread over many tools: `systemctl --failed`, `smartctl`, `journalctl`, `apt`, `timedatectl`, `df`, `du`, `docker system df`, `snap list --all`, and so on. Each speaks its own format and none of them tells you what to do next.

sysdoctor collects them into one report in plain English. Every problem comes with a copyable fix command, and `--fix` can run them for you, one confirmation at a time.

## Install

**From source** (Rust 1.85+, Linux with systemd):

```sh
cargo install --path .
```

With the report window (`sysdoctor gui`), which needs GTK 4 and VTE headers to build:

```sh
sudo apt install libgtk-4-dev libvte-2.91-gtk4-dev
cargo install --path . --features gui
```

**As a .deb** (Ubuntu 24.04, Mint 22, Debian 13 or newer), which includes the report window, an app-menu entry, the man page and shell completions. Download it from [Releases](https://github.com/fagostini/sysdoctor/releases), or build it:

```sh
cargo install cargo-deb
./scripts/deb.sh
sudo apt install ./target/debian/sysdoctor_*.deb
```

On Fedora use `sudo dnf install 'pkgconfig(gtk4)' 'pkgconfig(vte-2.91-gtk4)'`, on openSUSE the same with `sudo zypper install`, and on Arch `sudo pacman -S gtk4 vte4`.

Optional tools used when present: `smartmontools` (disk health and wear), `libnotify-bin` (desktop notifications), `gio`, `gpg`, `openssl`; `docker`, `podman`, `snap`, `flatpak`, `conda` for cleanup; `kopia`, `restic`, `borg`/`borgmatic`, `snapper` for the backup check.

**Shell completions** for a source install:

```sh
sysdoctor completions zsh > ~/.zfunc/_sysdoctor     # or bash, fish, elvish, powershell
```

## Commands

| Command | Does |
|---|---|
| `check` | Health report. `--fix` offers to run fixes, `--bar` prints status bar JSON |
| `clean` | Lists reclaimable space. `--apply` trashes regenerable files and offers cleanup commands |
| `gui` | Report window: Run and Ignore per problem, tick-and-clean cleanup list, embedded terminal. Needs `--features gui` |
| `tray` | Status icon in the panel. `--install` starts it at every login |
| `watch` | For timers: notifies on new problems, records daily snapshots. `--install`, `--snooze H` |
| `growth` | What grew on disk over the last N days |
| `trends` | Battery health and disk wear over time |
| `startup` | What slows boot, login and new terminals |
| `report` | Privacy-scrubbed Markdown summary for forums and bug reports |
| `ignore` | Mute a finding you've decided to live with |
| `config` | Show effective thresholds and where to override them |
| `completions` | Print shell completions |

### `sysdoctor check`

| Area | Checks |
|---|---|
| Storage | Disk space (warns at 90%, critical at 97%), inode exhaustion, SMART health (needs root) |
| Services | Failed systemd units, system and user |
| Updates | Pending security updates (apt, dnf, zypper; pacman shows a count only), reboot required, running kernel older than installed |
| Memory | OOM kills this boot, sustained memory pressure (PSI) |
| Security | Firewall active, SSH password login, permissions on `~/.ssh`, `~/.gnupg` and private keys, known API token formats in shell history |
| Expiry | Your GPG keys, SSH certificates, Let's Encrypt and configured certificates expiring within 30 days |
| Backups | Last Timeshift, Kopia, restic, borg/borgmatic, snapper or Déjà Dup backup older than 7 days, or no backups at all |
| Hardware | Battery health, CPU thermal throttling, missing firmware |
| Logs | Error messages this boot, grouped by pattern, highlighting the ones that are new since last boot |
| Time | NTP sync |
| Plugins | Your own checks, see [Custom checks](#custom-checks) |

Exit code is the worst severity found: `0` ok, `1` warning, `2` critical:

```sh
sysdoctor check >/dev/null || echo "something needs attention"
```

Run with `sudo` to include checks that need root, such as SMART. With a `cargo install`, use `sudo "$(command -v sysdoctor)" check`, because `sudo` does not search `~/.cargo/bin`.

**Secrets in history** are reported by file and line number only. The secret itself is never printed. Revoke the token first: history files get backed up and synced.

#### `--fix`

Shows each problem's fix command and asks before running it. Commands run in your shell, so `sudo` can ask for your password. Only works in an interactive terminal.

#### Status bar (`--bar`)

`sysdoctor check --bar` prints one line of JSON for status bars and always exits 0:

```json
{"text":"⚠ 2","tooltip":"BAT1 health 79%\n19 error patterns this boot, 4 new since last boot","class":"warn","alt":"warn"}
```

`text` is ✓, ⚠ or ✖ plus the number of problems, `tooltip` lists them, and `class` is `ok`, `warn` or `crit` for styling.

**Waybar** (`~/.config/waybar/config`):

```json
"custom/sysdoctor": {
    "exec": "sysdoctor check --bar",
    "return-type": "json",
    "interval": 300,
    "on-click": "x-terminal-emulator -e 'sh -c \"sysdoctor check; read _\"'"
}
```

```css
#custom-sysdoctor.warn { color: #e5a50a; }
#custom-sysdoctor.crit { color: #e01b24; }
```

**Polybar, i3blocks and others** that take plain text: `sysdoctor check --bar | jq -r .text`. **GNOME**: use the Executor or Argos extension.

### `sysdoctor gui`

The whole report in one window (build with `--features gui`):

- **Health** tab: one card per problem with its fix command, a **Run…** button and an **Ignore** button. Passing checks are folded away. Ignored problems stay listed, dimmed, with **Stop ignoring**.
- **Cleanup** tab: every reclaimable item with its size and exact command or folders. Regenerable items start ticked, items that need a decision start unticked. The header shows *Selected: X of Y reclaimable* as you tick.
- **Run** and **Clean selected** first show a confirmation dialog listing everything that will happen. Folders go to the trash. Commands run in a terminal panel at the bottom of the window, where `sudo` can ask for your password, then the report refreshes.
- **Check again** re-runs everything. **Copy** puts the scrubbed `report` on the clipboard.

Only one window opens: launching it again brings the existing one to the front.

### `sysdoctor tray [--install]`

A heart with a pulse line in the panel tray: **green** all OK, **yellow** warnings, **red** critical, grey while the first check runs. A badge shows the number of problems (+ for 10 or more). The menu can switch to a monochrome heart and to a plain red dot instead of the number; the choice is saved in `~/.local/state/sysdoctor/tray-style`. When a check finds a new problem the heart pulses until you click it or use the menu. Hovering lists the problems. Left click opens the report window (or the report in a terminal, if built without `gui`). The menu has:

- the current problems (click to open the report)
- **Open full report**, **Fix problems…** (`check --fix`), **Clean up (preview)**
- **Check now**, **Snooze notifications 24h**, **Monochrome icon**, **Red dot instead of count**, **Quit**

Ignored problems are not counted.

It re-runs the checks every 15 minutes. Only one instance runs at a time.

```sh
sysdoctor tray --install   # start now and at every login
```

To remove it: choose **Quit** in the menu, then `rm ~/.config/autostart/sysdoctor-tray.desktop`.

Works on KDE, Cinnamon, XFCE, MATE and most other panels that support StatusNotifierItem tray icons. **GNOME** needs the *AppIndicator and KStatusNotifierItem Support* extension (preinstalled on Ubuntu).

The tray only shows status. Desktop notifications come from the `watch` timer, so for both run `sysdoctor watch --install` too.

### `sysdoctor clean [--apply]`

Lists reclaimable space, largest first. By default it is a **dry run** and changes nothing.

| Category | Found when | `--apply` |
|---|---|---|
| Rust `target/`, `node_modules` | Project untouched for 30+ days and no uncommitted git changes | Trashed |
| Package caches | pip, cargo, npm, yarn, pnpm, Go, Gradle, Maven caches, Playwright browsers and thumbnails over 50 MB | Trashed |
| AI model caches | Hugging Face, Ollama, torch over 100 MB | Command offered (re-downloading is slow) |
| Stale `~/.cache` folders | Nothing written for 90+ days | Command offered (some apps keep state there) |
| Trash | Over 10 MB | `gio trash --empty` offered |
| Package cache | Downloaded packages (apt, dnf, pacman, zypper) | The distro's clean command offered (`apt clean`, `dnf clean packages`, `paccache -rk2`, `zypper clean`) |
| Old kernels | All but the newest two and the running one (apt, dnf, zypper; Arch keeps one) | `apt purge <packages>`, `dnf remove --oldinstallonly` or `zypper purge-kernels` offered |
| Docker, Podman, conda, journal, snap, flatpak | Reclaimable images, journal over 500 MB, disabled snap revisions, unused runtimes | Their cleanup commands offered |

`--apply` first shows every folder it will move to the trash and asks once. It then offers each cleanup command individually. Trashed files still use disk space until you empty the trash.

### `sysdoctor watch [--install] [--snooze HOURS]`

Runs the checks and sends a desktop notification **only for problems that were not there on the previous run**. A problem whose number changes ("91% full" → "92% full") is not new, but one that gets worse (warning → critical) is.

Several new problems at once arrive as **one** notification listing them all. Each notification has two buttons:

- **Show report** opens a terminal with the full `sysdoctor check` output (via `x-terminal-emulator`).
- **Snooze 24h** silences notifications for a day.

```sh
sysdoctor watch --install    # hourly systemd user timer, low CPU and I/O priority
sysdoctor watch --snooze 8   # quiet for 8 hours
sysdoctor watch --snooze 0   # resume
```

Problems that appear during a snooze are remembered as seen, so they won't pop up when it ends. `sysdoctor check` still shows them.

`watch` also records one disk-size snapshot (for `growth`) and one health sample (for `trends`) per day.

To remove the timer:

```sh
systemctl --user disable --now sysdoctor.timer
rm ~/.config/systemd/user/sysdoctor.{service,timer}
```

### `sysdoctor growth [--days N]`

Shows what grew on disk over the last N days (default 7), per top-level entry in your home folder and per filesystem:

```
Changes over the last 7.0 days:
  +   12.3G  ~/.cache
  +    1.1G  mount /
  -  800.0M  ~/Downloads
```

It compares the daily snapshots that `watch` saves. If no snapshot exists for today it measures first, which can take a few minutes on a large home folder.

### `sysdoctor trends`

Shows slow-moving health values over time, from the daily samples `watch` records:

```
BAT1 battery health %                 85.0 ->    79.0 over 182 days  (-0.99 per 30 days)
nvme0n1 wear %                         3.0 ->     5.0 over 182 days  (+0.33 per 30 days)
```

Battery health is recorded as a normal user. Disk wear and reallocated sectors need root, because `smartctl` does.

### `sysdoctor startup`

Shows boot time and its slowest services, the slowest login services, your desktop autostart entries, and how long your shell takes to open. It warns above 300 ms. It runs your interactive shell three times to measure that, so it is a separate command and never runs from the timer.

### `sysdoctor report`

Prints a Markdown summary: OS, kernel, desktop, CPU, memory, GPU, disks, current problems and passing checks. Paste it into a forum post or bug report:

```sh
sysdoctor report | xclip -selection clipboard
```

Before printing, it replaces your home path with `~`, and your user name, hostname, IPv4 and MAC addresses with placeholders. Read it before posting anyway.

### `sysdoctor ignore [PATTERN] [--remove]`

Mutes findings whose id or title contains PATTERN (case-insensitive). Useful for known, harmless problems:

```sh
sysdoctor ignore casper-md5check     # leftover from the installer
sysdoctor ignore                     # list
sysdoctor ignore --remove casper-md5check
```

The list lives in `~/.config/sysdoctor/ignore`, one pattern per line. Ignored findings disappear from `check`, `watch`, `--bar`, `report` and the tray count, and `check` shows how many were hidden. The window keeps them visible in an **Ignored** section with a **Stop ignoring** button, which removes every pattern that matches.

The window's **Ignore** button adds the finding's id, so it mutes that whole kind of finding (for example all failed units). Use `sysdoctor ignore` with part of the title to be more specific.

### `--json`

`check`, `clean` and `startup` accept `--json` and print a list of findings:

```json
[
  {
    "id": "battery",
    "severity": "warn",
    "title": "BAT1 health 79%",
    "detail": "Worn; capacity below 80% of design"
  }
]
```

Fields `bytes` (reclaimable size) and `fix` (`{"kind": "command" | "trash", "value": ...}`) appear when they apply.

## Configuration

`sysdoctor config` shows the effective settings. To change them, put only the keys you want in `~/.config/sysdoctor/config.toml`:

```toml
disk_warn = 85          # % full that warns (default 90)
disk_crit = 95          # % full that is critical (default 97)
battery_warn = 75       # battery health % (default 80)
battery_crit = 60
stale_days = 60         # build output in projects untouched this long (default 30)
cache_stale_days = 90   # unused ~/.cache folders (default 90)
journal_max_mb = 1000   # journal size before it counts as reclaimable (default 500)
backup_max_days = 3     # warn when the newest backup is older (default 7)
expiry_warn_days = 30   # keys/certificates expiring within (default 30)
cert_paths = ["/etc/ssl/private/mysite.pem"]   # extra certificates to watch
```

A misspelled key is reported, and the defaults are used, rather than silently ignored.

## Custom checks

Put any executable in `~/.config/sysdoctor/checks.d/`. It runs on every `check` and `watch`, using the same convention as Nagios plugins:

- **Exit code:** `0` ok, `1` warning, `2` critical, anything else marks the check as skipped
- **First line of output:** the title
- **Remaining lines:** the detail

Example, a stricter restic check (warn after 2 days instead of `backup_max_days`):

```sh
#!/bin/sh
# ~/.config/sysdoctor/checks.d/restic
last=$(restic snapshots --latest 1 --json | jq -r '.[0].time')
age=$(( ($(date +%s) - $(date -d "$last" +%s)) / 86400 ))
if [ "$age" -gt 2 ]; then
    echo "restic backup is $age days old"
    exit 1
fi
echo "restic backup is fresh"
```

```sh
chmod 755 ~/.config/sysdoctor/checks.d/restic
```

Plugins are stopped after 10 seconds. Files writable by group or others are ignored, so nobody else can slip code into your checks.

## Safety details

- **Disk scan:** size measurement does not follow symlinks, does not cross into other filesystems, and counts hardlinked files once.
- **Plugins:** only executables in `~/.config/sysdoctor/checks.d/` that are not writable by group or others are run, and each is stopped after 10 seconds.
- **`clean --apply`** shows every folder it will trash and asks once, then offers each command separately.
- **`check --fix`** only works in an interactive terminal, so it can never run unattended.

## Strategy and design

**One engine.** Every check and every cleanup scanner returns the same thing: a list of *findings*. A finding has a severity, a title, a detail, an optional size and an optional fix. Cleanup items are simply findings that carry a size. One renderer, one JSON format, one ignore list, one `--fix` loop and one notification path serve every command.

**Use the system's own tools.** sysdoctor calls `systemctl`, `journalctl`, `apt-get`, `smartctl`, `gpg` and friends instead of reimplementing them or binding their libraries. The results match what you would see by hand, and there are fewer dependencies. A missing tool or a missing permission turns into "skipped", never a crash.

**Runs as your user.** Everything works without root. Root-only checks say so and are skipped. Nothing is setuid, and no daemon is required.

**Quiet by design.** The background watcher only speaks up about new problems, grouped into one notification. Known, harmless issues can be muted with `ignore`. A warning you have already seen does not ping you every hour.

**Optional resident process.** Checks and notifications need no daemon: the hourly timer does the work. The tray icon is the only long-running part, and it is optional. Waiting for a click on a notification button runs as a short-lived transient unit (`systemd-run --user`), because the timer's own job is ended as soon as `watch` exits.

**Cheap to run often.** An hourly run takes about a second and a half. The expensive full disk measurement runs at most once a day, in parallel, at idle I/O priority. Cleanup scanners run concurrently.

**Plain files for state.** Kept in `~/.local/state/sysdoctor/` (or `$XDG_STATE_HOME`):

| File | Contents |
|---|---|
| `last.json` | Problems seen on the previous `watch` run |
| `growth.jsonl` | One disk-size snapshot per line, appended daily |
| `trends.jsonl` | One health sample (battery, disk wear) per line, appended daily |
| `snooze-until` | Unix time when a snooze ends |
| `tray.lock` | Keeps a second tray icon from starting |
| `tray-style` | Tray icon options (monochrome, red dot) |

Delete them at any time to reset.

### Code layout

| File | Role |
|---|---|
| `src/finding.rs` | The `Finding` type, command helpers, size parsing and formatting |
| `src/config.rs` | `config.toml` thresholds and the ignore list |
| `src/check.rs` | Core checks (disks, SMART, units, firewall, SSH, battery) and plugins |
| `src/security.rs` | Updates, kernel, permissions, secrets in history, key and certificate expiry |
| `src/health.rs` | Backups, journal digest, throttling, memory pressure, firmware |
| `src/clean.rs` | Cleanup scanners and the disk-usage walker |
| `src/watch.rs` | Notifications, snooze, timer install, growth and trends |
| `src/report.rs` | `report` and `startup` |
| `src/pkg.rs` | Package manager detection: updates, reboot, package cache, install hints |
| `src/tray.rs` | The tray icon |
| `src/gui.rs` | The report window (feature `gui`) |
| `src/main.rs` | Command line and output |

Adding a check means writing one `fn() -> Vec<Finding>` and listing it in `CHECKS` in `src/check.rs`. Adding a cleanup category works the same way with `SCANNERS` in `src/clean.rs`.

### Known limits

- restic and borg are found through `$RESTIC_REPOSITORY` / `$BORG_REPO` (or a borgmatic config). Local repositories are read without a password; remote ones need the password in the environment, otherwise they show as "not reachable". snapper needs your user in `ALLOW_USERS`.
- Package checks detect apt, dnf, pacman or zypper. They read cached metadata only (no network, no root), so counts are as fresh as the last refresh. Pending reboot on Fedora needs `dnf needs-restarting` (dnf-utils). The .deb package and `x-terminal-emulator` fallback are Debian-family only.
- The project scan skips folders whose names start with `.`, so a project kept in a hidden folder will not be found.
- `growth` tracks only the top level of your home folder.
- Thermal throttling uses a fixed threshold of 100 events since boot, which some laptops exceed under normal load. `ignore throttling` if it's noise for you.
- Flatpak does not report sizes for unused runtimes, so those show a count only.

## Development

```sh
cargo test                       # unit tests
cargo clippy --all-targets       # lints (add --features gui for the window)
cargo run -- check
cargo run --features gui -- gui
./scripts/deb.sh # build the .deb
```

## License

MIT, see [LICENSE](LICENSE).
