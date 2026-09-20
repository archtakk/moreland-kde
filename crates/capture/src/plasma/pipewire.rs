// SPDX-License-Identifier: Apache-2.0

//! Consume a KWin virtual-output PipeWire node as DMA-BUF frames.
//!
//! The node id comes from `PlasmaVirtualOutput::node_id`. We connect with
//! `target.object` set to that id, request DMA-BUF buffers, and hand the fds
//! to the encoder. No pixels are copied on the host.
//!
//! **Ownership model.** `pipewire-rs` 0.10 splits every type into a `Box`
//! variant that borrows its parent and an `Rc` variant that owns it. The
//! Box family cannot coexist in a single struct — `StreamBox<'c>` carries
//! `PhantomData<&'c Core>` and nothing in stable Rust expresses the
//! self-reference needed to hold the core alongside it. The Rc family can,
//! because each layer owns its parent by refcount: `StreamRc` owns a
//! `CoreRc`, which owns a `ContextRc`. That is why everything below is the
//! `Rc` spelling.

use anyhow::{bail, Context, Result};
use pipewire as pw;
use pw::spa;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use super::PlasmaVirtualOutput;
use crate::source::{Frame, FrameSource};

/// Negotiated stream format. Populated by `param_changed` before the first
/// frame arrives; the encoder is configured from this, not from what we asked
/// for, because KWin is free to ignore the size we suggested.
#[derive(Clone, Copy, Debug, Default)]
pub struct NegotiatedFormat {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub stride: u32,
    pub offset: u32,
}

pub struct PlasmaCapture {
    // Field declaration order is drop order once the explicit `Drop` below
    // returns. The listener must drop while the stream is still alive — its
    // own Drop unregisters the callbacks — so it is declared first. The
    // stream in turn owns the core, which owns the context, so the chain
    // unwinds in the right order.
    _stream_listener: pw::stream::StreamListener<()>,
    _stream: pw::stream::StreamRc,
    // Not strictly needed for ownership — the stream already keeps these
    // alive through its chain — but holding them makes the lifetimes
    // legible and gives the `Drop` impl something to stop.
    _context: pw::context::ContextRc,
    _thread_loop: pw::thread_loop::ThreadLoopRc,

    output: PlasmaVirtualOutput,

    frames: mpsc::Receiver<Frame>,
    format: Arc<Mutex<NegotiatedFormat>>,
    error: Arc<Mutex<Option<String>>>,
}

impl PlasmaCapture {
    pub fn new(output: PlasmaVirtualOutput) -> Result<Self> {
        pw::init();

        // `ThreadLoopRc` rather than `MainLoopRc`: the ThreadLoop variant
        // owns a dedicated pump thread, which is what we want — the main
        // loop is `!Send` and cannot be moved into a `std::thread::spawn`.
        //
        // SAFETY: the loop is created and consumed on the same thread, and
        // its `Drop` runs after every other field thanks to the explicit
        // `Drop` below stopping it first. The raw pointer it wraps never
        // escapes this struct.
        let thread_loop = unsafe {
            pw::thread_loop::ThreadLoopRc::new(Some("moreland-pipewire"), None)
        }
        .context("creating PipeWire thread loop")?;

        let context = pw::context::ContextRc::new(&thread_loop, None)
            .context("creating PipeWire context")?;

        let core = context
            .connect_rc(None)
            .context("connecting to the PipeWire daemon")?;

        let (tx, rx) = mpsc::channel();
        let format = Arc::new(Mutex::new(NegotiatedFormat::default()));
        let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // `target.object` pins us to the specific node KWin just created. A
        // bare autoconnect would latch onto the first screen-capture node it
        // found — on a busy desktop, someone else's window.
        // `target.object` must be the object serial, not the PipeWire node
        // id. They are different namespaces: the node id is the local object
        // id PipeWire assigned the node this session, while the object
        // serial is the stable identifier the session manager's linking
        // policy actually matches on. Handing it the node id produces "no
        // target node available" and the stream dies before any frame
        // arrives, with no error at the Wayland layer.
        //
        // KWin sends the serial in a `serial` event (protocol v6 and up).
        // Older compositors do not, in which case fall back to the node id —
        // which may still work if the node happens to carry it as
        // `object.serial`, but is not guaranteed.
        let target = output
            .object_serial()
            .or_else(|| output.node_id().map(u64::from))
            .context("KWin reported neither a serial nor a node for the virtual output")?
            .to_string();

        let props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            "target.object" => target,
        };

