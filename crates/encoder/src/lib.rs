// SPDX-License-Identifier: Apache-2.0

//! Hardware H.264 encoding of captured DMA-BUF frames.
//!
//! Two hardware backends, selected at runtime from what GStreamer has
//! registered. Both accept the raw DMA-BUF fd the compositor wrote into, and
//! both produce the same Annex-B stream.
//!
//! ```text
//! VA-API (AMD VCN, Intel QuickSync)
//!   appsrc  video/x-raw(memory:DMABuf) DMA_DRM XR24:<modifier>
//!     -> vapostproc      XR24 -> NV12, on the GPU
//!     -> vah264enc
//!
//! NVENC via GL (NVIDIA)
//!   appsrc  video/x-raw(memory:DMABuf) DMA_DRM XR24:<modifier>
//!     -> glupload        DMA-BUF -> GLMemory, tiling-aware
//!     -> glcolorconvert  XR24 -> NV12, on the GPU
//!     -> gldownload      GLMemory -> system memory
//!     -> nvh264enc
//! ```
//!
//! **Why the GL hop on NVIDIA.** `cudaupload` was the obvious importer, and
//! it does not accept `memory:DMABuf` in GStreamer 1.28. KWin on NVIDIA
//! hands out tiled buffers — the modifier carries the NVIDIA vendor prefix
//! `0x03` in its top byte — so the fd cannot be mapped and read as linear
//! pixels; the result is a scrambled image that resembles the source but is
//! shifted and banded. `glupload` is the one GStreamer importer that reads
//! the modifier and asks the GL driver to import the buffer accordingly, so
//! it produces correct pixels from a tiled source. `nvh264enc` then accepts
//! the NV12 that `gldownload` produces.

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_allocators::prelude::*;
use gstreamer_allocators::DmaBufAllocator;
use gstreamer_app::{AppSink, AppSrc};
use gstreamer_video::{VideoFormat, VideoFrameFlags, VideoMeta};
use std::os::fd::BorrowedFd;
use std::time::Duration;

/// Which hardware encoder to drive.
///
/// Chosen once, from what is actually registered. `nvh264enc` wins when
/// present because on NVIDIA systems there is no usable VA-API encode path:
/// `nvidia-vaapi-driver` supports decode only, and `vah264enc` either fails
/// to create or produces a stream nothing can decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// `vapostproc` -> `vah264enc`. AMD VCN, Intel QuickSync.
    Vaapi,
    /// `glupload` -> `glcolorconvert` -> `gldownload` -> `nvh264enc`.
    /// NVIDIA NVENC, with a tiling-aware DMA-BUF import.
    NvencGl,
}

fn select_backend() -> Backend {
    if gst::ElementFactory::find("nvh264enc").is_some() {
        Backend::NvencGl
    } else {
        Backend::Vaapi
    }
}

/// DRM format modifiers the encoder chain can import for `fourcc`.
///
/// On the VA path this is probed from `vapostproc`'s advertised
/// `memory:DMABuf` sink caps — the VA stack offers only the tilings VCN can
/// read, and pinning to one of those is what keeps the path zero-copy.
///
/// On the GL path the probe returns just LINEAR, and that is fine: `glupload`
/// imports whatever modifier the caps carry, so the encoder does not need to
/// constrain the compositor's choice.
pub fn supported_modifiers(fourcc: u32) -> Vec<u64> {
    let wanted = fourcc_name(fourcc);
    let mut modifiers = Vec::new();

    if gst::init().is_ok() {
        if let Some(factory) = gst::ElementFactory::find("vapostproc") {
            for template in factory.static_pad_templates() {
                if template.direction() != gst::PadDirection::Sink {
                    continue;
                }
                let caps = template.caps();
                for i in 0..caps.size() {
                    let Some(structure) = caps.structure(i) else {
                        continue;
                    };
                    let Ok(value) = structure.value("drm-format") else {
                        continue;
                    };
                    collect_modifiers(&value, &wanted, &mut modifiers);
                }
            }
        }
    }

    if !modifiers.contains(&0) {
        modifiers.push(0); // DRM_FORMAT_MOD_LINEAR
    }
    modifiers
}

