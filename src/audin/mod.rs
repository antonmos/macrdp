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

#[cfg(target_os = "macos")]
mod shm_sink;

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
        // Sink selection:
        //  * `MACRDP_MIC_DUMP=1` → WAV dump under `$TMPDIR` (Phase-1 debug: play
        //    it back to verify the decode; the audio analogue of
        //    `MACRDP_CAMERA_DUMP`). Overrides the feed.
        //  * otherwise (macOS) → the Phase-2 `SharedMemSink` feed into the
        //    "macrdp Microphone" HAL plug-in's shared ring. If the ring can't be
        //    mapped, fall back to `None` (negotiate + drop) so a mic setup
        //    problem never kills the session.
        //  * non-macOS → `None` (there is no virtual mic to feed).
        let sink: Option<Box<dyn AudinSampleSink>> =
            if std::env::var_os("MACRDP_MIC_DUMP").is_some() {
                Some(Box::new(WavDumpSink::new()))
            } else {
                #[cfg(target_os = "macos")]
                {
                    shm_sink::SharedMemSink::new().map(|s| Box::new(s) as Box<dyn AudinSampleSink>)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    None
                }
            };
        AudinServer::new(sink)
    }
}
