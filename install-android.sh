#!/usr/bin/env bash
# Build the moreland Android app and install it on every connected device.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ANDROID_DIR="$REPO/android"
APK="$ANDROID_DIR/app/build/outputs/apk/release/app-release.apk"
DRAW_OVER_CUTOUT=true

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m warn\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror\033[0m %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --no-draw-over-cutout) DRAW_OVER_CUTOUT=false ;;
        --draw-over-cutout)    DRAW_OVER_CUTOUT=true  ;;
        -h|--help)
            cat <<'EOF'
Usage: install-android.sh [--no-draw-over-cutout]

  --draw-over-cutout      Build with windowLayoutInDisplayCutoutMode=always
                          (the default).
  --no-draw-over-cutout   Build with the attribute left at the system default,
                          so the window is letterboxed around the cutout.
EOF
            exit 0
            ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
    shift
done

command -v adb >/dev/null 2>&1 \
    || die "adb not found on PATH.
         Install it with: pacman -S android-tools  (Arch)
                          apt install adb           (Debian/Ubuntu)"

if [ -z "${ANDROID_HOME:-}" ]; then
    if [ -f "$ANDROID_DIR/local.properties" ] \
        && grep -q '^sdk.dir=' "$ANDROID_DIR/local.properties"; then
        :
    elif [ -d "$HOME/Android/Sdk" ]; then
        ANDROID_HOME="$HOME/Android/Sdk"
    elif [ -d "/opt/android-sdk" ]; then
        ANDROID_HOME="/opt/android-sdk"
    else
        die "ANDROID_HOME is not set and no SDK was found.
         Install the Android SDK with platforms;android-34 and
         build-tools;34.0.0, then either export ANDROID_HOME or run:
             ANDROID_HOME=/path/to/sdk $0"
    fi
    [ -n "${ANDROID_HOME:-}" ] && export ANDROID_HOME
fi
say "Android SDK: ${ANDROID_HOME:-<from android/local.properties>}"

DEVICES=$(adb devices | awk 'NR>1 && $2=="device" {print $1}')
if [ -z "$DEVICES" ]; then
    die "No ADB devices in 'device' state.
         Plug the tablet in, enable USB debugging, accept the RSA prompt.
         Check with: adb devices -l"
fi
DEVICE_COUNT=$(printf '%s\n' "$DEVICES" | wc -l)
say "Found $DEVICE_COUNT device(s)"

say "Building the APK (drawOverCutout=$DRAW_OVER_CUTOUT)"
( cd "$ANDROID_DIR" && ./gradlew assembleRelease -PdrawOverCutout="$DRAW_OVER_CUTOUT" )

[ -f "$APK" ] || die "Build produced no APK at $APK"
say "APK: $APK  (drawOverCutout=$DRAW_OVER_CUTOUT)"

FAILED=""
for serial in $DEVICES; do
    say "Installing on $serial"
    if adb -s "$serial" install -r "$APK"; then
        say "  $serial: ok"
    else
        warn "  $serial: install failed"
        if adb -s "$serial" push "$APK" /sdcard/Download/moreland.apk >/dev/null 2>&1; then
            printf '    Staged for sideload. On the tablet:\n'
            printf '      Files -> Downloads -> moreland.apk -> Install\n'
        fi
        FAILED="$FAILED $serial"
    fi
done

if [ -z "$FAILED" ]; then
    say "Done: installed on all $DEVICE_COUNT device(s)."
else
    warn "Failed on:$FAILED"
    exit 1
fi
