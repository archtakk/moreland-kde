// SPDX-License-Identifier: Apache-2.0

//! One streaming session: virtual output -> capture -> encode -> USB -> tablet.

use anyhow::{bail, Context, Result};
use capture::session::{BufferMode, Capture, CaptureConfig};
use capture::source::FrameSource;
use encoder::{Encoder, EncoderConfig};
use std::collections::VecDeque;
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use transport::{adb, stream_header, Sender};

use crate::input::{InputMsg, TouchInput};
use crate::output::VirtualOutput;

const APP_PACKAGE: &str = "com.moreland.display";
const APP_ACTIVITY: &str = "com.moreland.display/.DisplayActivity";

#[derive(Debug, Clone)]
pub struct Config {
    pub resolution: Option<(u32, u32)>,
    pub max_width: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub position_x: i32,
    pub position_y: i32,
    pub output_name: String,
    /// Host-side port for this session's `adb forward`. The daemon picks a
    /// free port per device (see `main::allocate_port`); the default is the
    /// base of the range, which is correct when only one session runs.
    pub port: u16,
    pub paint_cursor: bool,
    pub stats: bool,
    /// Requested tablet brightness, 0-100. `None` leaves the device alone.
    pub brightness: Option<u8>,
    /// Requested rotation in degrees, one of 0/90/180/270. `90` and `270`
    /// make the virtual output portrait.
    pub rotation: u16,
    /// Whether to create the uinput virtual input device. On by default;
    /// the `--no-touch` flag turns it off for anyone who wants the stream
    /// to carry nothing but acks.
    pub enable_touch: bool,
    /// Which kind of input device to create.
    pub touch_mode: crate::input::TouchMode,
    /// Print the end-of-session statistics as a JSON object instead of a
    /// human-readable block. One object per session, one per line (JSONL),
    /// so a multi-device run produces multiple lines. Set by `--json`.
    pub json_output: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            resolution: None,
            max_width: 1920,
            fps: 90,
            bitrate_kbps: 20_000,
            position_x: 0,
            position_y: 1080,
            output_name: capture::VIRTUAL_OUTPUT_NAME.to_string(),
            port: protocol::DEFAULT_PORT,
            paint_cursor: false,
            stats: false,
            brightness: None,
            rotation: 0,
            enable_touch: true,
            touch_mode: crate::input::TouchMode::Screen,
            json_output: false,
        }
    }
}

pub fn app_installed(serial: &str) -> Result<bool> {
    let out = adb::shell(serial, &format!("pm list packages {APP_PACKAGE}"))?;
    Ok(out.contains(APP_PACKAGE))
}