/// `drm-format` is either a single string or a list of `FOURCC:0xMODIFIER`.
fn collect_modifiers(value: &gst::glib::Value, wanted: &str, out: &mut Vec<u64>) {
    if let Ok(text) = value.get::<String>() {
        if let Some(modifier) = parse_drm_format(&text, wanted) {
            if !out.contains(&modifier) {
                out.push(modifier);
            }
        }
        return;
    }
    if let Ok(list) = value.get::<gst::List>() {
        for item in list.iter() {
            collect_modifiers(item, wanted, out);
        }
    }
}

fn parse_drm_format(text: &str, wanted: &str) -> Option<u64> {
    let text = text.trim();
    let (fourcc, modifier) = match text.split_once(':') {
        Some((f, m)) => (f, m),
        None => (text, "0"),
    };
    if fourcc.trim() != wanted {
        return None;
    }
    let modifier = modifier.trim();
    let parsed = modifier
        .strip_prefix("0x")
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .or_else(|| modifier.parse::<u64>().ok())?;
    Some(parsed)
}

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_kbps: u32,
    /// DRM fourcc of the incoming frames (e.g. `XR24`).
    pub fourcc: u32,
    /// DRM format modifier of the incoming frames.
    pub modifier: u64,
    /// Distance between keyframes, in frames.
    pub keyframe_interval: u32,
    /// Encoder speed/quality balance, 1 (quality) .. 7 (speed). VA path only.
    pub target_usage: u32,
    /// `cbr`, `vbr`, or `cqp`.
    pub rate_control: String,
    /// CABAC entropy coding. VA path only.
    pub cabac: bool,
    /// Slices per frame. VA path only.
    pub num_slices: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1200,
            framerate: 60,
            bitrate_kbps: 20_000,
            fourcc: u32::from_le_bytes(*b"XR24"),
            modifier: 0x0200_0000_0000_0901,
            keyframe_interval: 600,
            target_usage: 3,
            rate_control: "vbr".to_string(),
            cabac: true,
            num_slices: 1,
        }
    }
}

/// One encoded access unit.
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub pts_ns: u64,
    pub keyframe: bool,
}

pub struct Encoder {
    pipeline: gst::Pipeline,
    appsrc: AppSrc,
    appsink: AppSink,
    allocator: DmaBufAllocator,
    frame_size: usize,
    width: u32,
    height: u32,
}

