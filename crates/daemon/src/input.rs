// SPDX-License-Identifier: Apache-2.0

//! Virtual input devices via `/dev/uinput`.
//!
//! Two device types are supported, matching [`TouchMode`]:
//!
//! - **Touchscreen.** A real uinput touchscreen (`INPUT_PROP_DIRECT`,
//!   multitouch protocol B) that the compositor associates with an output.
//!   Tap, drag and long-press all land where the finger is.
//!
//! - **Pointer.** A relative pointer that behaves like a laptop touchpad.
//!   Finger motion drives the cursor *relatively* (the cursor does not jump
//!   to the point of contact), tap is a left-click, a long-press without
//!   moving is a right-click, and double-tap-then-drag is a click-drag. No
//!   output association is involved anywhere in the path, so this mode works
//!   on compositors that would refuse to associate an absolute pointer with
//!   anything but their primary output.
//!
//! Every compositor reads input through libinput, so both devices work on
//! Hyprland, KWin, GNOME, Sway and labwc. No compositor protocol is
//! involved.
//!
//! Associating the *touchscreen* with the virtual output is compositor
//! policy, not something the uinput device can specify. On KWin this is
//! resolved by writing an entry to `~/.config/kcminputrc` and asking KWin
//! to reload — see [`kwin_associate_touchscreen`]. On other compositors the
//! user configures it once in the compositor's own settings. The pointer
//! device needs no association at all.

use anyhow::{bail, Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    PropType, RelativeAxisCode, UinputAbsSetup,
};
use protocol::TouchAction;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const ABS_RANGE: i32 = 32767;
const NOMINAL_PPI: f64 = 160.0;
const VENDOR_ID: u16 = 0x4d52;
const PRODUCT_ID: u16 = 0x4c44;
/// Pointer mode declares a different product ID so KConfig and libinput
/// see it as a distinct device from the touchscreen mode.
const PRODUCT_ID_POINTER: u16 = 0x4c45;

/// How far (in normalized units, per axis, from the DOWN position) the
/// finger may drift and still count as a tap. 1500 / 65535 ≈ 2.3% of the
/// tablet surface, which is roughly the slop a physical touchpad allows
/// before it treats a touch as motion rather than a click.
const TAP_MAX_MOVE: u32 = 1500;

/// How long a touch must be held *without moving* before it becomes a
/// right-click. 600 ms matches Android's own long-press timing. Any motion
/// beyond [`TAP_MAX_MOVE`] cancels the timer.
const LONG_PRESS: Duration = Duration::from_millis(600);

/// How long after a tap a second touch is treated as the beginning of a
/// double-tap drag. 300 ms is the usual double-click window.
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(300);

/// Default cursor sensitivity for `TouchMode::Pointer`: cursor pixels per
/// unit of normalized finger motion.
///
/// A full swipe across the tablet (65535 units) moves the cursor about 3900
/// px, which is roughly two screens on a 1920-wide desktop. That matches the
/// effective sensitivity of a laptop touchpad, where 5 cm of finger travel
/// covers most of the screen. Override with `--pointer-sensitivity`.
pub const DEFAULT_POINTER_SENSITIVITY: f32 = 0.06;

/// Which kind of virtual input device to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchMode {
    /// Real touchscreen. `INPUT_PROP_DIRECT`; the compositor associates
    /// it with an output. Default.
    Screen,
    /// Absolute pointer. Touches move the cursor and click.
    Pointer,
}

/// Message from the reverse-channel reader to the input worker.
///
/// The reader thread must never block on anything but `read`; a uinput
/// write can block briefly when the compositor is busy. The writes happen
/// on the worker, and messages are handed to it over an unbounded channel.
#[derive(Debug)]
pub enum InputMsg {
    Touch(protocol::TouchMessage),
}

pub struct Touchscreen {
    device: VirtualDevice,
    contact_open: bool,
    next_tracking_id: i32,
    last_pos: (i32, i32),
}

impl Touchscreen {
    pub fn new(name: &str, width: u32, height: u32) -> Result<Self> {
        let device = build(name, width, height)?;
        Ok(Self {
            device,
            contact_open: false,
            next_tracking_id: 1,
            last_pos: (0, 0),
        })
    }

