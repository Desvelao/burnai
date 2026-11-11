#!/bin/sh
# Exercises install.sh against a tarball packed from `make dist` output, with a throwaway HOME and
# --no-systemd. No network, no real systemd. Usage: tests/install_test.sh [dist-dir]
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DIST=${1:-$ROOT/dist}
INSTALL="$ROOT/install.sh"
[ -f "$DIST/MANIFEST" ] || { echo "run 'make dist' first ($DIST/MANIFEST missing)"; exit 1; }

W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
fails=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { # check DESC CMD...
    d=$1; shift
    if "$@" >/dev/null 2>&1; then ok "$d"; else bad "$d"; fi
}
refuses() { # refuses DESC CMD...
    d=$1; shift
    if "$@" >/dev/null 2>&1; then bad "$d"; else ok "$d"; fi
}

get() { sed -n "s/^$1=//p" "$DIST/MANIFEST"; }
SLUG=$(get slug); VER=$(get version)
GUI=$(get bin_gui); TUI=$(get bin_tui); TRAY=$(get bin_tray); CLI=$(get bin_cli); DAEMON=$(get bin_daemon)

# Pack like the release workflow does.
NAME="$SLUG-$VER-linux-x86_64"
mkdir -p "$W/pkg/$NAME"
cp -r "$DIST/." "$W/pkg/$NAME/"
cp "$ROOT/install.sh" "$W/pkg/$NAME/"
tar -czf "$W/$NAME.tar.gz" -C "$W/pkg" "$NAME"
(cd "$W" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
TB="$W/$NAME.tar.gz"

run() { # run HOME ARGS...   (no controlling terminal, no systemd)
    h=$1; shift
    mkdir -p "$h"
    HOME=$h XDG_CONFIG_HOME=$h/.config sh "$INSTALL" --no-prompt --tarball "$TB" --no-systemd "$@" </dev/null
}
has() { [ -e "$1" ]; }
hasnt() { [ ! -e "$1" ]; }

# --- profiles
for prof in gui tui cli all; do
    H="$W/home-$prof"
    run "$H" --profile "$prof" >/dev/null 2>&1 || bad "profile $prof installs"
    B="$H/.local/bin"
    check "$prof: daemon installed" has "$B/$DAEMON"
    check "$prof: tray installed" has "$B/$TRAY"
    case $prof in gui | all) check "$prof: gui installed" has "$B/$GUI"; check "$prof: desktop entry" has "$H/.local/share/applications/$SLUG.desktop"; check "$prof: icon" has "$H/.local/share/icons/hicolor/256x256/apps/$SLUG.png" ;;
        *) refuses "$prof: no gui" test -e "$B/$GUI"; refuses "$prof: no desktop entry" test -e "$H/.local/share/applications/$SLUG.desktop" ;; esac
    case $prof in tui | all) check "$prof: tui installed" has "$B/$TUI" ;; *) refuses "$prof: no tui" test -e "$B/$TUI" ;; esac
    case $prof in cli | all) check "$prof: cli installed" has "$B/$CLI" ;; *) refuses "$prof: no cli" test -e "$B/$CLI" ;; esac
done

# desktop actions follow the profile
D="$W/home-gui/.local/share/applications/$SLUG.desktop"
check "gui desktop: scheduler action" grep -q 'Desktop Action Scheduler' "$D"
refuses "gui desktop: no TUI action" grep -q 'Desktop Action Tui' "$D"
refuses "all desktop: no TUI action (a menu entry cannot open a terminal)" grep -q 'Desktop Action Tui' "$W/home-all/.local/share/applications/$SLUG.desktop"
check "desktop Exec is absolute" grep -q "^Exec=$W/home-gui/.local/bin/$GUI\$" "$D"

