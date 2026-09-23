#!/usr/bin/env bash
# Report whether this machine can run moreland, and if not, what blocks it.
#
# Nothing here writes anything or needs root — it only reads. The distro is
# detected solely to name packages; it is not what decides the answer. See the
# note under "Distribution" for why.
set -uo pipefail

pass() { printf '  \033[1;32m✓\033[0m %s\n' "$*"; }
fail() { printf '  \033[1;31m✗\033[0m %s\n' "$*"; }
warn() { printf '  \033[1;33m!\033[0m %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }
head_() { printf '\n\033[1;36m%s\033[0m\n' "$*"; }

BLOCKERS=()
block() { BLOCKERS+=("$1"); }

# Set by the encoder section, read by the distribution section so the
# package recommendations match what the daemon will actually use.
# "nvenc", "vaapi", or "unknown".
ENCODER_BACKEND="unknown"

# XDG_CURRENT_DESKTOP is colon-separated and case varies; mirror the daemon's
# own detection. `desktop_has KDE` matches "KDE", "kde", and "plasma:KDE".
desktop_has() {
    local want="${1,,}" comp
    local -a comps
    IFS=: read -ra comps <<< "${XDG_CURRENT_DESKTOP:-}"
    for comp in "${comps[@]}"; do
        [ "${comp,,}" = "$want" ] && return 0
    done
    return 1
}

# ---------------------------------------------------------------- session ---
head_ "Session"

if [ "${XDG_SESSION_TYPE:-}" = "wayland" ] && [ -n "${WAYLAND_DISPLAY:-}" ]; then
    pass "Wayland session (WAYLAND_DISPLAY=$WAYLAND_DISPLAY)"
else
    fail "not a Wayland session (XDG_SESSION_TYPE=${XDG_SESSION_TYPE:-unset})"
    info "moreland is Wayland-only; there is no X11 path."
    block "not running Wayland"
fi

# Confirm each environment marker with a live IPC round-trip. A stale
# HYPRLAND_INSTANCE_SIGNATURE inherited by a long-lived systemd user service
# otherwise names a compositor that exited hours ago.
COMPOSITOR="unsupported"
if { [ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ] || desktop_has Hyprland; } \
   && hyprctl version >/dev/null 2>&1; then
    COMPOSITOR="Hyprland"
elif { [ -n "${SWAYSOCK:-}" ] || desktop_has sway; } \
     && swaymsg -t get_version >/dev/null 2>&1; then
    COMPOSITOR="Sway"
elif { desktop_has labwc || desktop_has wlroots; } \
     && wlr-randr >/dev/null 2>&1; then
    COMPOSITOR="labwc"
elif { desktop_has KDE || [ -n "${KDE_FULL_SESSION:-}" ]; } \
     && kscreen-doctor -o >/dev/null 2>&1; then
    COMPOSITOR="Kwin"
fi

case "$COMPOSITOR" in
    Hyprland) pass "compositor: Hyprland — supported, verified" ;;
    Sway)     warn "compositor: Sway — capture should work, output creation unimplemented"
              block "Sway virtual-output creation is not implemented" ;;
    Kwin)     pass "compositor: KWin — supported via zkde_screencast_unstable_v1"
              info "Capture goes through PipeWire; ext-image-copy-capture is not used." ;;
    labwc)    pass "compositor: labwc — supported, community-tested"
              info "labwc cannot create an output at runtime. Start it with"
              info "WLR_HEADLESS_OUTPUTS=1 and pass the name wlr-randr lists"
              info "(usually HEADLESS-1) as: moreland --output-name HEADLESS-1" ;;
    *)        fail "compositor: ${XDG_CURRENT_DESKTOP:-unknown} — no virtual-output backend"
              block "no virtual-output backend for ${XDG_CURRENT_DESKTOP:-unknown}" ;;
esac

if [ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ] && [ "$COMPOSITOR" != "Hyprland" ]; then
    warn "HYPRLAND_INSTANCE_SIGNATURE is set but no Hyprland answers"
    info "Stale environment from an earlier session. Harmless here, but a"
    info "systemd user service started under Hyprland and left enabled will"
    info "inherit it. Reset with: systemctl --user import-environment"
