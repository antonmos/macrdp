//! Microphone / audio-input redirection (MS-RDPEAI) — the macrdp side.
//!
//! The RDP client redirects its microphone (a standalone mic, or a webcam's
//! built-in mic) over the `AUDIO_INPUT` dynamic virtual channel; macrdp is the
//! server and receives it, then presents it to macOS apps as a real input device,
//! "macrdp Microphone" — an `AudioServerPlugIn` HAL plug-in
//! (`audioplugin/macrdp_mic.c`) that reads the PCM from a shared-memory ring this
//! module's `SharedMemSink` writes.
//!
//! The protocol state machine lives in the vendored `ironrdp-server`
//! (`src/audin.rs`); this is the cross-platform factory that installs it, gated
//! behind `--enable-microphone-redirection`, and picks the sink behind the
//! vendored `AudinSampleSink` seam. See `TODO.md` ("Microphone / audio-input
//! redirection").

use ironrdp_server::{AudinSampleSink, AudinServer, AudinServerFactory};

mod wav_dump;
use wav_dump::WavDumpSink;

#[cfg(target_os = "macos")]
mod shm_sink;

/// The macrdp MS-RDPEAI factory. Cross-platform; the virtual-mic sink it builds is
/// macOS-only.
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
        //  * `MACRDP_MIC_DUMP=1` → WAV dump under `$TMPDIR` (debug: play it back to
        //    verify the decode; the audio analogue of `MACRDP_CAMERA_DUMP`).
        //    Overrides the feed.
        //  * otherwise (macOS) → `SharedMemSink`, feeding the "macrdp Microphone"
        //    plug-in. It creates nothing until the client negotiates the mic, so a
        //    connection that never gets that far (including an unauthenticated
        //    one — the channel can't open before the session is connected) leaves
        //    no trace.
        //  * non-macOS → `None` (there is no virtual mic to feed).
        let sink: Option<Box<dyn AudinSampleSink>> = if mic_dump_enabled() {
            tracing::info!(
                "mic: MACRDP_MIC_DUMP set — dumping received PCM to WAV, NOT feeding the virtual mic"
            );
            Some(Box::new(WavDumpSink::new()))
        } else {
            #[cfg(target_os = "macos")]
            {
                Some(Box::new(shm_sink::SharedMemSink::new()))
            }
            #[cfg(not(target_os = "macos"))]
            {
                None
            }
        };
        AudinServer::new(sink)
    }
}

/// `MACRDP_MIC_DUMP=1` (or `true`) — the same rule as `MACRDP_CAMERA_DUMP`, so
/// `MACRDP_MIC_DUMP=0` means off rather than on.
pub(crate) fn mic_dump_enabled() -> bool {
    matches!(
        std::env::var("MACRDP_MIC_DUMP").as_deref(),
        Ok("1") | Ok("true")
    )
}
