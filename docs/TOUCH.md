# Touchscreen input

moreland turns the tablet's screen into a real input device for the virtual
output. Touches on the tablet arrive at the host's compositor as standard
libinput events, so tap, drag, long-press, and any gesture the compositor
already implements all work.

There are two modes, chosen with `--touch-mode`:

- **`screen`** (default) — a real uinput touchscreen, declared with
  `INPUT_PROP_DIRECT` and the multitouch protocol B axes. The compositor
  associates it with an output the same way it associates a physical
  touchscreen.
- **`pointer`** — an absolute pointer. Touches move the mouse cursor to the
  corresponding position on the virtual output, a tap is a left click, and a
  long press (~600 ms without moving) is a right click. No output association
  is involved; the host computes the cursor position from the desktop bounding
  box reported by `kscreen-doctor -o`.

`--no-touch` disables input entirely; the tablet becomes a display only.

## Setup

Touch needs write access to `/dev/uinput`. This is a one-time group
membership, not root.

Check first:

```sh
ls -l /dev/uinput
```

If the permissions include a group your user is already in (`id -nG`), nothing
needs doing. Otherwise:

```sh
# /etc/udev/rules.d/60-moreland-uinput.rules
KERNEL=="uinput", SUBSYSTEM=="misc", MODE="0660", GROUP="uinput", OPTIONS+="static_node=uinput"
```

```sh
sudo groupadd -f uinput
sudo usermod -aG uinput "$USER"
sudo udevadm control --reload
sudo udevadm trigger
# log out and back in for the group change to take effect
```

If `/dev/uinput` does not exist, load the module:

```sh
sudo modprobe uinput
```

## Behaviour when /dev/uinput is unavailable

The session still starts and the tablet is a fully functional display. The
daemon logs exactly one warning at session start; touches are dropped silently
afterwards. The wire protocol is unchanged.

## Associating the touchscreen with the virtual output

A uinput touchscreen is a kernel input device. The kernel has no concept of
"this touchscreen belongs to that output"; the compositor decides which output
a `0..32767` coordinate pair maps onto.

### KWin

moreland writes the association to `~/.config/kcminputrc` itself, in the same
format KWin's own System Settings writes, and asks KWin to reload. A manual
choice in System Settings -> Input Devices -> Touchscreen takes precedence.

The daemon writes:

```ini
[Libinput][<vendor>][<product>][<output-name>-touch]
output=Virtual-<output-name>
```

The vendor and product are the decimal forms of `0x4d52` and `0x4c44`, the IDs
declared on the uinput device. The name is `<output-name>-touch`, e.g.
`moreland-touch` for the first tablet and `moreland-rdnv-touch` for the second.

### Hyprland

```ini
# ~/.config/hypr/hyprland.conf
input {
    touchdevice {
        output = moreland
    }
}
```

### Other compositors

Sway: `input "moreland-touch" map_to_output moreland`.

labwc and other wlroots compositors: see the compositor's own input
configuration.

## Known limitations

**`pointer` mode does not work on KWin.** KWin associates an absolute pointer
with the primary output rather than the one under the cursor, so touches land
on the main screen regardless of what the host computes. This is KWin
behaviour, not a moreland bug, and it does not affect Hyprland. Use `screen`
mode on KWin.

*Note that this is a subject to change, as the work is being done on modifing
the `pointer` mode to behave like a touchpad, meaning that it will work on KWin, too.*

**Single contact point only.** Multitouch, stylus, pen, pressure, tilt, and
palm rejection are all out of scope for now.