    pub fn send(&mut self, action: TouchAction, x: u16, y: u16) -> Result<()> {
        let x = scale_to_range(x);
        let y = scale_to_range(y);
        match action {
            TouchAction::Down => {
                if self.contact_open { self.emit_release()?; }
                self.contact_open = true;
                self.last_pos = (x, y);
                let id = self.alloc_tracking_id();
                self.device.emit(&[
                    abs(AbsoluteAxisCode::ABS_MT_SLOT, 0),
                    abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, id),
                    abs(AbsoluteAxisCode::ABS_MT_POSITION_X, x),
                    abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, y),
                    abs(AbsoluteAxisCode::ABS_X, x),
                    abs(AbsoluteAxisCode::ABS_Y, y),
                    key(KeyCode::BTN_TOUCH, 1),
                    syn(),
                ]).context("emitting touch DOWN")?;
            }
            TouchAction::Move => {
                if !self.contact_open { return Ok(()); }
                self.last_pos = (x, y);
                self.device.emit(&[
                    abs(AbsoluteAxisCode::ABS_MT_POSITION_X, x),
                    abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, y),
                    abs(AbsoluteAxisCode::ABS_X, x),
                    abs(AbsoluteAxisCode::ABS_Y, y),
                    syn(),
                ]).context("emitting touch MOVE")?;
            }
            TouchAction::Up | TouchAction::Cancel => {
                if self.contact_open {
                    if (x, y) != (0, 0) { self.last_pos = (x, y); }
                    self.emit_release()?;
                }
            }
        }
        Ok(())
    }

    fn emit_release(&mut self) -> Result<()> {
        let (x, y) = self.last_pos;
        self.contact_open = false;
        self.device.emit(&[
            abs(AbsoluteAxisCode::ABS_MT_SLOT, 0),
            abs(AbsoluteAxisCode::ABS_MT_POSITION_X, x),
            abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, y),
            abs(AbsoluteAxisCode::ABS_X, x),
            abs(AbsoluteAxisCode::ABS_Y, y),
            abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1),
            key(KeyCode::BTN_TOUCH, 0),
            syn(),
        ]).context("emitting touch release")?;
        Ok(())
    }

    fn alloc_tracking_id(&mut self) -> i32 {
        let id = self.next_tracking_id;
        self.next_tracking_id =
            if self.next_tracking_id >= 65535 { 1 } else { self.next_tracking_id + 1 };
        id
    }
}

fn build(name: &str, width: u32, height: u32) -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    keys.insert(KeyCode::BTN_TOUCH);
    let mut props = AttributeSet::<PropType>::new();
    props.insert(PropType::DIRECT);
    let res_x = ppi_resolution(width);
    let res_y = ppi_resolution(height);
    let mut b = VirtualDevice::builder()
        .context("opening /dev/uinput")?
        .name(name)
        .input_id(InputId::new(BusType::BUS_VIRTUAL, VENDOR_ID, PRODUCT_ID, 1))
        .with_keys(&keys).context("declaring BTN_TOUCH")?
        .with_properties(&props).context("declaring INPUT_PROP_DIRECT")?;
    for (axis, max, resolution) in [
        (AbsoluteAxisCode::ABS_X, ABS_RANGE, res_x),
        (AbsoluteAxisCode::ABS_Y, ABS_RANGE, res_y),
        (AbsoluteAxisCode::ABS_MT_POSITION_X, ABS_RANGE, res_x),
        (AbsoluteAxisCode::ABS_MT_POSITION_Y, ABS_RANGE, res_y),
        (AbsoluteAxisCode::ABS_MT_SLOT, 9, 0),
        (AbsoluteAxisCode::ABS_MT_TRACKING_ID, 65535, 0),
    ] {
        let setup = UinputAbsSetup::new(axis, AbsInfo::new(0, 0, max, 0, 0, resolution));
        b = b.with_absolute_axis(&setup).with_context(|| format!("declaring {axis:?}"))?;
    }
    b.build().context("creating the uinput touchscreen")
}

fn scale_to_range(v: u16) -> i32 { ((v as u32 * ABS_RANGE as u32) / 65535) as i32 }

fn ppi_resolution(pixels: u32) -> i32 {
    let physical_mm = (pixels as f64) * 25.4 / NOMINAL_PPI;
    if physical_mm <= 0.0 { return 0; }
    (ABS_RANGE as f64 / physical_mm) as i32
}

fn abs(axis: AbsoluteAxisCode, value: i32) -> InputEvent {
    InputEvent::new(EventType::ABSOLUTE.0, axis.0, value)
}
fn key(code: KeyCode, value: i32) -> InputEvent {
    InputEvent::new(EventType::KEY.0, code.0, value)
}
fn syn() -> InputEvent {
    InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0)
}