/// Stream until `shutdown` is set, the device vanishes, or the app disconnects.
pub fn run(serial: &str, config: &Config, shutdown: &AtomicBool) -> Result<()> {
    if !app_installed(serial)? {
        bail!(
            "app not installed on the tablet.\n\
             Build it with `cd android && ANDROID_HOME=/opt/android-sdk ./gradlew assembleRelease`,\n\
             then `adb install -r android/app/build/outputs/apk/release/app-release.apk`."
        );
    }

    // Validate rotation before touching the compositor. A bad value here is a
    // configuration error, not a runtime one.
    let rotation_msg = protocol::ControlMessage::rotation(config.rotation)
        .with_context(|| format!("invalid rotation {} in config", config.rotation))?;

    let (mut width, mut height) = match config.resolution {
        Some(explicit) => explicit,
        None => {
            let panel = adb::display_size(serial)?;
            let scaled = adb::stream_resolution(panel, config.max_width);
            tracing::info!(
                "device panel {}x{} (landscape) -> streaming {}x{}",
                panel.0,
                panel.1,
                scaled.0,
                scaled.1
            );
            scaled
        }
    };

    // Rotation 90/270 puts the tablet in portrait, so the virtual output has
    // to be portrait too. Applying this before `VirtualOutput::create` is what
    // keeps the compositor mode, the encoder geometry, the stream header and
    // the decoder's MediaFormat all agreeing on the same numbers.
    //
    // Do not attempt this mid-session: the capture session, the encoder, and
    // the stream header are all fixed at startup.
    if config.rotation == 90 || config.rotation == 270 {
        std::mem::swap(&mut width, &mut height);
        tracing::info!(
            "rotation {}: virtual output becomes {}x{} (portrait)",
            config.rotation,
            width,
            height
        );
    }

    let mut output = VirtualOutput::create(
        &config.output_name,
        width,
        height,
        config.fps,
        config.position_x,
        config.position_y,
    )?;
    if output.applied_mode() {
        tracing::info!(
            "virtual output {} at {}x{}@{}",
            output.name(),
            width,
            height,
            config.fps
        );
    } else {
        tracing::info!(
            "using existing output {} with its own mode",
            output.name()
        );
    }

    let forward = adb::Forward::new(
        serial,
        config.port,
        &format!("localabstract:{}", protocol::SOCKET_NAME),
    )?;
    let port = forward.local_port()?;

    let _ = adb::shell(serial, &format!("am force-stop {APP_PACKAGE}"));
    std::thread::sleep(Duration::from_millis(500));

    let _ = adb::shell(serial, "input keyevent KEYCODE_WAKEUP");
    let _ = adb::shell(serial, "wm dismiss-keyguard");
    std::thread::sleep(Duration::from_millis(400));

    adb::shell(serial, &format!("am start -n {APP_ACTIVITY}"))
        .context("launching the display app")?;
    std::thread::sleep(Duration::from_millis(2500));

    let allowed_modifiers = encoder::supported_modifiers(capture::XR24);
    tracing::debug!("encoder accepts modifiers {allowed_modifiers:02x?}");

    let mut source: Box<dyn FrameSource> = match &mut output {
        #[cfg(feature = "plasma")]
        VirtualOutput::Kwin(slot) => {
            let plasma = slot
                .take()
                .context("KWin output already consumed by a previous session")?;
            Box::new(capture::plasma::PlasmaCapture::new(plasma)?)
        }
        VirtualOutput::Owned { name, .. } => Box::new(Capture::new(
            name,
            &CaptureConfig {
                mode: BufferMode::Dmabuf,
                pool_size: 3,
                allowed_modifiers,
                paint_cursor: config.paint_cursor,
            },
        )?),
    };

    // The PipeWire source negotiates its format asynchronously — KWin's
    // `param_changed` arrives on another thread after the consumer is
    // constructed, so `source.width()` is 0 until it has fired. Blocking on
    // the first frame is the synchronisation point. The ext- path already
    // knows its geometry here and returns immediately.
    source.wait_for_format()?;

    let encoder = Arc::new(Encoder::new(&EncoderConfig {
        width: source.width(),
        height: source.height(),
        framerate: config.fps,
        bitrate_kbps: config.bitrate_kbps,
        fourcc: source.format(),
        modifier: source.modifier(),
        ..Default::default()
    })?);

    // The virtual touchscreen, and the thread that owns it.
    //
    // The uinput device is created here so a `/dev/uinput` failure is
    // reported at session start rather than on the first touch, but *all*
    // writes happen on a dedicated worker thread. `Touchscreen::send` calls
    // into the kernel, and when the compositor's main thread is busy the
    // kernel's uinput buffer fills and the write blocks. Doing that write
    // from the reverse-channel reader would stall ack processing — the
    // reader is the only thing reading acks, and the round-trip measurement
    // depends on it returning to `read` promptly.
    //
    // The channel is unbounded and a `send` on it never blocks; the reader
    // thread posts and moves on. The worker drains in FIFO order, which is
    // what a gesture needs.
    let input_tx: Option<std::sync::mpsc::Sender<InputMsg>> = if !config.enable_touch {
        tracing::info!("touch: disabled by --no-touch");
        None
    } else {
        // Name the device after its virtual output so the compositor's
        // input configuration can distinguish two sessions at a glance.
        let device_name = format!("{}-touch", config.output_name);
        let device = match config.touch_mode {
            crate::input::TouchMode::Screen => TouchInput::new_screen(
                &device_name,
                source.width(),
                source.height(),
            ),
            crate::input::TouchMode::Pointer => TouchInput::new_pointer(
                &device_name,
                &config.output_name,
                config.position_x,
                config.position_y,
                source.width(),
                source.height(),
            ),
        };

        match device {
            Ok(input) => {
                match config.touch_mode {
                    crate::input::TouchMode::Screen => tracing::info!(
                        "touch: /dev/uinput touchscreen {device_name:?} created for {}x{} output",
                        source.width(),
                        source.height()
                    ),
                    crate::input::TouchMode::Pointer => tracing::info!(
                        "touch: /dev/uinput absolute pointer {device_name:?} created"
                    ),
                }

                // The KWin association entry is only meaningful for the
                // touchscreen mode. The pointer mode positions the cursor
                // itself and does not need it.
                if config.touch_mode == crate::input::TouchMode::Screen
                    && crate::output::Compositor::detect() == crate::output::Compositor::Kwin
                {
                    if let Err(e) = crate::input::kwin_associate_touchscreen(
                        &device_name,
                        &config.output_name,
                    ) {
                        tracing::warn!("KWin touchscreen association failed: {e:#}");
                    }
                }

                let (tx, rx) = std::sync::mpsc::channel::<InputMsg>();
                let handle = std::thread::Builder::new()
                    .name("moreland-touch".into())
                    .spawn(move || crate::input::run_touch_worker(input, rx))
                    .context("spawning the touch worker thread")?;
                // Deliberately detached: the thread exits when `tx` drops
                // at the end of `run`, and joining would make teardown
                // wait for a uinput write.
                std::mem::drop(handle);
                Some(tx)
            }
            Err(e) => {
                tracing::warn!(
                    "touch is unavailable: {e:#}.\n\
                     To enable touch, run `./install.sh` or see docs/TOUCH.md."
                );
                None
            }
        }
    };

    // Wrap the sender so the drain thread can drive it. On an idle KWin
    // output, `source.next_frame()` blocks for seconds at a time — the
    // compositor only repaints on damage. If sending lived in the same loop,
    // packets produced by the encoder in the meantime would sit in a channel
    // and never reach the tablet, because the loop would still be blocked on
    // the *previous* `next_frame` call. Decoupling the two is what makes the
    // idle case work at all.
    let sender = Arc::new(Mutex::new(
        Sender::connect(
            port,
            &stream_header(source.width(), source.height(), config.fps),
        )
        .context("connecting to the app — is it in the foreground on the tablet?")?,
    ));
    tracing::info!("streaming to {serial}");

    // Configure the device before it sees any video. The app applies these on
    // arrival; sending them ahead of the first frame means the first thing
    // the user sees is already at the requested brightness and orientation.
    {
        let mut s = sender.lock().unwrap();
        if let Some(percent) = config.brightness {
            s.send_control(&protocol::ControlMessage::brightness(percent))
                .context("sending brightness control")?;
        }
        s.send_control(&rotation_msg)
            .context("sending rotation control")?;
    }

    let running = Arc::new(AtomicBool::new(true));
    let sent_at: Arc<Mutex<VecDeque<Instant>>> = Arc::new(Mutex::new(VecDeque::new()));
    let round_trips: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));

    // The reverse channel: 9-byte messages, one type byte and eight payload
    // bytes. Both acks and touches arrive here; which is which is the first
    // byte. The reader keeps the same partial-read/timeout discipline as
    // before - the framing change is a state-machine exit condition
    // (`filled == 9`) and a dispatch step, not a rewrite.
    //
    // Touch is emitted *from this thread*. The capture loop on the main
    // thread blocks for seconds at a time on an idle KWin output, so routing
    // touches through it would add that delay to every tap. The uinput write
    // is a single small `write()` to a character device - no Wayland pump, no
    // allocation, no contention on the mutex because nothing else holds it
    // after session start.
    let ack_thread = {
        let mut reader = sender.lock().unwrap().ack_reader()?;
        let sent_at = Arc::clone(&sent_at);
        let round_trips = Arc::clone(&round_trips);
        let running = Arc::clone(&running);
        let input_tx = input_tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; protocol::REVERSE_MSG_LEN];
            let mut filled = 0usize;
            while running.load(Ordering::Relaxed) {
                match reader.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => {
                        filled += n;
                        if filled == protocol::REVERSE_MSG_LEN {
                            filled = 0;
                            match protocol::decode_reverse_message(&buf) {
                                Ok(protocol::ReverseMessage::Ack(_pts)) => {
                                    // Match against send order, not the pts
                                    // in the message: the device echoes the
                                    // decoder's presentation timestamp, which
                                    // is not the one the host submitted, and
                                    // the transport is strictly ordered in
                                    // both directions, so FIFO matching is
                                    // exact.
                                    if let Some(sent) = sent_at.lock().unwrap().pop_front() {
                                        round_trips.lock().unwrap().push(sent.elapsed());
                                    }
                                }
                                Ok(protocol::ReverseMessage::Touch(msg)) => {
                                    if let Some(tx) = &input_tx {
                                        // Unbounded channel; never blocks.
                                        // A send failure means the worker
                                        // thread exited, which is not a
                                        // session-fatal condition.
                                        let _ = tx.send(InputMsg::Touch(msg));
                                    }
                                    // If `input_tx` is `None`, touch is
                                    // disabled or uinput was unavailable;
                                    // the reason was logged once at session
                                    // start. Dropping silently here avoids
                                    // flooding the journal during a drag.
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "dropping malformed reverse message: {e:#}"
                                    );
                                }
                            }
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
        })
    };

    // Drain encoded packets and send them on the same thread. Both are quick
    // operations that never block on each other, and this is the only place
    // bytes reach the socket. The main loop below only feeds frames to the
    // encoder.
    let drain_thread = {
        let encoder = Arc::clone(&encoder);
        let running = Arc::clone(&running);
        let sender = Arc::clone(&sender);
        let sent_at = Arc::clone(&sent_at);
        std::thread::spawn(move || -> Result<()> {
            while running.load(Ordering::Relaxed) {
                match encoder.pull_packet(Duration::from_millis(100)) {
                    Ok(Some(p)) => {
                        let mut s = sender.lock().unwrap();
                        sent_at.lock().unwrap().push_back(Instant::now());
                        s.send_frame(&p.data, p.pts_ns, p.keyframe)?;
                    }
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::error!("encoder: {e}");
                        break;
                    }
                }
            }
            Ok(())
        })
    };

    let frame_duration_ns = 1_000_000_000u64 / u64::from(config.fps);
    let mut index = 0u64;
    let result = (|| -> Result<()> {
        while !shutdown.load(Ordering::Relaxed) {
            let frame = source.next_frame()?;
            encoder.push_frame(
                frame.fd.as_fd(),
                frame.offset,
                frame.stride,
                index * frame_duration_ns,
            )?;
            index += 1;
        }
        Ok(())
    })();

    running.store(false, Ordering::Relaxed);
    let _ = drain_thread.join();
    let _ = ack_thread.join();

    if config.stats {
        let bytes = sender.lock().unwrap().bytes_sent();
        report(&round_trips.lock().unwrap(), index, bytes, config.json_output);
    }

    let _ = adb::shell(serial, &format!("am force-stop {APP_PACKAGE}"));

    drop(forward);
    drop(output);
    result
}

