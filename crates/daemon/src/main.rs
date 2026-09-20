// SPDX-License-Identifier: Apache-2.0

//! Moreland daemon.
//!
//!   moreland                 watch for tablets; stream to every one plugged in
//!   moreland --once          stream one session per ready tablet, then exit
//!   moreland --seconds N     stop after N seconds (implies --stats)
//!
//! Plug a tablet in and a virtual monitor appears; unplug it and that monitor
//! disappears. Several tablets may be connected at once; each gets its own
//! virtual output, its own encoder, and its own host-side forward port.
//!
//! The outer loop is a reconciler: it polls the device tracker, compares the
//! ready set against the registry of running sessions, and starts or stops
//! sessions to close the gap. Everything per-session — capture, encoder,
//! forward, virtual output — lives on that session's thread, and teardown is
//! RAII on the way out of `session::run`. Main only owns the shutdown flag
//! and the join handle.

mod input;
mod output;
mod session;
mod tracker;

use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracker::{Device, DeviceTracker};

/// How long a device must be continuously reported as `device` before the
/// daemon will start a session for it. The tracker is edge-driven — a list is
/// pushed whenever it changes — so a flapping cable or a briefly racing `adb`
/// daemon can present a device that is gone by the time the session opens it.
/// Requiring the device to survive one full poll interval filters that out.
/// Slightly longer than the 500 ms read timeout, so the state is observed
/// across at least two iterations.
const SPAWN_DEBOUNCE: Duration = Duration::from_millis(600);

struct Args {
    config: session::Config,
    /// True when the user passed --output-name explicitly. In that case the
    /// name is used verbatim for every session and a collision with an active
    /// session is refused rather than silently rewritten. See
    /// `resolve_output_name` for the default scheme.
    explicit_output_name: bool,
    once: bool,
    seconds: Option<u64>,
    /// Per-serial brightness overrides from `--brightness SERIAL=N`. A
    /// device absent from this map falls back to `config.brightness`.
    brightness_overrides: HashMap<String, u8>,
    /// Per-serial rotation overrides from `--rotation SERIAL=DEG`. A
    /// device absent from this map falls back to `config.rotation`.
    rotation_overrides: HashMap<String, u16>,
}