/// Associate a touchscreen device with a KWin virtual output.
///
/// KWin has no Wayland protocol for this. The association lives in
/// `~/.config/kcminputrc` under a section shaped like:
///
/// ```ini
/// [Libinput][19794][19524][moreland-touch]
/// Output=Virtual-moreland
/// ```
///
/// The numeric fields are the decimal forms of the vendor and product IDs
/// declared on the uinput device; the name is the device's `name` property.
/// KWin reads the file at startup and on `org.kde.KWin.reconfigure`.
///
/// This function writes the section if it is not already present, then
/// asks KWin to reload. An existing section is left alone — if the user
/// has set an output manually, the daemon must not override it.
pub fn kwin_associate_touchscreen(device_name: &str, output_name: &str) -> Result<()> {
    let path = kcminputrc_path()?;
    let section_no_brackets = format!("Libinput][{}][{}][{}", VENDOR_ID, PRODUCT_ID, device_name);
    let section = format!("[{section_no_brackets}]");
    let desired = format!("output=Virtual-{}", output_name);

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    // The *section header* is what we key the "already present" check on,
    // not the whole desired block. If a previous build wrote the entry with
    // a different key case (`Output=` vs KWin's `output=`) or a stale
    // output name, the section is present but wrong; the block below
    // replaces it rather than skipping. Match on the header so we detect
    // "already has an entry for this device" and then verify the *value*
    // inside.
    let has_section = existing.contains(&section);
    let has_correct_value = existing
        .lines()
        .skip_while(|l| !l.trim_end().starts_with(&format!("[{section_no_brackets}]")))
        .skip(1)
        .take_while(|l| !l.trim().starts_with('['))
        .any(|l| l.trim() == desired);
    if has_section && has_correct_value {
        tracing::debug!("KWin: {device_name} already associated with {output_name}");
    } else {
        // Rebuild the file: preserve every section except this device's,
        // then append a fresh one. This is the only way to fix an existing
        // wrong-case or wrong-value entry, since appending would leave the
        // broken one in place and KConfig reads the first match.
        let mut new_content = String::new();
        let mut in_target_section = false;
        for line in existing.lines() {
            let trimmed = line.trim_end();
            if trimmed.starts_with('[') {
                in_target_section = trimmed == section;
            }
            if !in_target_section {
                new_content.push_str(line);
                new_content.push('\n');
            }
        }
        if !new_content.is_empty() && !new_content.ends_with('\n') {
            new_content.push('\n');
        }
        new_content.push_str(&format!("\n{section}\n{desired}\n"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, new_content)
            .with_context(|| format!("writing {}", path.display()))?;
        tracing::info!("KWin: wrote touchscreen association to {}", path.display());
    }

    trigger_kwin_reconfigure()
}

fn kcminputrc_path() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("kcminputrc"));
        }
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/kcminputrc"))
}

fn trigger_kwin_reconfigure() -> Result<()> {
    let attempts: &[(&str, &[&str])] = &[
        ("qdbus6", &["org.kde.KWin", "/KWin", "reconfigure"]),
        ("qdbus", &["org.kde.KWin", "/KWin", "reconfigure"]),
        ("dbus-send", &[
            "--session", "--dest=org.kde.KWin",
            "/KWin", "org.kde.KWin.reconfigure",
        ]),
    ];
    for (cmd, args) in attempts {
        if let Ok(o) = Command::new(cmd).args(*args).output() {
            if o.status.success() { return Ok(()); }
        }
    }
    bail!(
        "no working D-Bus sender; tried qdbus6, qdbus, dbus-send. \
         The touchscreen association will take effect on the next KWin start."
    )
}

// --------------------------------------------------------------- pointer ---

