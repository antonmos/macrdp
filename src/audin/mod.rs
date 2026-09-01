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

use ironrdp_server::{AudinSampleSink, AudinServer, AudinServerFactory};

mod wav_dump;
use wav_dump::WavDumpSink;

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
        // Phase 1: with `MACRDP_MIC_DUMP=1`, capture the received PCM to a WAV
        // under `$TMPDIR` so it can be played back to verify the decode (the
        // audio analogue of `MACRDP_CAMERA_DUMP`). Off → `None` (Phase-0
        // negotiate + log + drop). Phase 2 replaces this with the
        // `AudioServerPlugIn` feed sink.
        let sink: Option<Box<dyn AudinSampleSink>> =
            if std::env::var_os("MACRDP_MIC_DUMP").is_some() {
                Some(Box::new(WavDumpSink::new()))
            } else {
                None
            };
        AudinServer::new(sink)
    }
}