fn report(trips: &[Duration], frames: u64, bytes: u64, json: bool) {
    let mut sorted = trips.to_vec();
    sorted.sort_unstable();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;

    if json {
        let round_trip = if sorted.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({
                "min":    ms(sorted[0]),
                "median": ms(sorted[sorted.len() / 2]),
                "p95":    ms(sorted[sorted.len() * 95 / 100]),
                "max":    ms(sorted[sorted.len() - 1]),
            })
        };
        let obj = serde_json::json!({
            "frames_captured": frames,
            "bytes_sent":      bytes,
            "acks_received":   sorted.len(),
            "round_trip_ms":   round_trip,
        });
        println!("{obj}");
        return;
    }

    println!("\n  frames captured   {frames}");
    println!("  bytes sent        {:.1} MB", bytes as f64 / 1e6);
    println!("  acks received     {}", trips.len());
    if sorted.is_empty() {
        println!("\n  no acknowledgements — check `adb logcat -s Moreland`");
        return;
    }
    println!("\n  round trip: host send -> device render -> host ack");
    println!("    min       {:>8.2} ms", ms(sorted[0]));
    println!("    median    {:>8.2} ms", ms(sorted[sorted.len() / 2]));
    println!("    p95       {:>8.2} ms", ms(sorted[sorted.len() * 95 / 100]));
    println!("    max       {:>8.2} ms", ms(sorted[sorted.len() - 1]));
}
