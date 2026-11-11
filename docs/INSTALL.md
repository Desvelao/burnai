# Installing Burnai

Everything installs into your home directory. No root is needed. Linux x86_64 only.

## What gets installed

The scheduler daemon is the only program that runs tasks. The other programs are clients that talk to it, so **every profile installs the daemon**. Each profile also installs the tray, unless you pass `--no-tray`.

| Profile | Programs | Pick it if |
|---|---|---|
| `gui` | daemon + tray + GUI | you use a desktop |
| `tui` | daemon + tray + terminal UI | you work in a terminal or over SSH |
| `cli` | daemon + tray + CLI | you only script the task queue |
| `all` | daemon + tray + GUI + TUI + CLI | you want everything |

Binary names come from `brand.toml`. With the default branding they are `burnai-daemon`, `burnai-tray`, `burnai-gui`, `burnai-tui` and `burnai-cli`.

## Requirements

- Linux x86_64 with glibc 2.35 or newer.
- A systemd user session, to start the daemon and tray at login. Without one, start the daemon yourself (see [Without systemd](#without-systemd)).
- The tray needs a StatusNotifier host: KDE, Waybar and most bars have one; GNOME needs the AppIndicator extension. On a desktop without one, install with `--no-tray`.
- The `claude` CLI, logged in with your subscription account. The app never refreshes or stores tokens.
- Optional: `xprintidle`, for the "machine idle" requirement.

## Quick install (installer script)

Pick a profile:

```sh
curl -fsSL https://raw.githubusercontent.com/Desvelao/burnai/main/install.sh | sh -s -- --profile gui
```

Replace `gui` with `tui`, `cli` or `all`. Without `--profile`, the script shows a menu when you run it from a terminal.

Piping a script into `sh` runs whatever the server sends. To read it first:

```sh
curl -fsSLO https://raw.githubusercontent.com/Desvelao/burnai/main/install.sh
less install.sh
sh install.sh --profile gui
```

The script downloads the latest release, checks its SHA-256 checksum, copies the files and starts the services. It never writes outside the install prefix and `~/.config/systemd/user`, and it never touches your data or credentials.

Options:

| Option | Meaning |
|---|---|
| `--profile gui\|tui\|cli\|all` | What to install (menu if omitted and a terminal is available) |
| `--version vX.Y.Z` | Install a specific release instead of the latest |
| `--prefix DIR` | Install under `DIR` instead of `~/.local` (absolute path, no spaces) |
| `--no-tray` | Skip the tray |
| `--no-prompt` | Never ask questions; exit with an error if `--profile` is missing (for scripts and CI) |
| `--no-systemd` | Only copy files; do not install or start services |
| `--tarball FILE` | Install from a downloaded release tarball (`FILE.sha256` must sit next to it) |
| `--repo OWNER/REPO` | Download from another GitHub repository |
| `--uninstall` | Remove what the installer installed |

Make sure `~/.local/bin` is on your `PATH`; the script warns if it is not.

## Manual install

Use this if you do not want to run the script. Download `<slug>-<version>-linux-x86_64.tar.gz` and its `.sha256` from the [releases page](https://github.com/Desvelao/burnai/releases), or build them yourself (see [Build from source](#build-from-source)). Then:

```sh
sha256sum -c burnai-*-linux-x86_64.tar.gz.sha256
tar -xzf burnai-*-linux-x86_64.tar.gz
cd burnai-*-linux-x86_64
mkdir -p ~/.local/bin ~/.config/systemd/user ~/.local/share/applications ~/.local/share/icons/hicolor/256x256/apps
```

The steps below assume you are in the extracted directory (the one with `MANIFEST`). The daemon and tray steps are the same for every profile. Do them first.

### Every profile: daemon and tray

```sh
cp burnai-daemon burnai-tray ~/.local/bin/
cp burnai-daemon.service burnai-tray.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now burnai-daemon.service burnai-tray.service
```

Skip `burnai-tray` and `burnai-tray.service` if you do not want the tray. The unit files assume `~/.local/bin`; edit `ExecStart=` if you install elsewhere.

### GUI + tray

```sh
cp burnai-gui ~/.local/bin/
cp burnai.desktop ~/.local/share/applications/
cp burnai.png ~/.local/share/icons/hicolor/256x256/apps/
```

Start it from your application menu, or run `burnai-gui`. The `.desktop` file also has a "Start scheduler" action (and "Start tray" when the tray is installed). There is no menu entry for the terminal UI, because a desktop action cannot ask for a terminal: run the TUI binary from a shell instead.

### TUI + tray

```sh
cp burnai-tui ~/.local/bin/
burnai-tui
```

### CLI + tray

```sh
cp burnai-cli ~/.local/bin/
burnai-cli status
```

The CLI talks to the daemon over its control socket, so the daemon must be running.

### All

Do the daemon and tray steps, then the GUI, TUI and CLI steps above.

## Without systemd

Skip the `.service` files and start the daemon yourself, for example from your session's autostart:

```sh
burnai-daemon
```

Only one daemon can run at a time; a second one exits with an error. Start the tray the same way if you want it.

## Check that it works

```sh
systemctl --user status burnai-daemon.service
journalctl --user -u burnai-daemon.service
```

Open a client. If the scheduler is not running, the GUI shows a red banner with **Start scheduler**, the TUI shows `[scheduler not running]` (press `R`), and the tray menu has **Start scheduler**. Clients never start the daemon by themselves.

## Upgrade

Run the installer again (same profile). It replaces the files and restarts the services. Your data and settings stay in place. Installing another profile later adds to what you already have.

## Uninstall

```sh
sh install.sh --uninstall
```

This stops and removes the services and deletes the files the installer put in place. It keeps your data and settings in `${XDG_DATA_HOME:-~/.local/share}/burnai`; delete that directory yourself if you want a clean slate.

If you installed manually, remove the files you copied and run `systemctl --user disable --now` on the two services first.

## Build from source

Builds run inside Docker; do not run `cargo` on the host.

```sh
make dist      # binaries, units, .desktop file, icon and MANIFEST in ./dist
make install-test   # checks install.sh against that output (no network, no systemd)
```

`dist/` has the same layout as the release tarball, so the manual steps above work from there. To install from your own build with the script, pack it like the release workflow and pass it with `--tarball`.

## Troubleshooting

- **"The scheduler is not running".** Use **Start scheduler**, or `systemctl --user start burnai-daemon.service`.
- **"Scheduler not responding".** The daemon is hung. Restart it with `systemctl --user restart burnai-daemon.service`.
- **The tray icon does not show.** Your desktop has no StatusNotifier host; reinstall with `--no-tray` or enable the AppIndicator extension. The tray unit retries a few times and then stops.
- **Command not found.** Add `~/.local/bin` to your `PATH`.
- **Checksum mismatch.** The download is corrupt or has been tampered with; the installer refuses to continue. Download it again.
- More: [`KNOWN_LIMITATIONS.md`](KNOWN_LIMITATIONS.md).