/// State machine for one touch gesture in pointer mode.
///
/// The tablet behaves like a touchpad: finger motion drives the cursor
/// *relatively*, and the cursor does not jump to the point of contact. That
/// is the single biggest difference from a real touchscreen, and it is what
/// makes this mode work on compositors that associate an absolute pointer
/// with the primary output regardless of what the host computes.
#[derive(Debug, Clone, Copy)]
enum PointerState {
    /// No finger down.
    Idle,
    /// Finger down, tracking motion. Tap vs. cursor-move is decided on
    /// release by comparing the final position against the DOWN position.
    Touched {
        down_at: Instant,
        down_x: u16,
        down_y: u16,
        last_x: u16,
        last_y: u16,
        /// Set when the previous release was a tap within
        /// [`DOUBLE_TAP_WINDOW`]. A move past [`TAP_MAX_MOVE`] from the
        /// current DOWN converts the gesture into a click-drag.
        pending_drag: bool,
        /// Latched once the finger has strayed beyond [`TAP_MAX_MOVE`] from
        /// the DOWN position. Long-press requires that it never did.
        moved: bool,
    },
    /// Left button is held; motion continues until release.
    Dragging { last_x: u16, last_y: u16 },
    /// Long-press already fired a right-click. Ignore movement until
    /// release.
    Consumed,
}

/// A relative pointer that behaves like a laptop touchpad.
///
/// Unlike [`Touchscreen`], no output association is needed and none is
/// performed: the device emits `REL_X`/`REL_Y` events, which the compositor
/// applies to whatever cursor position it already has. The absolute pointer
/// that this replaces — `ABS_X`/`ABS_Y` — needed an output association KWin
/// refuses to grant for a non-touchscreen, and without one KWin clamped
/// every event to the primary output.
pub struct PointerInput {
    device: VirtualDevice,
    state: PointerState,
    /// Time of the last tap release, for double-tap detection. Cleared as
    /// soon as any non-tap gesture or teardown consumes it.
    last_tap_end: Option<Instant>,
    /// Fractional cursor motion accumulated but not yet emitted. Without
    /// this a slow finger drag would round each per-sample delta to zero
    /// and the cursor would never move.
    residual: (f32, f32),
    /// Cursor pixels per unit of normalized finger motion, from
    /// `--pointer-sensitivity`.
    sensitivity: f32,
}

impl PointerInput {
    /// `sensitivity` is cursor pixels per unit of normalized finger motion.
    /// It is validated at parse time in `main::parse_args`, so by the time
    /// it reaches here it is a positive, finite number.
    pub fn new(name: &str, sensitivity: f32) -> Result<Self> {
        Ok(Self {
            device: build_pointer(name)?,
            state: PointerState::Idle,
            last_tap_end: None,
            residual: (0.0, 0.0),
            sensitivity,
        })
    }

    /// Time until a held, *stationary* touch becomes a long-press. `None`
    /// when no touch is held or the finger has already moved far enough
    /// that a long-press would be surprising.
    pub fn time_until_long_press(&self) -> Option<Duration> {
        match self.state {
            PointerState::Touched { down_at, moved: false, .. } => Some(
                LONG_PRESS
                    .checked_sub(down_at.elapsed())
                    .unwrap_or(Duration::ZERO),
            ),
            _ => None,
        }
    }

    pub fn on_touch(&mut self, action: TouchAction, x: u16, y: u16) -> Result<()> {
        match action {
            TouchAction::Down => self.on_down(x, y),
            TouchAction::Move => self.on_move(x, y),
            TouchAction::Up | TouchAction::Cancel => self.on_up(x, y),
        }
    }

    pub fn on_long_press(&mut self) -> Result<()> {
        if !matches!(self.state, PointerState::Touched { .. }) {
            return Ok(());
        }
        // Right-click at the *current* cursor position, not at the finger.
        // A touchpad does the same: the pointer stays where it was and the
        // button event lands there.
        self.device
            .emit(&[key(KeyCode::BTN_RIGHT, 1), syn()])
            .context("pointer: right button down (long-press)")?;
        self.device
            .emit(&[key(KeyCode::BTN_RIGHT, 0), syn()])
            .context("pointer: right button up (long-press)")?;
        self.state = PointerState::Consumed;
        self.last_tap_end = None;
        Ok(())
    }

    /// Release any held button and return to idle. Called on teardown.
    pub fn cancel(&mut self) -> Result<()> {
        if let PointerState::Dragging { .. } = self.state {
            self.device
                .emit(&[key(KeyCode::BTN_LEFT, 0), syn()])
                .context("pointer: left button up (cancel)")?;
        }
        self.state = PointerState::Idle;
        self.last_tap_end = None;
        self.residual = (0.0, 0.0);
        Ok(())
    }