fn parse_args() -> Args {
    let mut config = session::Config::default();
    let mut explicit_output_name = false;
    let mut once = false;
    let mut seconds = None;
    let (mut width, mut height) = (None, None);
    let mut brightness_overrides: HashMap<String, u8> = HashMap::new();
    let mut rotation_overrides: HashMap<String, u16> = HashMap::new();

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut next_u32 = |default: u32| {
            it.next().and_then(|v| v.parse().ok()).unwrap_or(default)
        };
        match arg.as_str() {
            "--width" => width = Some(next_u32(1920)),
            "--height" => height = Some(next_u32(1200)),
            "--max-width" => config.max_width = next_u32(1920),
            "--native" => config.max_width = u32::MAX,
            "--fps" => config.fps = next_u32(60),
            "--bitrate" => config.bitrate_kbps = next_u32(20_000),
            "--position" => {
                config.position_x = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(config.position_x);
            }
            "--position-y" => {
                config.position_y = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(config.position_y);
            }
            "--output-name" => {
                if let Some(name) = it.next() {
                    config.output_name = name;
                    explicit_output_name = true;
                }
            }
            // Two forms: `--brightness N` sets the default for every device
            // without its own override, and `--brightness SERIAL=N` sets it
            // for one. Repeatable, so a single invocation can give every
            // tablet a different value. Malformed brightness falls back to
            // "leave it alone" (matching --fps, --bitrate); values above 100
            // clamp rather than reject, since "brighter than 100" is a wish
            // and not a typo.
            "-b" | "--brightness" => {
                let raw = it.next().unwrap_or_default();
                let (serial, value) = match raw.split_once('=') {
                    Some((s, v)) if !s.is_empty() => (Some(s.to_ascii_lowercase()), v),
                    _ => (None, raw.as_str()),
                };
                match value.parse::<u32>() {
                    Ok(percent) => {
                        // Parse as u32 and clamp, not u8: `--brightness 500`
                        // should behave like `--brightness 100`, not silently
                        // do nothing because 500 overflows u8 while 200 clamps.
                        let percent = percent.min(100) as u8;
                        match serial {
                            Some(s) => { brightness_overrides.insert(s, percent); }
                            None => { config.brightness = Some(percent); }
                        }
                    }
                    Err(_) => {
                        // Was silent before. A malformed value on a flag the
                        // user just typed is worth a line on stderr — doing
                        // nothing with no explanation is the failure mode a
                        // typo produces.
                        eprintln!(
                            "moreland: ignoring --brightness {raw:?} (expected 0-100 or SERIAL=0-100)"
                        );
                    }
                }
            }
            // Rotation is worth a hard error: the four values are enumerable,
            // and a typo like `--rotation 45` silently producing landscape is
            // exactly the kind of failure the user would not notice until
            // they pick the tablet up. Exit non-zero rather than guess.
            // Two forms, same shape as --brightness: `--rotation DEG` sets
            // the default, `--rotation SERIAL=DEG` sets it for one device.
            // Still a hard error on a bad value: the four states are
            // enumerable and a typo silently producing landscape is exactly
            // the failure the user would not notice until they pick the
            // tablet up.
            "--rotation" => {
                let raw = it.next().unwrap_or_default();
                let (serial, value) = match raw.split_once('=') {
                    Some((s, v)) if !s.is_empty() => (Some(s.to_ascii_lowercase()), v),
                    _ => (None, raw.as_str()),
                };
                let degrees = match value.parse::<u16>() {
                    Ok(0) => 0u16,
                    Ok(90) => 90,
                    Ok(180) => 180,
                    Ok(270) => 270,
                    _ => {
                        // Print the whole argument, not just the value half:
                        // `--rotation RF8N50QRDNV=45` should show the serial
                        // too, so a user scanning a script can see which
                        // device the bad value was attached to.
                        eprintln!(
                            "moreland: --rotation must be 0, 90, 180, or 270 (got {raw:?})"
                        );
                        std::process::exit(2);
                    }
                };
                match serial {
                    Some(s) => { rotation_overrides.insert(s, degrees); }
                    None => { config.rotation = degrees; }
                }
            }
            "--show-cursor" => config.paint_cursor = true,
            // Opt out of the uinput virtual touchscreen. The stream still
            // carries acks; touches on the tablet are ignored by the host.
            "--no-touch" => config.enable_touch = false,
            // Emit end-of-session statistics as JSON instead of the
            // human-readable block. One object per session, one per line.
            "--json" => config.json_output = true,
            // How the uinput device is declared. `screen` is a real
            // touchscreen the compositor associates with an output;
            // `pointer` moves the mouse cursor to the touched position and
            // clicks. Pointer mode needs no output association.
            "--touch-mode" => {
                let raw = it.next().unwrap_or_default();
                config.touch_mode = match raw.as_str() {
                    "screen" | "touchscreen" => crate::input::TouchMode::Screen,
                    "pointer" | "mouse" => crate::input::TouchMode::Pointer,
                    _ => {
                        eprintln!(
                            "moreland: --touch-mode must be screen or pointer (got {raw:?})"
                        );
                        std::process::exit(2);
                    }
                };
            }
            "--once" => once = true,
            "--stats" => config.stats = true,
            // --seconds no longer implies --once. The timer sets the global
            // shutdown flag and all sessions tear down in parallel; --once is
            // now purely "one session per ready device, then exit".
            "--seconds" => {
                seconds = it.next().and_then(|v| v.parse().ok());
                config.stats = true;
            }
            "--help" | "-h" => {
                println!("{}", include_str!("usage.txt"));
                std::process::exit(0);
            }
            _ => {}
        }
    }
    // Only an explicit pair pins the resolution; one alone still auto-detects,
    // since a half-specified size would silently distort the aspect ratio.
    config.resolution = match (width, height) {
        (Some(w), Some(h)) => Some((w, h)),
        (Some(_), None) | (None, Some(_)) => {
            eprintln!("--width and --height must be given together; auto-detecting instead");
            None
        }
        (None, None) => None,
    };

    Args {
        config,
        explicit_output_name,
        once,
        seconds,
        brightness_overrides,
        rotation_overrides,
    }
}

