// SPDX-License-Identifier: Apache-2.0

//! Common surface for the two capture backends.
//!
//! The `ext-image-copy-capture` path is request/response: we hand the
//! compositor a buffer from our pool, it writes into it, we read it back. The
//! PipeWire path is push: KWin hands us buffers as it produces them. Neither
//! shape is wrong, but `daemon::session` should not have to care which one it
//! is driving, so both are flattened to "give me the next frame".

use anyhow::Result;
use std::os::fd::OwnedFd;

/// One captured frame, owning its DMA-BUF fd.
///
/// The fd is dup'd at the boundary rather than borrowed because the two
/// backends have different lifetimes: on the ext- path the fd lives in a pool
/// that outlives the frame, while on the PipeWire path it dies the instant the
/// process callback returns. Dup'ing normalises them at negligible cost — one
/// syscall, against a 16.7 ms frame budget.
pub struct Frame {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
    pub pts_ns: Option<u64>,
}

pub trait FrameSource {
    /// Block until a fresh frame is available.
    fn next_frame(&mut self) -> Result<Frame>;

    fn width(&self) -> u32;
    fn height(&self) -> u32;
    /// DRM fourcc of the negotiated format.
    fn format(&self) -> u32;
    /// DRM format modifier of the negotiated format.
    fn modifier(&self) -> u64;

    /// Block until the backend has actually negotiated a format.
    ///
    /// The ext- path negotiates synchronously inside its constructor, so
    /// this returns immediately. The PipeWire path does not: KWin's
    /// `param_changed` callback fires on the PipeWire thread some time after
    /// `PlasmaCapture::new` returns. Between those two moments the accessors
    /// above read zeros, and a consumer that queries them gets a malformed
    /// configuration.
    ///
    /// The default implementation is a no-op, which is correct for sources
    /// that already know their geometry by the time they are constructed.
    fn wait_for_format(&mut self) -> Result<()> {
        Ok(())
    }
}