    fn on_down(&mut self, x: u16, y: u16) -> Result<()> {
        // A second tap within the double-click window begins a pending
        // drag: the finger has not moved yet, so we do not press the button
        // until motion actually starts.
        let pending_drag = self
            .last_tap_end
            .map(|t| t.elapsed() < DOUBLE_TAP_WINDOW)
            .unwrap_or(false);
        self.residual = (0.0, 0.0);
        self.state = PointerState::Touched {
            down_at: Instant::now(),
            down_x: x,
            down_y: y,
            last_x: x,
            last_y: y,
            pending_drag,
            moved: false,
        };
        Ok(())
    }

    fn on_move(&mut self, x: u16, y: u16) -> Result<()> {
        match self.state {
            PointerState::Idle | PointerState::Consumed => Ok(()),
            PointerState::Touched {
                down_at,
                down_x,
                down_y,
                last_x,
                last_y,
                pending_drag,
                moved,
            } => {
                let dx_from_down = (x as i32 - down_x as i32).unsigned_abs();
                let dy_from_down = (y as i32 - down_y as i32).unsigned_abs();
                let now_moved =
                    moved || dx_from_down > TAP_MAX_MOVE || dy_from_down > TAP_MAX_MOVE;

                // Commit a pending double-tap into a real drag as soon as
                // the finger has moved far enough to be unambiguous.
                if pending_drag && now_moved {
                    self.device
                        .emit(&[key(KeyCode::BTN_LEFT, 1), syn()])
                        .context("pointer: left button down (double-tap drag)")?;
                    self.state = PointerState::Dragging {
                        last_x: x,
                        last_y: y,
                    };
                    return Ok(());
                }

                let (rdx, rdy) = self.relative_delta((last_x, last_y), (x, y));
                self.emit_relative(rdx, rdy)?;
                self.state = PointerState::Touched {
                    down_at,
                    down_x,
                    down_y,
                    last_x: x,
                    last_y: y,
                    pending_drag,
                    moved: now_moved,
                };
                Ok(())
            }
            PointerState::Dragging { last_x, last_y } => {
                let (rdx, rdy) = self.relative_delta((last_x, last_y), (x, y));
                self.emit_relative(rdx, rdy)?;
                self.state = PointerState::Dragging {
                    last_x: x,
                    last_y: y,
                };
                Ok(())
            }
        }
    }

    fn on_up(&mut self, x: u16, y: u16) -> Result<()> {
        let prev = std::mem::replace(&mut self.state, PointerState::Idle);
        match prev {
            PointerState::Touched { down_x, down_y, moved, .. } => {
                let dx = (x as i32 - down_x as i32).unsigned_abs();
                let dy = (y as i32 - down_y as i32).unsigned_abs();
                if moved || dx > TAP_MAX_MOVE || dy > TAP_MAX_MOVE {
                    // Moved too far to be a tap: this was a cursor move. No
                    // click.
                    self.last_tap_end = None;
                    return Ok(());
                }
                // Quick, stationary touch: left-click.
                self.device
                    .emit(&[key(KeyCode::BTN_LEFT, 1), syn()])
                    .context("pointer: left button down (tap)")?;
                self.device
                    .emit(&[key(KeyCode::BTN_LEFT, 0), syn()])
                    .context("pointer: left button up (tap)")?;
                self.last_tap_end = Some(Instant::now());
                Ok(())
            }
            PointerState::Dragging { .. } => {
                self.device
                    .emit(&[key(KeyCode::BTN_LEFT, 0), syn()])
                    .context("pointer: left button up (drag end)")?;
                self.last_tap_end = None;
                Ok(())
            }
            PointerState::Consumed | PointerState::Idle => {
                self.last_tap_end = None;
                Ok(())
            }
        }
    }

    /// Scale a finger delta from normalized units into cursor pixels,
    /// carrying the sub-pixel remainder forward so slow drags accumulate
    /// instead of rounding to zero forever.
    fn relative_delta(&mut self, last: (u16, u16), now: (u16, u16)) -> (i32, i32) {
        let dx = now.0 as i32 - last.0 as i32;
        let dy = now.1 as i32 - last.1 as i32;
        let fx = dx as f32 * self.sensitivity + self.residual.0;
        let fy = dy as f32 * self.sensitivity + self.residual.1;
        let ix = fx.trunc() as i32;
        let iy = fy.trunc() as i32;
        self.residual.0 = fx - ix as f32;
        self.residual.1 = fy - iy as f32;
        (ix, iy)
    }