/// One running streaming session.
///
/// The session thread owns its virtual output and its adb forward via RAII,
/// so teardown is "set the flag, join the thread". Main keeps the flag, the
/// handle, and the port and output name for logging and for the allocators to
/// consult.
struct Session {
    shutdown: Arc<AtomicBool>,
    handle: JoinHandle<Result<()>>,
    port: u16,
    output_name: String,
}

/// Per-device backoff after a failed session. Replaces the single global
/// counter of the pre-multi-device daemon: one tablet failing repeatedly must
/// not delay another tablet's first attempt.
struct Cooldown {
    failures: u32,
    next_attempt: Instant,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        // Logs go to stderr so stdout is reserved for the report. With
        // `--json` that makes the output a clean JSONL stream that jq,
        // scripts/bench.sh, and any other consumer can parse directly.
        .with_writer(std::io::stderr)
        .init();

    let args = parse_args();

    // Refuse to start rather than discovering this once per device event. The
    // compositor cannot change under a running daemon — the unit is
    // PartOf=graphical-session.target — so an unsupported one is fatal, not
    // transient, and the retry backoff would otherwise spin on it for as long
    // as the session lasts.
    let compositor = output::Compositor::detect();
    compositor.ensure_supported()?;
    tracing::info!("compositor: {}", compositor.name());

    let shutdown = Arc::new(AtomicBool::new(false));

