// SPDX-License-Identifier: Apache-2.0

//! Virtual touchscreen via `/dev/uinput`.
//!
//! A uinput touchscreen appears as `/dev/input/eventN`; every compositor
//! reads input through libinput, so the device works on Hyprland, KWin,
//! GNOME, Sway and labwc. No compositor protocol is involved.
//!
//! Associating the touchscreen with the virtual output is compositor
//! policy, not something the uinput device can specify. On KWin this is
//! resolved by writing an entry to `~/.config/kcminputrc` and asking KWin
//! to reload — see [`kwin_associate_touchscreen`]. On other compositors
//! the user configures it once in the compositor's own settings.

use anyhow::{bail, Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    PropType, UinputAbsSetup,
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

/// How far the finger may drift from the initial touch before a gesture is
/// reclassified from "tap" to "drag". 1% of the absolute range is ~19 px
/// on a 1920-wide output.
const DRAG_THRESHOLD: u32 = 65535 / 100;

/// How long a touch must be held without moving before it becomes a
/// right-click. 600 ms matches Android's own long-press timing.
const LONG_PRESS: Duration = Duration::from_millis(600);

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

/// Desktop bounding box, in global pixel coordinates.
///
/// An absolute pointer's `ABS_X`/`ABS_Y` cover the whole desktop, not a
/// single output. To place the cursor at a specific position on the virtual
/// output, the host needs to know where that output sits inside the
/// desktop — hence the bounds.
#[derive(Debug, Clone, Copy)]
struct DesktopBounds {
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
}

impl DesktopBounds {
    fn width(&self) -> i32 {
        (self.max_x - self.min_x).max(1)
    }
    fn height(&self) -> i32 {
        (self.max_y - self.min_y).max(1)
    }
}

/// Parse `Geometry: X,Y WxH` from a single `kscreen-doctor -o` line.
fn parse_geometry(line: &str) -> Option<(i32, i32, i32, i32)> {
    if line.contains(" disabled ") {
        return None;
    }
    let pos = line.find("Geometry:")?;
    let rest = line[pos + "Geometry:".len()..].trim();
    let mut fields = rest.split_whitespace();
    let pos_field = fields.next()?;
    let size_field = fields.next()?;
    let (x_str, y_str) = pos_field.split_once(',')?;
    let (w_str, h_str) = size_field.split_once('x')?;
    Some((
        x_str.parse().ok()?,
        y_str.parse().ok()?,
        w_str.parse().ok()?,
        h_str.parse().ok()?,
    ))
}

/// Read the desktop layout from `kscreen-doctor -o`.
///
/// Returns the desktop bounding box, the virtual output's origin, and the
/// virtual output's size. KWin names virtual outputs `Virtual-{name}` where
/// `{name}` is what was passed to `stream_virtual_output`.
fn query_layout(output_name: &str) -> Result<(DesktopBounds, (i32, i32), (u32, u32))> {
    let out = Command::new("kscreen-doctor")
        .arg("-o")
        .output()
        .context("running kscreen-doctor -o (part of libkscreen; needed on KDE)")?;
    if !out.status.success() {
        bail!(
            "kscreen-doctor -o failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);

    let wanted = format!("Virtual-{output_name}");
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    let mut output_rect: Option<(i32, i32, i32, i32)> = None;

    for line in text.lines() {
        let Some((x, y, w, h)) = parse_geometry(line) else { continue };
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w);
        max_y = max_y.max(y + h);
        if line.contains(&wanted) {
            output_rect = Some((x, y, w, h));
        }
    }
    if min_x == i32::MAX {
        bail!("kscreen-doctor -o reported no enabled outputs");
    }
    let (x, y, w, h) = output_rect.with_context(|| {
        format!("kscreen-doctor -o has no output named {wanted:?}; is the virtual output up?")
    })?;

    Ok((
        DesktopBounds { min_x, min_y, max_x, max_y },
        (x, y),
        (w as u32, h as u32),
    ))
}

/// State machine for one touch gesture in pointer mode.
#[derive(Debug, Clone, Copy)]
enum PointerState {
    Idle,
    Pressed { down_at: Instant, down_x: u16, down_y: u16 },
    Dragging,
    /// Long-press already fired a right-click. Further movement until
    /// release is ignored.
    Consumed,
}

/// An absolute pointer that drives the mouse cursor on the virtual output.
///
/// No output association is needed: the absolute range covers the whole
/// desktop, and the host computes the correct position from the output's
/// own placement. This is what makes the mode immune to the KWin
/// association bug that plagues `TouchMode::Screen`.
pub struct PointerInput {
    device: VirtualDevice,
    desktop: DesktopBounds,
    output_origin: (i32, i32),
    output_size: (u32, u32),
    state: PointerState,
    /// Last cursor position in the device's `0..32767` range, so a release
    /// lands at the last known position if the caller passes `(0, 0)`.
    last_pos: (i32, i32),
}

impl PointerInput {
    pub fn new(
        name: &str,
        output_name: &str,
        fallback_x: i32,
        fallback_y: i32,
        fallback_w: u32,
        fallback_h: u32,
    ) -> Result<Self> {
        let (desktop, origin, size) = query_layout(output_name).unwrap_or_else(|e| {
            tracing::warn!(
                "pointer: cannot read the layout from kscreen-doctor ({e:#}); \
                 using the requested position instead"
            );
            (
                DesktopBounds {
                    min_x: fallback_x,
                    min_y: fallback_y,
                    max_x: fallback_x + fallback_w as i32,
                    max_y: fallback_y + fallback_h as i32,
                },
                (fallback_x, fallback_y),
                (fallback_w, fallback_h),
            )
        });

        Ok(Self {
            device: build_pointer(name)?,
            desktop,
            output_origin: origin,
            output_size: size,
            state: PointerState::Idle,
            last_pos: (0, 0),
        })
    }

    /// How long until a held touch becomes a long-press. `None` when no
    /// touch is held.
    pub fn time_until_long_press(&self) -> Option<Duration> {
        match self.state {
            PointerState::Pressed { down_at, .. } => {
                Some(LONG_PRESS.checked_sub(down_at.elapsed()).unwrap_or(Duration::ZERO))
            }
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
        let PointerState::Pressed { down_x, down_y, .. } = self.state else {
            return Ok(());
        };
        self.move_to(down_x, down_y)?;
        self.device
            .emit(&[key(KeyCode::BTN_RIGHT, 1), syn()])
            .context("pointer: right button down (long-press)")?;
        self.device
            .emit(&[key(KeyCode::BTN_RIGHT, 0), syn()])
            .context("pointer: right button up (long-press)")?;
        self.state = PointerState::Consumed;
        Ok(())
    }

    /// Release any held button and return to idle. Called on teardown.
    pub fn cancel(&mut self) -> Result<()> {
        if let PointerState::Dragging = self.state {
            self.device
                .emit(&[key(KeyCode::BTN_LEFT, 0), syn()])
                .context("pointer: left button up (cancel)")?;
        }
        self.state = PointerState::Idle;
        Ok(())
    }

    fn on_down(&mut self, x: u16, y: u16) -> Result<()> {
        // Move the cursor to the touched position immediately. No button
        // yet: pressing before we know whether this is a tap or a drag
        // would produce a spurious click on every drag.
        self.move_to(x, y)?;
        self.state = PointerState::Pressed {
            down_at: Instant::now(),
            down_x: x,
            down_y: y,
        };
        Ok(())
    }

    fn on_move(&mut self, x: u16, y: u16) -> Result<()> {
        match self.state {
            PointerState::Idle | PointerState::Consumed => Ok(()),
            PointerState::Pressed { down_x, down_y, .. } => {
                let dx = (x as i32 - down_x as i32).unsigned_abs();
                let dy = (y as i32 - down_y as i32).unsigned_abs();
                if dx > DRAG_THRESHOLD || dy > DRAG_THRESHOLD {
                    // Escalate to a drag. The button press lands at the
                    // *original* touch position — that is where the user
                    // meant to grab — so the cursor returns there before
                    // the press.
                    self.move_to(down_x, down_y)?;
                    self.device
                        .emit(&[key(KeyCode::BTN_LEFT, 1), syn()])
                        .context("pointer: left button down (drag)")?;
                    self.move_to(x, y)?;
                    self.state = PointerState::Dragging;
                }
                Ok(())
            }
            PointerState::Dragging => self.move_to(x, y),
        }
    }

    fn on_up(&mut self, x: u16, y: u16) -> Result<()> {
        let prev = std::mem::replace(&mut self.state, PointerState::Idle);
        match prev {
            PointerState::Pressed { .. } => {
                self.move_to(x, y)?;
                self.device
                    .emit(&[key(KeyCode::BTN_LEFT, 1), syn()])
                    .context("pointer: left button down (tap)")?;
                self.device
                    .emit(&[key(KeyCode::BTN_LEFT, 0), syn()])
                    .context("pointer: left button up (tap)")?;
                Ok(())
            }
            PointerState::Dragging => {
                self.move_to(x, y)?;
                self.device
                    .emit(&[key(KeyCode::BTN_LEFT, 0), syn()])
                    .context("pointer: left button up (drag end)")?;
                Ok(())
            }
            PointerState::Consumed | PointerState::Idle => Ok(()),
        }
    }

    fn move_to(&mut self, x: u16, y: u16) -> Result<()> {
        let (abs_x, abs_y) = self.to_device_coords(x, y);
        self.last_pos = (abs_x, abs_y);
        self.device
            .emit(&[
                abs(AbsoluteAxisCode::ABS_X, abs_x),
                abs(AbsoluteAxisCode::ABS_Y, abs_y),
                syn(),
            ])
            .context("pointer: cursor move")
    }

    fn to_device_coords(&self, x_norm: u16, y_norm: u16) -> (i32, i32) {
        let (ox, oy) = self.output_origin;
        let (ow, oh) = self.output_size;
        let local_x = ox as i64 + (x_norm as i64 * ow as i64) / 65535;
        let local_y = oy as i64 + (y_norm as i64 * oh as i64) / 65535;
        let dw = self.desktop.width() as i64;
        let dh = self.desktop.height() as i64;
        let abs_x = ((local_x - self.desktop.min_x as i64) * ABS_RANGE as i64) / dw;
        let abs_y = ((local_y - self.desktop.min_y as i64) * ABS_RANGE as i64) / dh;
        (abs_x as i32, abs_y as i32)
    }
}

fn build_pointer(name: &str) -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    keys.insert(KeyCode::BTN_LEFT);
    keys.insert(KeyCode::BTN_RIGHT);
    keys.insert(KeyCode::BTN_MIDDLE);

    // `INPUT_PROP_POINTER`, *not* `DIRECT`. This tells libinput the device
    // drives a cursor rather than a touch surface. Without it, KWin would
    // apply output association to the absolute axes — exactly the failure
    // this mode exists to sidestep.
    let mut props = AttributeSet::<PropType>::new();
    props.insert(PropType::POINTER);

    let mut b = VirtualDevice::builder()
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
        .context("declaring INPUT_PROP_POINTER")?;

    for axis in [AbsoluteAxisCode::ABS_X, AbsoluteAxisCode::ABS_Y] {
        let setup = UinputAbsSetup::new(axis, AbsInfo::new(0, 0, ABS_RANGE, 0, 0, 0));
        b = b
            .with_absolute_axis(&setup)
            .with_context(|| format!("declaring {axis:?}"))?;
    }
    b.build().context("creating the uinput absolute pointer")
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

    pub fn new_pointer(
        name: &str,
        output_name: &str,
        fallback_x: i32,
        fallback_y: i32,
        fallback_w: u32,
        fallback_h: u32,
    ) -> Result<Self> {
        Ok(Self::Pointer(PointerInput::new(
            name, output_name, fallback_x, fallback_y, fallback_w, fallback_h,
        )?))
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