        // `StreamRc::new` takes the core *by value* and stores it, which is
        // what keeps the entire ownership chain alive without us holding
        // each link as a separate field.
        let stream = pw::stream::StreamRc::new(core, "moreland-capture", props.into())
            .context("creating PipeWire stream")?;

        // The process callback runs on PipeWire's real-time thread. It must
        // not block, allocate, or lock anything a slower thread holds. The
        // mutex on `format` is only ever held for a memcpy of a POD struct,
        // and the channel is unbounded, so neither can stall it.
        let f = Arc::clone(&format);
        let err_process = Arc::clone(&error);
        let tx_for_process = tx.clone();
        let fmt_for_param = Arc::clone(&format);
        let err_state = Arc::clone(&error);

        let stream_listener = stream
            .add_local_listener_with_user_data(())
            .process(move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data) = datas.get_mut(0) else { return };

                // `Data::fd()` on `libspa` 0.10 returns `RawFd`; a buffer
                // that is not fd-backed reports -1, and a DMA-BUF always
                // carries one.
                let raw_fd = data.fd();
                if raw_fd < 0 {
                    *err_process.lock().unwrap() = Some(
                        "PipeWire buffer carried no fd (not DMA-BUF?)".into(),
                    );
                    return;
                }

                let fmt = *f.lock().unwrap();
                if fmt.width == 0 {
                    // Format negotiation has not completed yet.
                    return;
                }

                // The raw fd is owned by PipeWire and reclaimed the moment
                // this callback returns; the encoder holds the frame for
                // milliseconds. Dup it.
                //
                // SAFETY: the fd is valid for the duration of the callback,
                // and we are duplicating it into our own owned descriptor
                // before the callback returns.
                let borrowed = unsafe {
                    std::os::fd::BorrowedFd::borrow_raw(raw_fd)
                };
                let owned = match borrowed.try_clone_to_owned() {
                    Ok(fd) => fd,
                    Err(e) => {
                        *err_process.lock().unwrap() = Some(format!("dup failed: {e}"));
                        return;
                    }
                };