# --- --no-tray and --prefix
H="$W/home-notray"
run "$H" --profile tui --no-tray --prefix "$W/custom" >/dev/null 2>&1 || bad "prefix/no-tray install"
check "prefix honoured" has "$W/custom/bin/$DAEMON"
refuses "no-tray: tray not installed" test -e "$W/custom/bin/$TRAY"

# --- systemd unit rendering (copy files only; systemctl is not called)
H="$W/home-units"
mkdir -p "$H"
HOME=$H XDG_CONFIG_HOME=$H/.config sh "$INSTALL" --tarball "$TB" --profile cli --prefix "$W/p2" </dev/null >/dev/null 2>&1 || true
U="$H/.config/systemd/user/$SLUG-daemon.service"
check "daemon unit installed" has "$U"
check "unit ExecStart honours prefix" grep -qx "ExecStart=$W/p2/bin/$DAEMON" "$U"
check "tray unit installed" has "$H/.config/systemd/user/$SLUG-tray.service"

# --- refusals
cp "$TB" "$W/bad.tar.gz"; echo "0000  bad.tar.gz" > "$W/bad.tar.gz.sha256"
refuses "bad checksum refused" env HOME="$W/h-bad" sh "$INSTALL" --tarball "$W/bad.tar.gz" --profile gui --no-systemd
refuses "missing checksum file refused" env HOME="$W/h-bad" sh "$INSTALL" --tarball "$TB.nope" --profile gui --no-systemd
set +e
HOME="$W/h-np" sh "$INSTALL" --no-prompt --tarball "$TB" --no-systemd </dev/null >/dev/null 2>&1
rc=$?
set -e
[ "$rc" = 2 ] && ok "no profile with --no-prompt exits 2" || bad "no profile with --no-prompt exits 2 (got $rc)"
refuses "unknown profile refused" run "$W/h-up" --profile nope
refuses "relative prefix refused" run "$W/h-rp" --profile gui --prefix rel
refuses "prefix with a space refused" run "$W/h-sp" --profile gui --prefix "$W/a b"

# --- unsafe archive paths
mkdir -p "$W/evil/x"; echo hi > "$W/evil/x/f"
tar -czf "$W/evil.tar.gz" -C "$W/evil" --transform 's|^x/|../|' x/f 2>/dev/null || tar -czf "$W/evil.tar.gz" -C "$W/evil" x/f
(cd "$W" && sha256sum evil.tar.gz > evil.tar.gz.sha256)
refuses "archive without MANIFEST/with unsafe layout refused" env HOME="$W/h-ev" sh "$INSTALL" --tarball "$W/evil.tar.gz" --profile gui --no-systemd

# --- data is untouched and uninstall removes exactly what was installed
H="$W/home-all"
mkdir -p "$H/.local/share/$SLUG"; echo keep > "$H/.local/share/$SLUG/data.db"
HOME=$H XDG_CONFIG_HOME=$H/.config sh "$INSTALL" --uninstall --no-systemd </dev/null >/dev/null 2>&1 || bad "uninstall runs"
for f in "$DAEMON" "$TRAY" "$GUI" "$TUI" "$CLI"; do refuses "uninstall removed $f" test -e "$H/.local/bin/$f"; done
refuses "uninstall removed desktop entry" test -e "$H/.local/share/applications/$SLUG.desktop"
check "uninstall keeps user data" grep -qx keep "$H/.local/share/$SLUG/data.db"

# --- later install of another profile merges the receipt
H="$W/home-merge"
run "$H" --profile gui >/dev/null 2>&1; run "$H" --profile cli >/dev/null 2>&1
HOME=$H sh "$INSTALL" --uninstall --no-systemd </dev/null >/dev/null 2>&1 || true
refuses "merged uninstall removes the first profile's files" test -e "$H/.local/bin/$GUI"
refuses "merged uninstall removes the second profile's files" test -e "$H/.local/bin/$CLI"

if [ "$fails" -ne 0 ]; then echo "$fails check(s) failed"; exit 1; fi
echo "install-test: ok"
