# sysdoctor

A health check and cleanup advisor for Linux desktops and servers, in one small Rust binary.

```
$ sysdoctor check
[  ok] Disk space and inodes OK
[warn] system unit casper-md5check.service failed
         fix: journalctl -u casper-md5check.service -b -n 50
[  ok] No reboot pending
[  ok] Clock synced
[  ok] Firewall active
[warn] BAT1 health 79%
         Worn; capacity below 80% of design
```

It answers two everyday questions:

- **Is anything wrong with this machine?** (`sysdoctor check`)
- **What can I safely delete?** (`sysdoctor clean`)

It can also watch in the background and notify you only when something *new* goes wrong (`sysdoctor watch`), and tell you what has been growing on disk (`sysdoctor growth`).

## Why

Linux has the raw information spread over many tools: `systemctl --failed`, `smartctl`, `journalctl`, `timedatectl`, `df`, `du`, `docker system df`, `snap list --all`, and so on. Each speaks its own format and none of them tells you what to do next.

sysdoctor collects them into one report in plain English. Every problem comes with a copyable fix command.

## Install

Requires Rust 1.87+ and Linux with systemd.

```sh
cargo install --path .
```

Optional tools used when present: `smartmontools` (disk health), `notify-send` (desktop notifications), `docker`, `snap`, `flatpak`, `gio`.

## Usage

### `sysdoctor check`

Runs all health checks and prints one line per result.