fi

# ------------------------------------------------------- capture protocol ---
# The capture path depends on the compositor: ext-image-copy-capture-v1 for
# Hyprland/wlroots, zkde_screencast_unstable_v1 + PipeWire for KWin. Checking
# the wrong one on KWin reports a blocker that does not exist.
if [ "$COMPOSITOR" = "Kwin" ]; then
    head_ "Capture path (KDE Plasma: zkde_screencast_unstable_v1 + PipeWire)"

    # KWin only advertises the interface to a client whose desktop entry
    # declares it, and denies silently otherwise. The grant, not the protocol,
    # is the thing that can fail.
    ENTRY="$HOME/.local/share/applications/moreland.desktop"
    if [ -f "$ENTRY" ]; then
        pass "desktop entry installed ($ENTRY)"
        exec_line=$(grep -m1 '^Exec=' "$ENTRY" 2>/dev/null | cut -d= -f2-)
        case "$exec_line" in
            /*) pass "Exec is an absolute path ($exec_line)" ;;
            *)  fail "Exec is not absolute: '$exec_line'"
                info "KWin silently denies the interface for a bare command name."
                block "desktop entry Exec is not an absolute path" ;;
        esac
        if grep -q 'X-KDE-Wayland-Interfaces=.*zkde_screencast_unstable_v1' "$ENTRY"; then
            pass "declares zkde_screencast_unstable_v1"
        else
            fail "does not declare zkde_screencast_unstable_v1"
            block "desktop entry does not declare the screencast interface"
        fi
    else
        fail "no desktop entry at $ENTRY"
        info "Run ./install.sh on the Plasma session, then kbuildsycoca6 --noincremental."
        block "no KDE desktop entry; KWin will not grant the screencast interface"
    fi

    if command -v kbuildsycoca6 >/dev/null 2>&1; then
        pass "kbuildsycoca6 present (KWin reads the service cache, not the directory)"
    else
        warn "kbuildsycoca6 missing; a new desktop entry may not be picked up"
        info "Install with your distro's kservice package (Arch: kservice)."
    fi

    # kscreen-doctor is what Compositor::detect round-trips against to confirm
    # KWin, so it is a runtime dependency of the daemon itself, not just the
    # doctor.
    if command -v kscreen-doctor >/dev/null 2>&1; then
        pass "kscreen-doctor present (used by the daemon's KWin detection)"
    else
        fail "kscreen-doctor not found"
        info "The daemon uses this to identify KWin. Arch: pacman -S kscreen"
        block "kscreen-doctor not found; the daemon cannot identify KWin"
    fi

    # The XML is a build-time dependency for the plasma feature. A binary
    # built without it refuses to talk to KWin, and the daemon reports that
    # clearly; the check here is informational, for someone about to build.
    XML_FOUND=$(find /usr/share/plasma-wayland-protocols \
                     /usr/local/share/plasma-wayland-protocols \
                     /run/current-system/sw/share/plasma-wayland-protocols \
                     -name 'zkde-screencast-unstable-v1.xml' -print -quit 2>/dev/null)
    if [ -n "$XML_FOUND" ]; then
        pass "plasma-wayland-protocols XML ($XML_FOUND)"
    else
        warn "plasma-wayland-protocols XML not found"
        info "Build-time only: needed to compile 'cargo build --features plasma'."
        info "Arch: pacman -S plasma-wayland-protocols"
    fi

    # PipeWire: the consumer connects to the running daemon. The socket is
    # the runtime fact; the .pc file is the build-time one.
    PW_SOCKET="/run/user/$(id -u)/pipewire-0"
    if [ -S "$PW_SOCKET" ] || pgrep -x pipewire >/dev/null 2>&1; then
        pass "PipeWire daemon running"
    else
        warn "PipeWire daemon does not appear to be running ($PW_SOCKET missing)"
        info "The capture consumer connects to the running PipeWire daemon."
        info "Arch: systemctl --user enable --now pipewire wireplumber"
    fi
    if pkg-config --exists libpipewire-0.3 2>/dev/null; then
        pass "libpipewire-0.3 development headers (build-time)"
    else
        warn "libpipewire-0.3.pc not found"
        info "Needed to build the 'pipewire' crate. Arch: pacman -S pipewire"
    fi

    # Did the installed binary actually get built with the plasma feature?
    # A binary built without it fails on KWin with a clear message, but
    # telling the user before they plug the tablet in is friendlier.
    MORELAND_BIN=""
    for candidate in "$HOME/.local/bin/moreland" "./target/release/moreland"; do
        [ -x "$candidate" ] && MORELAND_BIN="$candidate" && break
    done
    if [ -n "$MORELAND_BIN" ]; then
        if grep -qa 'zkde_screencast_unstable_v1' "$MORELAND_BIN" 2>/dev/null; then
            pass "installed moreland references zkde_screencast_unstable_v1 (plasma feature built)"
        else
            warn "installed moreland does not reference zkde_screencast_unstable_v1"
            info "This build cannot create virtual outputs on KWin. Rebuild with:"
            info "  ./install.sh    (or: cargo build --release --features plasma)"
        fi
    fi
else
    head_ "Capture protocol (ext-image-copy-capture-v1)"

    if ! command -v wayland-info >/dev/null 2>&1; then
        warn "wayland-info not installed — cannot check (package: wayland-utils)"
    else
        PROTOCOLS=$(wayland-info 2>/dev/null | grep -oP "interface: '\K[^']+")
        have() { printf '%s\n' "$PROTOCOLS" | grep -qx "$1"; }

        if have ext_image_copy_capture_manager_v1; then
            pass "ext_image_copy_capture_manager_v1"
        else
            fail "ext_image_copy_capture_manager_v1 — ABSENT"
            block "compositor does not implement ext-image-copy-capture-v1"
        fi

        if have ext_output_image_capture_source_manager_v1; then
            pass "ext_output_image_capture_source_manager_v1"
        else
            fail "ext_output_image_capture_source_manager_v1 — ABSENT"
            block "compositor does not implement ext-output-image-capture-source-manager-v1"
        fi

        if have zwp_linux_dmabuf_v1; then
            pass "zwp_linux_dmabuf_v1 (zero-copy import)"
        else
            fail "zwp_linux_dmabuf_v1 — ABSENT"
            block "no linux-dmabuf; the zero-copy path cannot work"
        fi
    fi
fi

# ---------------------------------------------------------------- encoder ---
head_ "Encoder (hardware H.264)"

# `encoder::select_backend` prefers NVENC whenever `nvh264enc` is present,
# because on an NVIDIA machine there is no usable VA-API encode path:
# `nvidia-vaapi-driver` is decode-only, and `vah264enc` either fails to
# create or produces a stream nothing can decode. Reporting the VA-API
# elements missing on such a machine is a false blocker — the daemon does
# not use them. Mirror the selection here.
if ! command -v gst-inspect-1.0 >/dev/null 2>&1; then
    fail "gst-inspect-1.0 not found"
    block "GStreamer is not installed"
elif [ -e /proc/driver/nvidia/version ] \
     && gst-inspect-1.0 nvh264enc >/dev/null 2>&1; then
    ENCODER_BACKEND="nvenc"
    info "encoder path: NVENC via GL — nvh264enc present, the daemon selects"
    info "             this over VA-API (see crates/encoder/src/lib.rs)"
    for element in glupload glcolorconvert gldownload nvh264enc h264parse; do
        if gst-inspect-1.0 "$element" >/dev/null 2>&1; then
            pass "GStreamer element: $element"
        else
            fail "GStreamer element missing: $element"
            block "NVENC path needs GStreamer element $element"
        fi
    done
    # `glupload`'s DMA-BUF importer is what reads the NVIDIA tiling modifier
    # the compositor puts on the capture buffer. If the elements above exist,
    # GL support is present; there is nothing further to check.
else
    ENCODER_BACKEND="vaapi"
    info "encoder path: VA-API — nvh264enc not found, the daemon uses"
    info "             vapostproc + vah264enc"
    for element in vapostproc vah264enc h264parse; do
        if gst-inspect-1.0 "$element" >/dev/null 2>&1; then
            pass "GStreamer element: $element"
        else
            fail "GStreamer element missing: $element"
            block "VA-API path needs GStreamer element $element"
        fi
    done

    if command -v vainfo >/dev/null 2>&1; then
        if vainfo 2>/dev/null | grep -q 'VAProfileH264.*VAEntrypointEncSlice'; then
            pass "VA-API H.264 encode: $(vainfo 2>/dev/null | grep -oP 'Driver version: \K.*' | head -1)"
        else
            fail "no VA-API H.264 encode entrypoint"
            block "GPU/driver exposes no VA-API H.264 encoder"
        fi
    else
        warn "vainfo not installed — cannot confirm VA-API (package: libva-utils)"
    fi
fi

# Either path needs a render node: VA-API opens one directly, and the GL
# path imports DMA-BUFs whose fds were minted on one.
if ls /dev/dri/renderD* >/dev/null 2>&1; then
    pass "render nodes: $(ls -m /dev/dri/renderD* 2>/dev/null)"
else
    fail "no /dev/dri/renderD* render node"
    block "no DRM render node"
fi

# --------------------------------------------------------------- transport ---
head_ "Transport (ADB)"

if command -v adb >/dev/null 2>&1; then
    pass "adb present: $(adb version 2>/dev/null | head -1)"
    DEVICES=$(adb devices 2>/dev/null | awk 'NR>1 && $2=="device" {print $1}')
    if [ -n "$DEVICES" ]; then
        for serial in $DEVICES; do
            MODEL=$(adb -s "$serial" shell getprop ro.product.model 2>/dev/null | tr -d '\r')
            SIZE=$(adb -s "$serial" shell wm size 2>/dev/null | tr -d '\r' | head -1)
            pass "device $serial — ${MODEL:-unknown} (${SIZE:-size unknown})"
            if adb -s "$serial" shell pm list packages com.moreland.display 2>/dev/null \
                | grep -q com.moreland.display; then
                pass "  tablet app installed"
            else
                warn "  tablet app NOT installed — see README 'Install'"
            fi
        done
    else
        warn "no authorised device — plug the tablet in and accept the USB-debugging prompt"
    fi
else
    fail "adb not found"
    block "adb is not installed"
fi

# ------------------------------------------------------------ distribution ---
head_ "Distribution"

DISTRO_ID=$(. /etc/os-release 2>/dev/null && echo "${ID:-unknown}")
DISTRO_LIKE=$(. /etc/os-release 2>/dev/null && echo "${ID_LIKE:-}")
DISTRO_NAME=$(. /etc/os-release 2>/dev/null && echo "${PRETTY_NAME:-unknown}")
info "$DISTRO_NAME (kernel $(uname -r))"
info ""
info "The distribution is not what decides this. Every check above depends on"
info "the compositor, the GStreamer version and the video driver, and each of"
info "those ships on every mainstream distro. A distro matters only in that it"
info "picks your default desktop and how new your GStreamer is."
info ""
info "Requirements, whatever the distro:"
info "  • Hyprland (any recent release) or another compositor implementing"
info "    ext-image-copy-capture-v1 — wlroots 0.18+ compositors do"
info "  • KDE Plasma / KWin 6.x — supported via zkde_screencast_unstable_v1,"
info "    which needs plasma-wayland-protocols at build time and a running"
info "    PipeWire at runtime"
info "  • GStreamer 1.22+; the daemon picks the encoder from what is installed:"
case "$ENCODER_BACKEND" in
    nvenc)
        info "      NVENC via GL — needs nvh264enc, glupload, glcolorconvert,"
        info "      gldownload (all in gst-plugins-bad on most distros)."
        info "      nvidia-vaapi-driver is NOT used: it is decode-only."
        ;;
    vaapi)
        info "      VA-API — needs the va plugin (vapostproc, vah264enc) and a"
        info "      VA-API driver: Mesa radeonsi/iHD, or nvidia-vaapi-driver."
        ;;
    *)
        info "      VA-API (AMD/Intel: vapostproc + vah264enc) or NVENC-via-GL"
        info "      (NVIDIA: nvh264enc + glupload + glcolorconvert + gldownload)."
        ;;
esac

case "$DISTRO_ID $DISTRO_LIKE" in
    *arch*)
        info ""
        info "Install (verified on this family):"
        case "$ENCODER_BACKEND" in
            nvenc)
                info "  sudo pacman -S --needed rust gstreamer gst-plugins-base \\"
                info "      gst-plugins-good gst-plugins-bad \\"
                info "      android-tools android-udev wayland-utils"
                info "  # gst-plugins-bad carries nvh264enc, glupload and gldownload."
                info "  # If your GPU also has a VA-API driver and you want to try it:"
                info "  #   sudo pacman -S --needed gst-plugin-va libva libva-utils"
                ;;
            vaapi)
                info "  sudo pacman -S --needed rust gstreamer gst-plugins-base \\"
                info "      gst-plugins-good gst-plugin-va libva libva-utils \\"
                info "      android-tools android-udev wayland-utils"
                ;;
            *)
                info "  sudo pacman -S --needed rust gstreamer gst-plugins-base \\"
                info "      gst-plugins-good gst-plugin-va libva libva-utils \\"
                info "      android-tools android-udev wayland-utils"
                info "  # for NVIDIA, swap gst-plugin-va libva libva-utils for gst-plugins-bad"
                ;;
        esac
        if [ "$COMPOSITOR" = "Kwin" ]; then
            info "  # KDE Plasma additionally needs:"
            info "  sudo pacman -S --needed plasma-wayland-protocols pipewire kservice"
        fi
        ;;
    *fedora*|*rhel*)
        info ""
        info "Install (UNVERIFIED — package names are best-effort):"
        info "  sudo dnf install rust cargo gstreamer1-plugins-base \\"
        info "      gstreamer1-plugins-good gstreamer1-plugins-bad-free \\"
        info "      libva libva-utils android-tools wayland-utils"
        if [ "$COMPOSITOR" = "Kwin" ]; then
            info "  # KDE Plasma additionally needs:"
            info "  sudo dnf install plasma-wayland-protocols pipewire kf6-kservice"
        fi
        ;;
    *debian*|*ubuntu*)
        info ""
        info "Install (UNVERIFIED — package names are best-effort):"
        info "  sudo apt install rustc cargo gstreamer1.0-plugins-base \\"
        info "      gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \\"
        info "      libva2 vainfo adb wayland-utils"
        info "  Check GStreamer is 1.22+; older stable releases lack the va plugin."
        if [ "$COMPOSITOR" = "Kwin" ]; then
            info "  # KDE Plasma additionally needs:"
            info "  sudo apt install plasma-wayland-protocols pipewire kservice"
        fi
        ;;
    *suse*)
        info ""
        info "Install (UNVERIFIED — package names are best-effort):"
        info "  sudo zypper install rust cargo gstreamer-plugins-base \\"
        info "      gstreamer-plugins-good gstreamer-plugins-bad libva2 \\"
        info "      libva-utils android-tools wayland-utils"
        if [ "$COMPOSITOR" = "Kwin" ]; then
            info "  # KDE Plasma additionally needs:"
            info "  sudo zypper install plasma-wayland-protocols pipewire kservice"
        fi
        ;;
    *)
        info ""
        info "Unrecognised distribution; install the equivalents of the above."
        ;;
esac

# ---------------------------------------------------------------- verdict ---
head_ "Verdict"

if [ ${#BLOCKERS[@]} -eq 0 ]; then
    printf '  \033[1;32mREADY\033[0m — every requirement is met.\n\n'
    exit 0
fi

printf '  \033[1;31mBLOCKED\033[0m — %d issue(s):\n\n' "${#BLOCKERS[@]}"
for b in "${BLOCKERS[@]}"; do printf '    • %s\n' "$b"; done
printf '\n  See docs/COMPATIBILITY.md.\n\n'
exit 1