    // SIGTERM matters here: systemd uses it to stop the service, and every
    // virtual output must come down cleanly or the compositor is left with a
    // phantom monitor per session.
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&shutdown))?;
    }

    if let Some(seconds) = args.seconds {
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(seconds));
            shutdown.store(true, Ordering::Relaxed);
        });
    }

    let mut tracker = DeviceTracker::connect()?;
    tracing::info!("watching for devices");

    // The last device list we were told about. This must persist across
    // iterations: `next_update` only returns when the list *changes*, so after
    // a session ends for any reason other than the device vanishing, no
    // further update is ever sent, and blocking on one would strand the
    // daemon.
    let mut devices: Vec<Device> = Vec::new();
    let mut registry: HashMap<String, Session> = HashMap::new();
    let mut cooldowns: HashMap<String, Cooldown> = HashMap::new();
    // Serial -> the Instant it was first seen continuously ready. Cleared when
    // the device stops being ready; consulted for SPAWN_DEBOUNCE.
    let mut seen_ready_at: HashMap<String, Instant> = HashMap::new();
    let mut announced_idle = false;
    // --once only spawns for devices that were already ready when it armed.
    // Later arrivals are ignored by design.
    let mut once_armed = false;

    while !shutdown.load(Ordering::Relaxed) {
        reap_finished(&mut registry, &mut cooldowns);

        // --once exits as soon as every session it armed has been reaped.
        if args.once && once_armed && registry.is_empty() {
            break;
        }

        // Short timeout: absence of an update is normal and just means the
        // list still stands.
        //
        // A failure here means the tracker's socket is in a state where
        // setsockopt failed — an fd the process no longer owns, or some
        // unusual kernel condition. That is not fatal and does not mean the
        // daemon should exit; it is a signal that the tracker should be
        // replaced, which the next `next_update` will discover if it also
        // fails. Swallow the error, log it, and let the read path drive
        // reconnection.
        if let Err(e) = tracker.set_timeout(Some(Duration::from_millis(500))) {
            tracing::warn!("tracker set_timeout failed: {e}; next read will retry");
        }
        match tracker.next_update() {
            Ok(update) => {
                devices = update;
                announced_idle = false;
            }
            Err(e) if is_timeout(&e) => {}
            Err(e) => {
                tracing::warn!("device tracker lost: {e}; reconnecting");
                // Do not clear `devices` here. Losing the tracker is not the
                // same as losing every device, and `currently_ready` is
                // computed from this list — clearing it would mark every
                // running session as vanished on the next iteration and tear
                // them all down. The last known list stands until the
                // reconnected tracker sends a fresh one; sessions whose
                // device is genuinely gone will fail on their own and be
                // reaped with a cooldown.
                std::thread::sleep(Duration::from_secs(2));
                match DeviceTracker::connect() {
                    Ok(t) => {
                        tracker = t;
                        tracing::info!("device tracker reconnected");
                    }
                    Err(e) => tracing::warn!("reconnect failed: {e}"),
                }
                continue;
            }
        }

        let now = Instant::now();
        let currently_ready: HashSet<&str> = devices
            .iter()
            .filter(|d| d.is_ready())
            .map(|d| d.serial.as_str())
            .collect();

        // Track how long each serial has been continuously ready.
        seen_ready_at.retain(|serial, _| currently_ready.contains(serial.as_str()));
        for serial in &currently_ready {
            seen_ready_at.entry((*serial).to_string()).or_insert(now);
        }

        // Stop sessions whose device is no longer ready. The session thread
        // notices the flag and exits through its normal RAII path; reaping
        // happens on a later iteration. Stopping only this session is the
        // whole point of per-session flags.
        let vanished: Vec<String> = registry
            .keys()
            .filter(|s| !currently_ready.contains(s.as_str()))
            .cloned()
            .collect();
        for serial in vanished {
            if let Some(session) = registry.get(&serial) {
                tracing::info!("device {serial} disconnected; stopping session");
                session.shutdown.store(true, Ordering::Relaxed);
            }
        }

        // Decide who to spawn for this iteration.
        let candidates: Vec<Device> = if args.once {
            if once_armed {
                Vec::new()
            } else {
                // Arm as soon as any ready device has survived the debounce,
                // then spawn for everything ready at that instant — including
                // devices that appeared this iteration and have *not* yet
                // debounced. The asymmetry is deliberate: `--once` is a
                // one-shot, so requiring every ready device to debounce
                // first would delay the run for everyone until the last
                // arrival settles, and the cost of getting it wrong is
                // bounded to one failed session per flickering device,
                // reaped with a cooldown. Devices that arrive after arming
                // are ignored by design.
                let ready: Vec<Device> =
                    devices.iter().filter(|d| d.is_ready()).cloned().collect();
                let any_debounced = ready.iter().any(|d| {
                    seen_ready_at
                        .get(&d.serial)
                        .map(|t| now.duration_since(*t) >= SPAWN_DEBOUNCE)
                        .unwrap_or(false)
                });
                if any_debounced {
                    once_armed = true;
                    ready
                } else {
                    Vec::new()
                }
            }
        } else {
            devices
                .iter()
                .filter(|d| d.is_ready())
                .filter(|d| !registry.contains_key(&d.serial))
                .filter(|d| {
                    seen_ready_at
                        .get(&d.serial)
                        .map(|t| now.duration_since(*t) >= SPAWN_DEBOUNCE)
                        .unwrap_or(false)
                })
                .filter(|d| {
                    cooldowns
                        .get(&d.serial)
                        .map(|c| now >= c.next_attempt)
                        .unwrap_or(true)
                })
                .cloned()
                .collect()
        };

        for device in candidates {
            if registry.contains_key(&device.serial) {
                continue;
            }
            match spawn_session(&device, &args, &registry) {
                Ok(session) => {
                    tracing::info!(
                        "device {} connected: port {}, output {:?}",
                        device.serial,
                        session.port,
                        session.output_name
                    );
                    registry.insert(device.serial.clone(), session);
                    announced_idle = false;
                }
                Err(e) => {
                    // Allocation or thread-spawn failure. Back this serial
                    // off rather than retrying every 500 ms.
                    tracing::warn!("cannot start session for {}: {e:#}", device.serial);
                    bump_cooldown(&mut cooldowns, &device.serial);
                }
            }
        }

        if registry.is_empty() && !announced_idle {
            if devices.is_empty() {
                tracing::info!("no device connected");
            } else {
                for device in &devices {
                    if !device.is_ready() {
                        tracing::info!("device {} is {} — waiting", device.serial, device.state);
                    }
                }
            }
            announced_idle = true;
        }
    }

    tracing::info!("shutting down");
    // Signal every session, then join them in parallel. A serial join would
    // make teardown take N x one-session time; on KWin an idle virtual output
    // blocks `next_frame` for seconds at a time, and the systemd unit's
    // TimeoutStopSec has to cover the worst idle frame. See the unit.
    for session in registry.values() {
        session.shutdown.store(true, Ordering::Relaxed);
    }
    join_all_parallel(&mut registry);
    Ok(())
}

