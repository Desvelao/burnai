#!/bin/sh
# Installer for the release tarball (per-user, no root). Names (slug, binaries) come from the MANIFEST file
# inside the tarball, so nothing about the product is hardcoded here.
#
#   curl -fsSL https://raw.githubusercontent.com/Desvelao/burnai/main/install.sh | sh -s -- --profile gui
#
# See docs/INSTALL.md for the profiles and the manual steps.
set -eu

REPO="Desvelao/burnai"
PREFIX="${HOME:-}/.local"
PROFILE=""
VERSION=""
TARBALL=""
WITH_TRAY=1
WITH_SYSTEMD=1
UNINSTALL=0
PROMPT=1

usage() {
    cat >&2 <<'EOF'
Usage: install.sh [--profile gui|tui|cli|all] [options]

Profiles (each also installs the scheduler daemon and, unless --no-tray, the tray):
  gui   daemon + tray + GUI
  tui   daemon + tray + terminal UI
  cli   daemon + tray + CLI
  all   daemon + tray + GUI + TUI + CLI

Options:
  --version vX.Y.Z   release tag to install (default: latest)
  --prefix DIR       install under DIR (default: ~/.local)
  --repo OWNER/REPO  GitHub repository to download from
  --tarball FILE     install from a local release tarball (needs FILE.sha256 next to it)
  --no-tray          do not install or enable the tray
  --no-systemd       do not touch systemd; just copy files
  --no-prompt        never ask questions (exit 2 when --profile is missing)
  --uninstall        remove what this installer installed (your data and settings are kept)
  -h, --help         show this help

Without --profile, a menu is shown when a terminal is available; otherwise this help is printed.
EOF
}

say() { printf '%s\n' "$*"; }
err() { printf 'error: %s\n' "$*" >&2; }
die() { err "$*"; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# True when a controlling terminal exists (also under `curl | sh`, where stdin is the pipe).
have_tty() { ( : </dev/tty ) 2>/dev/null; }

choose_profile() {
    {
        say "What do you want to install? (daemon + tray are always included)"
        say "  1) gui  - graphical app"
        say "  2) tui  - terminal UI"
        say "  3) cli  - scripting CLI"
        say "  4) all  - GUI, TUI and CLI"
        printf 'Choice [1-4]: '
    } >&2
    read -r choice </dev/tty || die "no answer"
    case "$choice" in
        1 | gui) PROFILE=gui ;;
        2 | tui) PROFILE=tui ;;
        3 | cli) PROFILE=cli ;;
        4 | all) PROFILE=all ;;
        *) die "invalid choice: $choice" ;;
    esac
}

fetch() { # fetch URL DEST
    if have curl; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
    elif have wget; then
        wget -q -O "$2" "$1"
    else
        die "curl or wget is required"
    fi
}

sha256_of() {
    if have sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    elif have shasum; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        die "sha256sum is required to verify the download"
    fi
}

# Verify FILE against the first field of SUMFILE.
verify_sum() {
    want=$(cut -d' ' -f1 "$2" | head -n1)
    got=$(sha256_of "$1")
    [ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $(basename "$1"): refusing to install"
}

safe_name() { # safe_name VALUE: non-empty, [A-Za-z0-9._-] only
    case "$1" in
        '' | *[!A-Za-z0-9._-]*) return 1 ;;
    esac
    return 0
}