                let _ = tx_for_process.send(Frame {
                    fd: owned,
                    offset: fmt.offset,
                    stride: fmt.stride,
                    // PipeWire pts is stream-relative and shares no clock
                    // with the host's; the transport layer assigns its own.
                    pts_ns: None,
                });
            })
            .param_changed(move |_stream, _data, id, param| {
                let Some(param) = param else { return };
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Ok((media_type, media_subtype)) =
                    spa::param::format_utils::parse_format(param)
                else {
                    return;
                };
                if media_type != spa::param::format::MediaType::Video
                    || media_subtype != spa::param::format::MediaSubtype::Raw
                {
                    return;
                }

                let mut info = spa::param::video::VideoInfoRaw::default();
                if info.parse(param).is_err() {
                    return;
                }

                let mut slot = fmt_for_param.lock().unwrap();
                slot.width = info.size().width;
                slot.height = info.size().height;
                slot.modifier = info.modifier();

                // HACK: `libspa` 0.10 has no accessor for the DRM fourcc
                // when the format is DMA_DRM — `VideoInfoRaw::format()`
                // returns the marker `SPA_VIDEO_FORMAT_DMA_DRM` (0x0c), and
                // the real fourcc lives in a sibling pod structure the
                // wrapper does not parse. KWin only offers XR24 for its
                // shared virtual output, so pinning it here is a stopgap
                // until the correct accessor is identified. Remove once
                // `drm_fourcc_from_spa` returns a real value.
                const SPA_VIDEO_FORMAT_DMA_DRM: u32 = 0x0c;
                slot.fourcc = if info.format().as_raw() == SPA_VIDEO_FORMAT_DMA_DRM {
                    0x34325258 // "XR24"
                } else {
                    drm_fourcc_from_spa(&info)
                };
                // Stride and offset arrive per-buffer, not in the format;
                // for single-plane XR24/AR24 they are stable across frames.
                if slot.stride == 0 {
                    slot.stride = slot.width * 4;
                    slot.offset = 0;
                }
            })
            .state_changed(move |_stream, _data, _old, new| {
                if let pw::stream::StreamState::Error(msg) = new {
                    *err_state.lock().unwrap() = Some(msg.to_string());
                }
            })
            .register()
            .context("registering PipeWire stream listeners")?;

        // MAP_BUFFERS is what makes PipeWire hand us the DMA-BUF fd rather
        // than a memfd copy. Without it we would silently be back on the CPU
        // path the whole design exists to avoid.
        //
        // `params` is the negotiation list — we leave it empty and let the
        // stream's `param_changed` callback read what KWin chooses.
        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut [],
            )
            .context("connecting PipeWire stream to the KWin node")?;

        // `start` spins the loop's thread up and returns; it is infallible
        // in this release.
        thread_loop.start();

        Ok(Self {
            _stream_listener: stream_listener,
            _stream: stream,
            _context: context,
            _thread_loop: thread_loop,
            output,
            frames: rx,
            format,
            error,
        })
    }

    /// PipeWire node id, if the compositor sent one. See
    /// [`PlasmaVirtualOutput::node_id`] — since protocol v6 the stable
    /// identifier is `object_serial`, not this.
    pub fn node_id(&self) -> Option<u32> {
        self.output.node_id()
    }
}

impl Drop for PlasmaCapture {
    fn drop(&mut self) {
        // Stop the pump thread first, so no process callback can fire while
        // the fields below are unwinding. This is the ordering the comment
        // on the struct declaration is referring to.
        self._thread_loop.stop();
    }
}

impl FrameSource for PlasmaCapture {
    fn next_frame(&mut self) -> Result<Frame> {
        // Short timeout, checked against the error cell between attempts, so
        // a stream that died reports *why* rather than just going quiet.
        loop {
            match self.frames.recv_timeout(Duration::from_millis(500)) {
                Ok(frame) => return Ok(frame),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(e) = self.error.lock().unwrap().take() {
                        bail!("PipeWire stream failed: {e}");
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("PipeWire stream ended");
                }
            }
        }
    }

    fn width(&self) -> u32 {
        self.format.lock().unwrap().width
    }

    fn height(&self) -> u32 {
        self.format.lock().unwrap().height
    }

    fn format(&self) -> u32 {
        self.format.lock().unwrap().fourcc
    }

    fn modifier(&self) -> u64 {
        self.format.lock().unwrap().modifier
    }

    /// Drain one frame to force `param_changed` to run, then stop.
    ///
    /// `PlasmaCapture::new` connects the stream and starts the pump thread,
    /// but the format only becomes known when KWin's `param_changed` callback
    /// fires — which is asynchronous and may lag the constructor by tens of
    /// milliseconds. Until then the accessors above read zeros. Pulling a
    /// frame is the reliable synchronisation point: the process callback
    /// cannot deliver one without `param_changed` having run first.
    fn wait_for_format(&mut self) -> Result<()> {
        if self.width() != 0 {
            return Ok(());
        }
        // Blocks until the first frame arrives (or the stream fails). The
        // frame itself is discarded — it carries the right dimensions for
        // its own construction, but the consumer builds the pipeline from
        // the negotiated format, not from a specific buffer.
        let _ = self.next_frame()?;
        Ok(())
    }
}

/// Map an SPA video format to a DRM fourcc.
///
/// For `VideoFormat::DmaDrm` the raw SPA format value *is* the DRM fourcc,
/// packed little-endian. If a future release changes that encoding, this is
/// the one place to adjust.
fn drm_fourcc_from_spa(info: &spa::param::video::VideoInfoRaw) -> u32 {
    info.format().as_raw()
}