/// Choose a fresh port and output name for `device`, spawn `session::run` on
/// its own thread, and return the handle plus the values it will use.
///
/// Both allocations are made against the *current* registry, so the registry
/// and the allocators can never disagree: a session is running iff its name
/// and port are spoken for.
fn spawn_session(
    device: &Device,
    args: &Args,
    registry: &HashMap<String, Session>,
) -> Result<Session> {
    let port = allocate_port(registry, args.config.port)?;
    let output_name = resolve_output_name(
        args.explicit_output_name,
        &args.config.output_name,
        &device.serial,
        registry,
    )?;

    let mut config = args.config.clone();
    config.port = port;
    config.output_name = output_name.clone();

    // Per-serial overrides win over the global defaults. A device that has
    // neither falls through to whatever the bare flags set (which is `None`
    // for brightness — "leave the device alone" — and 0 for rotation).
    //
    // The serial is lowercased on both sides: `adb devices -l` returns
    // uppercase hex on most physical devices, lowercase on emulators, and
    // mixed on some vendor-chosen ones. A user typing
    // `--brightness RF8N50QRDNV=20` should not silently miss a device whose
    // serial adb happens to report as `rf8n50qrdnv`.
    let serial_key = device.serial.to_ascii_lowercase();
    if let Some(&percent) = args.brightness_overrides.get(&serial_key) {
        config.brightness = Some(percent);
    }
    if let Some(&degrees) = args.rotation_overrides.get(&serial_key) {
        config.rotation = degrees;
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_thread = Arc::clone(&shutdown);
    let serial = device.serial.clone();
    let handle = std::thread::Builder::new()
        .name(format!("moreland-{serial}"))
        .spawn(move || session::run(&serial, &config, &shutdown_thread))
        .context("spawning session thread")?;

    Ok(Session {
        shutdown,
        handle,
        port,
        output_name,
    })
}

/// Lowest free port in `base..base + protocol::PORT_RANGE`.
///
/// The whole range is claimed by this daemon — see the note on
/// `protocol::BASE_PORT` and on `adb::Forward::new` — so a foreign forward
/// left anywhere in it will be silently wiped. Two daemons on one host will
/// race for the same base; run one daemon per host.
fn allocate_port(registry: &HashMap<String, Session>, base: u16) -> Result<u16> {
    for offset in 0..protocol::PORT_RANGE {
        let candidate = base
            .checked_add(offset)
            .context("host-side port range overflowed")?;
        if !registry.values().any(|s| s.port == candidate) {
            return Ok(candidate);
        }
    }
    bail!(
        "no free host-side port in {}..{}; too many concurrent sessions",
        base,
        base.saturating_add(protocol::PORT_RANGE - 1)
    );
}

/// Decide the virtual output name for a new session.
///
/// Two rules:
///
/// 1. If the user passed --output-name, it is used verbatim. That is a CLI
///    contract and is not rewritten — but it also means two sessions cannot
///    share that one name, so a collision is refused with a message naming
///    both serials. The first session keeps running; only the new one is
///    rejected. Silently reusing the name would be worse than failing: on
///    Hyprland `VirtualOutput::create` sees the existing monitor and reuses
///    it, so the second tablet would capture the first tablet's output.
///
/// 2. Otherwise, if nothing holds the base name, use it — the single-tablet
///    case yields `moreland`. If it is taken, append the last four
///    alphanumerics of the serial, lowercased. If that also collides (very
///    rare between two devices from one manufacturer), append the full
///    serial. If even that collides, refuse.
///
/// The name is stable for the session's lifetime: a later tablet does not
/// rename an earlier session, so the running output never churns.
fn resolve_output_name(
    explicit: bool,
    base: &str,
    serial: &str,
    registry: &HashMap<String, Session>,
) -> Result<String> {
    let taken: HashSet<&str> = registry
        .values()
        .map(|s| s.output_name.as_str())
        .collect();

    if explicit {
        if taken.contains(base) {
            let other = registry
                .iter()
                .find(|(_, s)| s.output_name == base)
                .map(|(serial, _)| serial.as_str())
                .unwrap_or("<unknown>");
            bail!(
                "output name {base:?} is already held by the session for {other}.\n\
                 Pass --output-name unique per daemon, or stop that session first.\n\
                 (An explicit --output-name is never rewritten: the daemon will not \
                 silently suffix a name the user chose.)"
            );
        }
        return Ok(base.to_string());
    }

    if !taken.contains(base) {
        return Ok(base.to_string());
    }

    let candidate = format!("{base}-{}", serial_suffix(serial, 4));
    if !taken.contains(candidate.as_str()) {
        return Ok(candidate);
    }

    let candidate = format!("{base}-{serial}");
    if !taken.contains(candidate.as_str()) {
        return Ok(candidate);
    }

    bail!(
        "virtual output name {base:?} is already held, and neither the last four \
         characters nor the full serial of {serial} disambiguates it.\n\
         Stop the other session, or set a distinct name with --output-name."
    );
}

/// Last `n` alphanumerics of `serial`, lowercased. Serials are ASCII hex in
/// practice, so this is normally just the last n characters; the filter is
/// there because ADB permits `:` in emulator serials and `_` in some
/// vendor-chosen ones, neither of which belong in a monitor name.
fn serial_suffix(serial: &str, n: usize) -> String {
    let cleaned: String = serial
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let start = cleaned.len().saturating_sub(n);
    cleaned[start..].to_ascii_lowercase()
}

/// Reap any session threads that have finished, without blocking.
///
/// `JoinHandle::join` returns `Result<Result<()>, Box<dyn Any>>`: the outer
/// error is a panic in the session thread. A panic there — historically from
/// `ElementBuilder::property` on `nvh264enc` — must not take down the daemon
/// and every other session with it, so it is logged and converted into a
/// per-device cooldown like any other failure.
fn reap_finished(
    registry: &mut HashMap<String, Session>,
    cooldowns: &mut HashMap<String, Cooldown>,
) {
    let finished: Vec<String> = registry
        .iter()
        .filter(|(_, s)| s.handle.is_finished())
        .map(|(serial, _)| serial.clone())
        .collect();

    for serial in finished {
        let Some(session) = registry.remove(&serial) else {
            continue;
        };
        match session.handle.join() {
            Ok(Ok(())) => {
                tracing::info!("session {serial} ended cleanly");
                cooldowns.remove(&serial);
            }
            Ok(Err(e)) => {
                tracing::warn!("session {serial} ended: {e:#}");
                bump_cooldown(cooldowns, &serial);
            }
            Err(panic) => {
                tracing::error!("session {serial} panicked: {}", panic_message(&panic));
                bump_cooldown(cooldowns, &serial);
            }
        }
    }
}

/// Join every remaining session thread, in parallel.
///
/// `std::thread::scope` is used rather than spawning detached joiners so the
/// scope cannot outlive the borrow of the drained registry.
fn join_all_parallel(registry: &mut HashMap<String, Session>) {
    let sessions: Vec<(String, Session)> = registry.drain().collect();
    std::thread::scope(|scope| {
        let mut joins = Vec::with_capacity(sessions.len());
        for (serial, session) in sessions {
            joins.push(scope.spawn(move || (serial, session.handle.join())));
        }
        for join in joins {
            let (serial, result) = join.join().expect("join helper thread panicked");
            match result {
                Ok(Ok(())) => tracing::info!("session {serial} ended cleanly"),
                Ok(Err(e)) => tracing::warn!("session {serial} ended: {e:#}"),
                Err(panic) => {
                    tracing::error!("session {serial} panicked: {}", panic_message(&panic))
                }
            }
        }
    });
}

fn bump_cooldown(cooldowns: &mut HashMap<String, Cooldown>, serial: &str) {
    let now = Instant::now();
    let entry = cooldowns.entry(serial.to_string()).or_insert(Cooldown {
        failures: 0,
        next_attempt: now,
    });
    entry.failures += 1;
    let backoff = Duration::from_secs(2 * u64::from(entry.failures.min(8)).max(1));
    entry.next_attempt = now + backoff;
    if entry.failures > 2 {
        tracing::info!("device {serial}: retrying in {}s", backoff.as_secs());
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

fn is_timeout(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        })
    })
}
