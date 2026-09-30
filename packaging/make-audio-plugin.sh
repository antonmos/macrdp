#!/usr/bin/env bash
# make-audio-plugin.sh — build the "macrdp Microphone" CoreAudio AudioServerPlugIn
# as a hand-assembled `.driver` bundle (NO Xcode), the audio analogue of
# make-camera-extension.sh.
#
# The plug-in is a plain CFPlugIn loaded by coreaudiod from
# /Library/Audio/Plug-Ins/HAL/. Unlike the camera system extension or the USB
# host-controller path it needs NO entitlement and NO provisioning profile — it
# installs like the IFD handler (a file copy + a coreaudiod restart, see
# packaging/install-audio-plugin.sh). It should still be Developer-ID signed so
# coreaudiod on a stock Mac will load it.
#
# Env:
#   CODESIGN_IDENTITY   Developer ID Application name (default "-" = ad-hoc, fine
#                       for local testing on the build Mac; a stock/other Mac
#                       wants a real identity).
#   ARCHS               space-separated arches (default "arm64 x86_64" universal).
#   OUT_DIR             where to drop macrdp-mic.driver (default $REPO_ROOT/target).
#
# Usage:
#   packaging/make-audio-plugin.sh
#   CODESIGN_IDENTITY="Developer ID Application: … (QGLA89KHM7)" packaging/make-audio-plugin.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PKG_DIR="$REPO_ROOT/packaging"
SRC="$REPO_ROOT/audioplugin/macrdp_mic.c"
IDENTITY="${CODESIGN_IDENTITY:--}"
ARCHS="${ARCHS:-arm64 x86_64}"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/target}"
DRIVER="$OUT_DIR/macrdp-mic.driver"

echo "==> macrdp Microphone AudioServerPlugIn"
echo "    src=$SRC  archs=$ARCHS  identity=$IDENTITY"

# Assemble a fresh bundle skeleton.
rm -rf "$DRIVER"
mkdir -p "$DRIVER/Contents/MacOS"

# Compile the plug-in as a Mach-O BUNDLE (dlopen'd by coreaudiod). The factory
# symbol (MacRDPMic_Create) stays exported so CFBundleGetFunctionPointerForName
# can find it.
ARCH_FLAGS=()
for a in $ARCHS; do ARCH_FLAGS+=(-arch "$a"); done
echo "==> clang -bundle"
clang -bundle "${ARCH_FLAGS[@]}" \
    -mmacosx-version-min=12.3 \
    -O2 -fobjc-arc -Wall -Wextra \
    -framework CoreAudio -framework CoreFoundation \
    -o "$DRIVER/Contents/MacOS/macrdp-mic" \
    "$SRC"

# Monotonic build number (epoch seconds) so each rebuild's CFBundleVersion is
# strictly greater — coreaudiod, like sysextd, will not reload a plug-in whose
# CFBundleVersion hasn't increased.
BUILD="$(date +%s)"
sed "s/__BUILD__/$BUILD/" "$PKG_DIR/audio-plugin-Info.plist" > "$DRIVER/Contents/Info.plist"
echo "    CFBundleVersion (build) = $BUILD"

# Sign (hardened runtime; NO entitlements — a HAL plug-in needs none).
# No secure timestamp for ad-hoc or the self-signed dev identity (as make-app.sh).
if [ "$IDENTITY" = "-" ] || [ "$IDENTITY" = "macrdp-dev" ]; then TS="--timestamp=none"; else TS="--timestamp"; fi
echo "==> codesign (hardened runtime, no entitlements)"
codesign --force --options runtime $TS -s "$IDENTITY" "$DRIVER/Contents/MacOS/macrdp-mic"
codesign --force --options runtime $TS -s "$IDENTITY" "$DRIVER"
codesign --verify --strict "$DRIVER"

echo "==> built $DRIVER"
codesign -dv "$DRIVER" 2>&1 | sed 's/^/    /' | head -8
echo ""
echo "Install it with:  sudo packaging/install-audio-plugin.sh"