impl Encoder {
    pub fn new(config: &EncoderConfig) -> Result<Self> {
        gst::init().context("initialising GStreamer")?;

        let backend = select_backend();
        tracing::info!(
            "encoder backend: {}",
            match backend {
                Backend::Vaapi => "VA-API (vapostproc + vah264enc), zero copy",
                Backend::NvencGl => "NVENC via GL (glupload + glcolorconvert + gldownload + nvh264enc)",
            }
        );
        tracing::info!(
            "encoder input: {}x{} fourcc={} modifier=0x{:016x}",
            config.width,
            config.height,
            fourcc_name(config.fourcc),
            config.modifier,
        );

        let drm_format = format!(
            "{}:0x{:016x}",
            fourcc_name(config.fourcc),
            config.modifier
        );

        // Both backends now accept the same thing on appsrc: the raw DMA-BUF
        // fd, tagged with the DRM format and modifier the compositor wrote.
        // What differs is how the pixels get from there to NV12-in-something
        // the encoder accepts.
        let src_caps = gst::Caps::builder("video/x-raw")
            .features(["memory:DMABuf"])
            .field("format", "DMA_DRM")
            .field("drm-format", &drm_format)
            .field("width", config.width as i32)
            .field("height", config.height as i32)
            .field(
                "framerate",
                gst::Fraction::new(config.framerate as i32, 1),
            )
            .build();

        let pipeline = gst::Pipeline::new();

        let appsrc = gst::ElementFactory::make("appsrc")
            .property("caps", &src_caps)
            .property_from_str("format", "time")
            .property("is-live", true)
            .property("max-buffers", 1u64)
            .property_from_str("leaky-type", "downstream")
            .build()
            .context("creating appsrc")?;

        let mut elements: Vec<gst::Element> = vec![appsrc.clone()];

        match backend {
            Backend::Vaapi => {
                let postproc = gst::ElementFactory::make("vapostproc")
                    .build()
                    .context("creating vapostproc")?;

                let nv12_caps = gst::Caps::builder("video/x-raw")
                    .features(["memory:VAMemory"])
                    .field("format", "NV12")
                    .build();
                let capsfilter = gst::ElementFactory::make("capsfilter")
                    .property("caps", &nv12_caps)
                    .build()
                    .context("creating capsfilter")?;

                let encoder = gst::ElementFactory::make("vah264enc")
                    .property_from_str("rate-control", &config.rate_control)
                    .property("bitrate", config.bitrate_kbps)
                    .property("cpb-size", config.bitrate_kbps / 2)
                    .property("b-frames", 0u32)
                    .property("ref-frames", 1u32)
                    .property("key-int-max", config.keyframe_interval)
                    .property("target-usage", config.target_usage)
                    .property("cabac", config.cabac)
                    .property("num-slices", config.num_slices)
                    .property("aud", true)
                    .build()
                    .context("creating vah264enc")?;

                elements.extend([postproc, capsfilter, encoder]);
            }
            Backend::NvencGl => {
                // `glupload` is the only GStreamer DMA-BUF importer that
                // reads the modifier and asks the GL driver to import the
                // tiled buffer correctly. That is the whole reason this
                // branch exists: a straight mmap of a tiled NVIDIA buffer
                // produces a scrambled image, no matter which element reads
                // it afterwards.
                let upload = gst::ElementFactory::make("glupload")
                    .build()
                    .context(
                        "creating glupload — gst-plugins-base needs to be built \
                         with GL support, which is normal on any desktop",
                    )?;

                let convert = gst::ElementFactory::make("glcolorconvert")
                    .build()
                    .context("creating glcolorconvert")?;

                let download = gst::ElementFactory::make("gldownload")
                    .build()
                    .context("creating gldownload")?;

                // `nvh264enc` wants NV12 on its sink pad; pin the caps so
                // GStreamer does not negotiate something GL-only that the
                // encoder cannot read.
                let nv12_caps = gst::Caps::builder("video/x-raw")
                    .field("format", "NV12")
                    .build();
                let capsfilter = gst::ElementFactory::make("capsfilter")
                    .property("caps", &nv12_caps)
                    .build()
                    .context("creating capsfilter")?;

                // `nvh264enc` property names verified against
                // `gst-inspect-1.0 nvh264enc` on GStreamer 1.28.7. Only
                // properties known to exist are set — gstreamer-rs's
                // `ElementBuilder::property` *panics* on a missing one.
                let encoder = gst::ElementFactory::make("nvh264enc")
                    .property("bitrate", config.bitrate_kbps)
                    .property("gop-size", config.keyframe_interval as i32)
                    .property("zerolatency", true)
                    // `aud` is verified present on nvh264enc (it is in the
                    // property dump from earlier in this build). Emitting
                    // access-unit delimiters gives `h264parse` explicit
                    // boundaries instead of relying on it inferring them
                    // from NAL types, which the GL chain can fragment
                    // differently than the videoconvert chain did.
                    .property("aud", true)
                    .build()
                    .context("creating nvh264enc")?;

                elements.extend([upload, convert, download, capsfilter, encoder]);
            }
        }

        let parser = gst::ElementFactory::make("h264parse")
            .property("config-interval", -1i32)
            .build()
            .context("creating h264parse")?;

        let parsed_caps = gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        let parsed_filter = gst::ElementFactory::make("capsfilter")
            .property("caps", &parsed_caps)
            .build()
            .context("creating h264 capsfilter")?;

        let appsink = gst::ElementFactory::make("appsink")
            .property("sync", false)
            .property("max-buffers", 4u32)
            .property("drop", false)
            .build()
            .context("creating appsink")?;

        elements.extend([parser, parsed_filter, appsink.clone()]);

        pipeline
            .add_many(elements.iter())
            .context("adding elements")?;
        gst::Element::link_many(elements.iter()).context("linking pipeline")?;

        let appsrc = appsrc.dynamic_cast::<AppSrc>().unwrap();
        let appsink = appsink.dynamic_cast::<AppSink>().unwrap();

        pipeline
            .set_state(gst::State::Playing)
            .context("starting pipeline")?;

        // `set_state` is asynchronous: it returns as soon as the transition
        // is scheduled, not when it has completed. On an `appsrc`-driven
        // live pipeline that is the behaviour we want — the first
        // `push_frame` supplies the buffer that lets the transition finish,
        // and blocking here would deadlock against the pipeline waiting for
        // exactly that buffer. The alternative — asserting PLAYING before
        // returning — was tried and made every run produce zero packets,
        // so the immediate-return form is what stays.

        Ok(Self {
            pipeline,
            appsrc,
            appsink,
            allocator: DmaBufAllocator::new(),
            frame_size: (config.width as usize) * (config.height as usize) * 4,
            width: config.width,
            height: config.height,
        })
    }