# Remove everything a previous run installed (reads the receipts it wrote).
do_uninstall() {
    rdir="$PREFIX/share/install-receipts"
    found=0
    if [ -d "$rdir" ]; then
        for r in "$rdir"/*.list; do
            [ -f "$r" ] || continue
            found=1
            while IFS=' ' read -r kind path; do
                case "$kind" in
                    unit)
                        if [ "$WITH_SYSTEMD" = 1 ] && have systemctl; then
                            systemctl --user disable --now "$path" >/dev/null 2>&1 || true
                        fi
                        ;;
                esac
            done <"$r"
            while IFS=' ' read -r kind path; do
                case "$kind" in
                    file) rm -f -- "$path" && say "removed $path" ;;
                esac
            done <"$r"
            rm -f -- "$r"
        done
        rmdir "$rdir" 2>/dev/null || true
    fi
    [ "$found" = 1 ] || say "nothing to uninstall under $PREFIX"
    if [ "$WITH_SYSTEMD" = 1 ] && have systemctl; then
        systemctl --user daemon-reload >/dev/null 2>&1 || true
    fi
    say "Your data and settings were not touched."
}

main() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --profile) [ $# -ge 2 ] || die "--profile needs a value"; PROFILE=$2; shift 2 ;;
            --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION=$2; shift 2 ;;
            --prefix) [ $# -ge 2 ] || die "--prefix needs a value"; PREFIX=$2; shift 2 ;;
            --repo) [ $# -ge 2 ] || die "--repo needs a value"; REPO=$2; shift 2 ;;
            --tarball) [ $# -ge 2 ] || die "--tarball needs a value"; TARBALL=$2; shift 2 ;;
            --no-tray) WITH_TRAY=0; shift ;;
            --no-systemd) WITH_SYSTEMD=0; shift ;;
            --no-prompt) PROMPT=0; shift ;;
            --uninstall) UNINSTALL=1; shift ;;
            -h | --help) usage; exit 0 ;;
            *) err "unknown option: $1"; usage; exit 2 ;;
        esac
    done

    [ -n "${HOME:-}" ] || die "HOME is not set"
    case "$PREFIX" in
        /*) ;;
        *) die "--prefix must be an absolute path" ;;
    esac
    case "$PREFIX" in
        *[[:space:]]* | *'|'* | *'&'* | *'\'*) die "--prefix must not contain spaces or | & \\" ;;
    esac
    case "$REPO" in
        */*) ;;
        *) die "--repo must look like OWNER/REPO" ;;
    esac
    safe_name "$(printf '%s' "$REPO" | tr '/' '-')" || die "invalid --repo"
    CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
    UNIT_DIR="$CONFIG_HOME/systemd/user"

    if [ "$UNINSTALL" = 1 ]; then
        do_uninstall
        return 0
    fi

    if [ -z "$PROFILE" ]; then
        if [ "$PROMPT" = 1 ] && have_tty; then
            choose_profile
        else
            err "no --profile given and nothing to ask on"
            usage
            exit 2
        fi
    fi
    case "$PROFILE" in
        gui | tui | cli | all) ;;
        *) err "unknown profile: $PROFILE"; usage; exit 2 ;;
    esac

    [ "$(uname -s)" = Linux ] || die "only Linux is supported"
    [ "$(uname -m)" = x86_64 ] || die "only x86_64 builds are published"

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT INT TERM

    # ---- get and verify the tarball
    if [ -n "$TARBALL" ]; then
        [ -f "$TARBALL" ] || die "no such file: $TARBALL"
        [ -f "$TARBALL.sha256" ] || die "missing checksum file: $TARBALL.sha256"
        archive=$TARBALL
        sumfile=$TARBALL.sha256
    else
        if [ -n "$VERSION" ]; then
            api="https://api.github.com/repos/$REPO/releases/tags/$VERSION"
        else
            api="https://api.github.com/repos/$REPO/releases/latest"
        fi
        say "Looking up the release ($api)"
        fetch "$api" "$tmp/release.json" || die "cannot read the release information"
        url=$(grep -o '"browser_download_url": *"[^"]*linux-x86_64\.tar\.gz"' "$tmp/release.json" | head -n1 | sed 's/.*"\(https[^"]*\)"$/\1/')
        [ -n "$url" ] || die "no linux-x86_64 release asset found"
        case "$url" in
            https://*) ;;
            *) die "unexpected download URL" ;;
        esac
        archive="$tmp/$(basename "$url")"
        sumfile="$archive.sha256"
        say "Downloading $url"
        fetch "$url" "$archive"
        fetch "$url.sha256" "$sumfile"
    fi
    verify_sum "$archive" "$sumfile"
    say "Checksum OK"

    # ---- unpack safely
    tar -tzf "$archive" >"$tmp/list" || die "cannot read the archive"
    while IFS= read -r entry; do
        case "$entry" in
            /* | ../* | */../* | *..) die "unsafe path in the archive: $entry" ;;
        esac
    done <"$tmp/list"
    mkdir "$tmp/x"
    tar -xzf "$archive" -C "$tmp/x"
    set -- "$tmp"/x/*
    [ $# -eq 1 ] && [ -d "$1" ] || die "unexpected archive layout"
    src=$1
    [ -f "$src/MANIFEST" ] || die "the archive has no MANIFEST (is it a release tarball?)"

    name=""; slug=""; version=""; bin_gui=""; bin_tui=""; bin_tray=""; bin_cli=""; bin_daemon=""
    while IFS='=' read -r k v; do
        case "$k" in
            name) name=$v ;;
            slug) slug=$v ;;
            version) version=$v ;;
            bin_gui) bin_gui=$v ;;
            bin_tui) bin_tui=$v ;;
            bin_tray) bin_tray=$v ;;
            bin_cli) bin_cli=$v ;;
            bin_daemon) bin_daemon=$v ;;
        esac
    done <"$src/MANIFEST"
    for v in "$slug" "$bin_gui" "$bin_tui" "$bin_tray" "$bin_cli" "$bin_daemon"; do
        safe_name "$v" || die "invalid MANIFEST value: $v"
    done
    [ -n "$name" ] || die "MANIFEST has no name"

    # ---- what to install
    bins="$bin_daemon"
    [ "$WITH_TRAY" = 1 ] && bins="$bins $bin_tray"
    with_gui=0; with_tui=0
    case "$PROFILE" in
        gui) bins="$bins $bin_gui"; with_gui=1 ;;
        tui) bins="$bins $bin_tui"; with_tui=1 ;;
        cli) bins="$bins $bin_cli" ;;
        all) bins="$bins $bin_gui $bin_tui $bin_cli"; with_gui=1; with_tui=1 ;;
    esac

    for b in $bins; do
        [ -f "$src/$b" ] || die "the archive lacks $b"
    done

    mkdir -p "$PREFIX/bin" "$PREFIX/share/install-receipts"
    receipt="$PREFIX/share/install-receipts/$slug.list"
    : >"$tmp/new-receipt"

    for b in $bins; do
        install -m 0755 "$src/$b" "$PREFIX/bin/$b"
        say "installed $PREFIX/bin/$b"
        echo "file $PREFIX/bin/$b" >>"$tmp/new-receipt"
    done

    # ---- desktop entry and icon (GUI profiles)
    if [ "$with_gui" = 1 ]; then
        mkdir -p "$PREFIX/share/applications" "$PREFIX/share/icons/hicolor/256x256/apps"
        # The TUI needs a terminal, which a Desktop Action cannot request, so it has no menu entry.
        awk -v tray="$WITH_TRAY" '
            /^Actions=/ { a = "Scheduler;"; if (tray == 1) a = a "Tray;"; print "Actions=" a; next }
            /^\[Desktop Action / { skip = 0; if ($0 == "[Desktop Action Tray]" && tray != 1) skip = 1 }
            skip { next }
            { print }
        ' "$src/$slug.desktop" | sed "s|^Exec=|Exec=$PREFIX/bin/|" >"$PREFIX/share/applications/$slug.desktop"
        install -m 0644 "$src/$slug.png" "$PREFIX/share/icons/hicolor/256x256/apps/$slug.png"
        say "installed $PREFIX/share/applications/$slug.desktop"
        echo "file $PREFIX/share/applications/$slug.desktop" >>"$tmp/new-receipt"
        echo "file $PREFIX/share/icons/hicolor/256x256/apps/$slug.png" >>"$tmp/new-receipt"
    fi

    # ---- systemd user units
    units="$slug-daemon.service"
    [ "$WITH_TRAY" = 1 ] && units="$units $slug-tray.service"
    if [ "$WITH_SYSTEMD" = 1 ]; then
        mkdir -p "$UNIT_DIR"
        for u in $units; do
            case "$u" in
                *-daemon.service) exe=$bin_daemon ;;
                *) exe=$bin_tray ;;
            esac
            sed "s|^ExecStart=.*|ExecStart=$PREFIX/bin/$exe|" "$src/$u" >"$UNIT_DIR/$u"
            say "installed $UNIT_DIR/$u"
            echo "file $UNIT_DIR/$u" >>"$tmp/new-receipt"
            echo "unit $u" >>"$tmp/new-receipt"
        done
        # Without --tray, make sure an earlier install's tray unit does not keep running.
        if [ "$WITH_TRAY" = 0 ] && [ -f "$UNIT_DIR/$slug-tray.service" ] && have systemctl; then
            systemctl --user disable --now "$slug-tray.service" >/dev/null 2>&1 || true
            rm -f "$UNIT_DIR/$slug-tray.service"
        fi
    fi

    # Keep earlier entries (installing another profile later only adds to the receipt).
    if [ -f "$receipt" ]; then
        cat "$receipt" "$tmp/new-receipt" | sort -u >"$tmp/merged"
    else
        sort -u "$tmp/new-receipt" >"$tmp/merged"
    fi
    cat "$tmp/merged" >"$receipt"

    # ---- start
    started=0
    if [ "$WITH_SYSTEMD" = 1 ]; then
        if have systemctl && systemctl --user show-environment >/dev/null 2>&1; then
            systemctl --user daemon-reload
            for u in $units; do
                systemctl --user enable "$u" >/dev/null 2>&1 || err "could not enable $u"
                # restart: an upgrade must replace a running old process
                systemctl --user restart "$u" || err "could not start $u (see: journalctl --user -u $u)"
            done
            started=1
        else
            err "no systemd user session found; files are in place but nothing was enabled"
        fi
    fi

    say ""
    say "$name $version installed (profile: $PROFILE)."
    case ":$PATH:" in
        *":$PREFIX/bin:"*) ;;
        *) say "Note: $PREFIX/bin is not on your PATH; add it to run the programs by name." ;;
    esac
    if [ "$started" = 1 ]; then
        say "Scheduler: systemctl --user status $slug-daemon.service"
    else
        say "Start the scheduler yourself: $PREFIX/bin/$bin_daemon"
    fi
    [ "$WITH_TRAY" = 1 ] && say "The tray needs a StatusNotifier host (KDE, Waybar; GNOME needs the AppIndicator extension)."
    say "Uninstall: sh install.sh --uninstall"
}

main "$@"
