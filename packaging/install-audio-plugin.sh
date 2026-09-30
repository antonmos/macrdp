#!/usr/bin/env bash
# install-audio-plugin.sh — install (or remove) the "macrdp Microphone" CoreAudio
# AudioServerPlugIn into the system HAL plug-ins directory and restart coreaudiod
# so it loads. The audio analogue of install-ifd-handler.sh.
#
# The plug-in is a plain CFPlugIn `.driver` — no entitlement, no provisioning
# profile — so installing is a file copy into a root-owned system dir plus a
# coreaudiod restart. A single GUI admin prompt covers the privileged step (no
# manual sudo). macrdp.app ships the driver and this script side by side in
# Contents/Resources (packaging/make-app.sh); the menu-bar controller runs the
# embedded copy. From a checkout, packaging/make-audio-plugin.sh builds it to
# target/macrdp-mic.driver.
#
# Usage:
#   packaging/install-audio-plugin.sh              # install
#   packaging/install-audio-plugin.sh --uninstall  # remove
#
# Env:
#   APP_DIR=/Applications   where macrdp.app is installed (to find an embedded copy)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
APP_DIR="${APP_DIR:-/Applications}"

HAL_DIR="/Library/Audio/Plug-Ins/HAL"
DEST="$HAL_DIR/macrdp-mic.driver"

# GUI admin prompt so no manual sudo is needed. $1 must be pre-escaped by callers.
run_admin() {
    /usr/bin/osascript -e "do shell script \"$1\" with administrator privileges" >/dev/null
}

# Restart coreaudiod so it drops the old plug-in and rescans the HAL dir. launchd
# respawns it immediately; a brief (~1 s) glitch on other audio is expected.
RESTART_COREAUDIO="launchctl kill SIGTERM system/com.apple.audio.coreaudiod 2>/dev/null || killall coreaudiod 2>/dev/null || true"

if [ "${1:-}" = "--uninstall" ]; then
    echo "==> removing $DEST + restarting coreaudiod"
    run_admin "rm -rf '$DEST'; $RESTART_COREAUDIO"
    echo "    done — 'macrdp Microphone' will disappear from input pickers."
    exit 0
fi

# Locate the built bundle: next to this script (running from inside
# macrdp.app/Contents/Resources), then an installed/staged macrdp.app, then the
# fresh build under target/.
SRC=""
for cand in \
    "$SCRIPT_DIR/macrdp-mic.driver" \
    "$APP_DIR/macrdp.app/Contents/Resources/macrdp-mic.driver" \
    "$REPO_ROOT/target/macrdp.app/Contents/Resources/macrdp-mic.driver" \
    "$REPO_ROOT/target/macrdp-mic.driver"; do
    if [ -d "$cand" ]; then SRC="$cand"; break; fi
done
if [ -z "$SRC" ]; then
    echo "error: macrdp-mic.driver not found — build it first:" >&2
    echo "         packaging/make-audio-plugin.sh" >&2
    exit 1
fi
echo "==> installing $SRC"

# Stage a clean copy (strip quarantine + any xattrs the copy might carry), then
# install it root-owned in one privileged step.
STAGE_DIR="$(mktemp -d)"
STAGED="$STAGE_DIR/macrdp-mic.driver"
cp -R "$SRC" "$STAGED"
xattr -cr "$STAGED" 2>/dev/null || true

run_admin "mkdir -p '$HAL_DIR' && rm -rf '$DEST' && cp -R '$STAGED' '$DEST' && chown -R root:wheel '$DEST' && $RESTART_COREAUDIO"
rm -rf "$STAGE_DIR"

echo ""
echo "==> installed $DEST"
echo "    'macrdp Microphone' should now appear in Audio MIDI Setup and app input"
echo "    pickers (Zoom / FaceTime / QuickTime). It is silent until macrdp is"
echo "    running with --enable-microphone-redirection and a client is streaming"
echo "    its mic; then it carries the client's audio."
echo ""
echo "Uninstall:  packaging/install-audio-plugin.sh --uninstall"