    fn emit_relative(&mut self, dx: i32, dy: i32) -> Result<()> {
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        // Both axes in one batch, so the compositor applies them as a
        // single motion event and the cursor does not trace an L-shape.
        let mut events = Vec::with_capacity(3);
        if dx != 0 {
            events.push(rel(RelativeAxisCode::REL_X, dx));
        }
        if dy != 0 {
            events.push(rel(RelativeAxisCode::REL_Y, dy));
        }
        events.push(syn());
        self.device
            .emit(&events)
            .context("pointer: cursor move")
    }
}

fn build_pointer(name: &str) -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    keys.insert(KeyCode::BTN_LEFT);
    keys.insert(KeyCode::BTN_RIGHT);
    keys.insert(KeyCode::BTN_MIDDLE);

    // `INPUT_PROP_POINTER`, *not* `DIRECT`. This tells libinput the device
    // drives a cursor rather than a touch surface. Relative axes plus this
    // property are what a mouse or touchpad declares, and it is why KWin
    // treats the events as ordinary pointer motion with no output
    // association anywhere in the path.
    let mut props = AttributeSet::<PropType>::new();
    props.insert(PropType::POINTER);

    let mut rel_axes = AttributeSet::<RelativeAxisCode>::new();
    rel_axes.insert(RelativeAxisCode::REL_X);
    rel_axes.insert(RelativeAxisCode::REL_Y);

    VirtualDevice::builder()
        .context("opening /dev/uinput")?
        .name(name)
        .input_id(InputId::new(
            BusType::BUS_VIRTUAL,
            VENDOR_ID,
            PRODUCT_ID_POINTER,
            1,
        ))
        .with_keys(&keys)
        .context("declaring mouse buttons")?
        .with_properties(&props)
        .context("declaring INPUT_PROP_POINTER")?
        .with_relative_axes(&rel_axes)
        .context("declaring REL_X/REL_Y")?
        .build()
        .context("creating the uinput relative pointer")
}

fn rel(axis: RelativeAxisCode, value: i32) -> InputEvent {
    InputEvent::new(EventType::RELATIVE.0, axis.0, value)
}

// ----------------------------------------------------------- dispatcher ---

pub enum TouchInput {
    Screen(Touchscreen),
    Pointer(PointerInput),
}

impl TouchInput {
    pub fn new_screen(name: &str, width: u32, height: u32) -> Result<Self> {
        Ok(Self::Screen(Touchscreen::new(name, width, height)?))
    }

    pub fn new_pointer(name: &str, sensitivity: f32) -> Result<Self> {
        Ok(Self::Pointer(PointerInput::new(name, sensitivity)?))
    }

    /// Time until a held touch becomes a long-press, if the mode has that
    /// concept. Screen mode does not: the compositor does its own timing.
    pub fn time_until_long_press(&self) -> Option<Duration> {
        match self {
            Self::Pointer(p) => p.time_until_long_press(),
            Self::Screen(_) => None,
        }
    }

    pub fn send(&mut self, action: TouchAction, x: u16, y: u16) -> Result<()> {
        match self {
            Self::Screen(ts) => ts.send(action, x, y),
            Self::Pointer(p) => p.on_touch(action, x, y),
        }
    }

    pub fn long_press(&mut self) -> Result<()> {
        match self {
            Self::Pointer(p) => p.on_long_press(),
            Self::Screen(_) => Ok(()),
        }
    }

    pub fn cancel(&mut self) -> Result<()> {
        match self {
            Self::Screen(ts) => ts.send(TouchAction::Cancel, 0, 0),
            Self::Pointer(p) => p.cancel(),
        }
    }
}

/// Consume `InputMsg`s and drive a [`TouchInput`].
///
/// The long-press timer lives here rather than in a per-gesture thread:
/// the worker blocks on `recv_timeout` for the remaining time while a
/// press is pending, so a long-press fires exactly when due and no thread
/// is spawned per tap.
pub fn run_touch_worker(mut input: TouchInput, rx: std::sync::mpsc::Receiver<InputMsg>) {
    loop {
        let msg = match input.time_until_long_press() {
            Some(t) => match rx.recv_timeout(t) {
                Ok(m) => Some(m),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(e) = input.long_press() {
                        tracing::warn!("touch long-press failed: {e:#}");
                    }
                    None
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            },
        };
        if let Some(InputMsg::Touch(t)) = msg {
            if let Err(e) = input.send(t.action, t.x, t.y) {
                tracing::warn!("touch emit failed: {e:#}");
            }
        }
    }
    if let Err(e) = input.cancel() {
        tracing::warn!("touch cancel failed: {e:#}");
    }
}
