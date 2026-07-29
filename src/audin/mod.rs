//! Microphone / audio-input redirection (MS-RDPEAI) — the macrdp side.
//!
//! The RDP client redirects its microphone (a standalone mic, or a webcam's
//! built-in mic) over the `AUDIO_INPUT` dynamic virtual channel; macrdp is the
//! server and receives it. **Phase 0 (this module): negotiate the channel and log
//! the client streaming its mic** — the go/no-go gate proving mstsc/FreeRDP hand
//! macrdp a mic over a server-direction DVC, before any macOS virtual-audio work.
//!
//! The protocol state machine lives in the vendored `ironrdp-server`
//! (`src/audin.rs`, divergence 22); this is just the cross-platform factory that
//! installs it, gated behind `--enable-microphone-redirection`. Phase 2 will add a
//! macOS virtual microphone (an `AudioServerPlugIn` HAL plug-in fed via
//! shared-memory) behind the vendored `AudinSampleSink` seam; `build_processor`
//! will then hand the processor a `Some(sink)` instead of `None`. See
//! `TODO.md` ("Microphone / audio-input redirection").

use ironrdp_server::{AudinServer, AudinServerFactory};

/// The macrdp MS-RDPEAI factory. Cross-platform — Phase 0 has no platform code (it
/// only negotiates + logs). Phase 2's virtual-mic sink sits behind
/// `AudinSampleSink`, built here.
pub struct MacAudin;

impl MacAudin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MacAudin {
    fn default() -> Self {
        Self::new()
    }
}

impl AudinServerFactory for MacAudin {
    fn build_processor(&self) -> AudinServer {
        // Phase 0: no sink → the processor negotiates, logs the mic stream, and
        // drops the audio (the go/no-go gate). Phase 2 passes `Some(sink)`.
        AudinServer::new(None)
    }
}