    /// Submit a captured frame.
    ///
    /// The fd is wrapped as a `GstMemory` tagged with `VideoMeta` carrying
    /// the fourcc and modifier from the caps. Both backends read the same
    /// DMA-BUF; what happens next is what differs.
    ///
    /// `alloc_dmabuf` takes ownership of the fd and closes it with the
    /// memory, so the fd is duplicated first — otherwise GStreamer would
    /// close the capture pool's buffer out from under the compositor.
    pub fn push_frame(
        &self,
        fd: BorrowedFd<'_>,
        offset: u32,
        stride: u32,
        pts_ns: u64,
    ) -> Result<()> {
        let owned = fd
            .try_clone_to_owned()
            .context("duplicating DMA-BUF fd for the encoder")?;

        let memory = unsafe {
            self.allocator
                .alloc_dmabuf(owned, self.frame_size)
                .context("wrapping DMA-BUF fd as GstMemory")?
        };

        let mut buffer = gst::Buffer::new();
        {
            let buf = buffer.get_mut().unwrap();
            buf.append_memory(memory);

            VideoMeta::add_full(
                buf,
                VideoFrameFlags::empty(),
                VideoFormat::DmaDrm,
                self.width,
                self.height,
                &[offset as usize],
                &[stride as i32],
            )
            .context("adding DMA-BUF VideoMeta")?;

            buf.set_pts(gst::ClockTime::from_nseconds(pts_ns));
        }

        self.appsrc
            .push_buffer(buffer)
            .map_err(|e| anyhow::anyhow!("pushing buffer into appsrc: {e:?}"))?;
        Ok(())
    }

    /// Pull the next encoded access unit, waiting up to `timeout`.
    pub fn pull_packet(&self, timeout: Duration) -> Result<Option<EncodedPacket>> {
        let sample = match self
            .appsink
            .try_pull_sample(gst::ClockTime::from_nseconds(timeout.as_nanos() as u64))
        {
            Some(sample) => sample,
            None => {
                self.check_bus()?;
                return Ok(None);
            }
        };

        let buffer = sample.buffer().context("sample carried no buffer")?;
        let map = buffer.map_readable().context("mapping encoded buffer")?;
        let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);

        Ok(Some(EncodedPacket {
            data: map.as_slice().to_vec(),
            pts_ns: buffer.pts().map(|t| t.nseconds()).unwrap_or(0),
            keyframe,
        }))
    }

    /// Surface any pipeline error rather than letting it stall silently.
    fn check_bus(&self) -> Result<()> {
        let Some(bus) = self.pipeline.bus() else {
            return Ok(());
        };
        while let Some(msg) = bus.pop() {
            if let gst::MessageView::Error(err) = msg.view() {
                bail!(
                    "pipeline error from {}: {} ({})",
                    err.src().map(|s| s.path_string()).unwrap_or_default(),
                    err.error(),
                    err.debug().unwrap_or_default()
                );
            }
        }
        Ok(())
    }

    pub fn stop(&self) {
        let _ = self.appsrc.end_of_stream();
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn fourcc_name(code: u32) -> String {
    String::from_utf8_lossy(&code.to_le_bytes()).into_owned()
}