| Check | What it looks at |
|---|---|
| Disk space | Every real filesystem; warns at 90%, critical at 97% |
| Inodes | Same filesystems; "disk full" with free space left |
| SMART | Disk self-assessment via `smartctl` (needs root, skipped otherwise) |
| Failed units | Failed systemd units, both system and user |
| Reboot required | `/var/run/reboot-required` and the packages that triggered it |
| OOM kills | Out-of-memory kills in the kernel log since boot |
| Clock | Whether NTP sync is active |
| Firewall | ufw (its real `ENABLED=` switch), firewalld or nftables |
| SSH | Whether a running sshd accepts password logins |
| Battery | Current full capacity vs design capacity |
| Plugins | Your own checks, see [Custom checks](#custom-checks) |

Exit code is the worst severity found: `0` ok, `1` warning, `2` critical. This makes it usable in scripts and monitoring:

```sh
sysdoctor check >/dev/null || echo "something needs attention"
```

Run with `sudo` to include the checks that need root, such as SMART.

#### Status bar (`--bar`)

`sysdoctor check --bar` prints one line of JSON for status bars and always exits 0:

```json
{"text":"⚠ 2","tooltip":"system unit casper-md5check.service failed\nBAT1 health 79%","class":"warn","alt":"warn"}
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

**Polybar, i3blocks and others** that take plain text can extract the text field:

```sh
sysdoctor check --bar | jq -r .text
```

**GNOME** can show it with the Executor or Argos extensions.

### `sysdoctor clean [--apply]`

Lists reclaimable space, largest first. By default it is a **dry run** and changes nothing.

| Category | Found when | Fix |
|---|---|---|
| Rust `target/` | Next to a `Cargo.toml`, project untouched for 30+ days, no uncommitted git changes | Trashed by `--apply` |
| `node_modules` | Next to a `package.json`, same rules | Trashed by `--apply` |
| AI model caches | `~/.cache/huggingface`, `~/.ollama/models`, `~/.cache/torch` over 100 MB | Command shown (review first: re-download is slow) |
| Trash | Over 10 MB | `gio trash --empty` |
| Docker | Reclaimable images, containers, build cache | `docker system prune` |
| Journal | systemd journal over 500 MB | `sudo journalctl --vacuum-size=500M` |
| Snap | Disabled old revisions | `sudo snap remove <name> --revision <rev>` |
| Flatpak | Unused runtimes | `flatpak uninstall --unused` |

`--apply` shows the exact list of folders and asks for confirmation before moving them to the trash.

### `sysdoctor watch [--install]`

Runs the checks and sends a desktop notification **only for problems that were not there on the previous run**. A problem whose number changes ("91% full" → "92% full") is not new, but one that gets worse (warning → critical) is.

Several new problems at once arrive as **one** notification listing them all. Each notification has two buttons:

- **Show report** opens a terminal with the full `sysdoctor check` output (uses `x-terminal-emulator`).
- **Snooze 24h** silences notifications for a day.

Snooze from the command line too:

```sh
sysdoctor watch --snooze 8   # quiet for 8 hours
sysdoctor watch --snooze 0   # resume
```

Problems that appear during a snooze are remembered as seen, so they won't pop up when it ends. `sysdoctor check` still shows them.

It also records a disk-size snapshot, at most once a day, for `growth`.

`--install` sets up and enables an hourly systemd user timer that runs `sysdoctor watch` at low CPU and disk priority:

```sh
sysdoctor watch --install
```

To remove it:

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

It compares saved daily snapshots, so it needs `watch` to have been running (via the timer) for a while. If no snapshot exists for today, it measures first. On a large home folder that can take a few minutes.

### `--json`

`check` and `clean` accept `--json` and print a list of findings:

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

## Custom checks

Put any executable in `~/.config/sysdoctor/checks.d/`. It is run on every `check` and `watch`, using the same convention as Nagios plugins:

- **Exit code:** `0` ok, `1` warning, `2` critical, anything else marks the check as skipped
- **First line of output:** the title
- **Remaining lines:** the detail

Example, warning when the last backup is more than 2 days old:

```sh
#!/bin/sh
# ~/.config/sysdoctor/checks.d/backup
last=$(stat -c %Y ~/backups/latest 2>/dev/null || echo 0)
age=$(( ($(date +%s) - last) / 86400 ))
if [ "$age" -gt 2 ]; then
    echo "backup is $age days old"
    echo "check the backup job: systemctl --user status backup"
    exit 1
fi
echo "backup is fresh"
```

```sh
chmod 755 ~/.config/sysdoctor/checks.d/backup
```

Plugins are stopped after 10 seconds. Files writable by group or others are ignored, so nobody else can slip code into your checks.

## Safety

- **Read-only by default.** `check`, `clean`, `watch` and `growth` never change the system.
- **Only `clean --apply` deletes, and only to the trash.** It handles nothing but regenerable build output in your own projects, shows the list and asks first. You can restore from the trash.
- **Skips active work.** Projects touched in the last 30 days or with uncommitted git changes are left alone.
- **Never runs system commands for you.** Anything needing root or affecting shared state (journal, snaps, docker) is printed as a command for you to run.
- **No surprises from the disk scan.** Size measurement does not follow symlinks, does not cross into other filesystems, and counts hardlinked files once.

## Strategy and design

**One engine.** Every check and every cleanup scanner returns the same thing: a list of *findings*. A finding has a severity, a title, a detail, an optional size and an optional fix. Cleanup items are simply findings that carry a size. One renderer, one JSON format and one notification path serve every command.

**Use the system's own tools.** sysdoctor calls `systemctl`, `journalctl`, `smartctl`, `timedatectl`, `docker`, `snap` and `flatpak` instead of reimplementing them or binding their libraries. The results match what you would see by hand, and there are fewer dependencies. A missing tool or a missing permission turns into "skipped", never a crash.

**Runs as your user.** Everything works without root. Root-only checks say so and are skipped. Nothing is setuid, and no daemon is required.

**Quiet by design.** The background watcher only speaks up about new problems, grouped into one notification. A warning you have already seen does not ping you every hour.

**No resident process.** There is no tray daemon. Notifications and the status bar output cover the same ground. Waiting for a click on a notification button runs as a short-lived transient unit (`systemd-run --user`), because the timer's own job is ended as soon as `watch` exits.

**Cheap to run often.** Hourly runs take a fraction of a second. The expensive full disk-size measurement runs at most once a day, in parallel, at idle I/O priority.

**Plain files for state.** Kept in `~/.local/state/sysdoctor/` (or `$XDG_STATE_HOME`):

| File | Contents |
|---|---|
| `last.json` | Problems seen on the previous `watch` run |
| `growth.jsonl` | One disk-size snapshot per line, appended daily |
| `snooze-until` | Unix time when a snooze ends |

Delete them at any time to reset.

### Code layout

| File | Role |
|---|---|
| `src/finding.rs` | The `Finding` type, command helpers, size parsing and formatting |
| `src/check.rs` | Health checks and custom check plugins |
| `src/clean.rs` | Cleanup scanners and the disk-usage walker |
| `src/watch.rs` | New-problem detection, notifications, timer install, growth snapshots |
| `src/main.rs` | Command line and output |

Adding a check means writing one `fn() -> Vec<Finding>` and listing it in `CHECKS` in `src/check.rs`. Adding a cleanup category works the same way with `SCANNERS` in `src/clean.rs`.

### Known limits

- Debian/Ubuntu-centric for now: reboot detection uses `/var/run/reboot-required`, and package caches for apt, dnf and pacman are not scanned yet.
- The project scan skips folders whose names start with `.`, so a project kept in a hidden folder will not be found.
- `growth` tracks only the top level of your home folder, not deeper paths.
- Flatpak does not report sizes for unused runtimes, so those show a count only.

## Development

```sh
cargo test       # unit tests
cargo clippy     # lints
cargo run -- check
```
