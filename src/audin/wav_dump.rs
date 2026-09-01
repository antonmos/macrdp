//! Phase-1 mic-dump sink (opt-in `MACRDP_MIC_DUMP=1`): write the received
//! microphone PCM to a WAV under `$TMPDIR` so the captured audio can be played
//! back and verified — the audio analogue of `MACRDP_CAMERA_DUMP`. It proves the
//! negotiated PCM the client streams is real, correctly-framed audio *before*
//! Phase 2 (the `AudioServerPlugIn` virtual mic) is built on top of the same
//! `AudinSampleSink` seam. Off by default: when the env var is unset the factory
//! hands the processor `None` and this is never constructed.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ironrdp_rdpsnd::pdu::AudioFormat;
use ironrdp_server::AudinSampleSink;
use tracing::{info, warn};

/// Writes the inbound mic PCM to a canonical 44-byte-header WAV. The two size
/// fields (RIFF `ChunkSize` @ offset 4, `data` `Subchunk2Size` @ offset 40) are
/// re-patched from `data_bytes` **periodically while streaming** (every
/// [`FINALIZE_EVERY_PACKETS`]) and again on drop, so the file is playable at any
/// point without waiting for the connection to tear down — the `Drop` alone
/// wouldn't run until the whole `macrdp` process exits when the dump is taken via
/// a foreground run.
pub struct WavDumpSink {
    file: Option<File>,
    data_bytes: u32,
    packets: u64,
    path: Option<PathBuf>,
}

/// Re-patch the WAV size headers every this many Data PDUs (~0.5 s at the
/// ~100 pkt/s the client streams), then seek back to the end to keep appending.
const FINALIZE_EVERY_PACKETS: u64 = 100;

impl WavDumpSink {
    pub fn new() -> Self {
        Self {
            file: None,
            data_bytes: 0,
            packets: 0,
            path: None,
        }
    }
}

/// Patch the RIFF `ChunkSize` (offset 4) and `data` `Subchunk2Size` (offset 40)
/// from the running byte count, then seek back to the end so the next append
/// continues writing PCM rather than overwriting the header. Best-effort: a
/// failed seek/write just leaves the last-patched sizes in place.
fn patch_wav_sizes(f: &mut File, data_bytes: u32) {
    let riff_len = 36u32.saturating_add(data_bytes);
    if f.seek(SeekFrom::Start(4)).is_ok() {
        let _ = f.write_all(&riff_len.to_le_bytes());
    }
    if f.seek(SeekFrom::Start(40)).is_ok() {
        let _ = f.write_all(&data_bytes.to_le_bytes());
    }
    // Restore the append position (end of the data written so far) so on_data's
    // next write extends the PCM instead of clobbering the header.
    let _ = f.seek(SeekFrom::End(0));
}

impl Default for WavDumpSink {
    fn default() -> Self {
        Self::new()
    }
}

/// The 44-byte canonical PCM WAV/RIFF header. Both length fields are written as
/// `0` here and patched on finalize (see [`WavDumpSink::drop`]).
fn write_wav_header(f: &mut File, format: &AudioFormat) -> std::io::Result<()> {
    let channels = format.n_channels;
    let rate = format.n_samples_per_sec;
    let bits = format.bits_per_sample;
    let block_align = channels.saturating_mul(bits / 8);
    let byte_rate = rate.saturating_mul(u32::from(block_align));

    f.write_all(b"RIFF")?;
    f.write_all(&0u32.to_le_bytes())?; // ChunkSize — patched on drop
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?; // Subchunk1Size (PCM)
    f.write_all(&1u16.to_le_bytes())?; // AudioFormat = PCM (Phase 0 only negotiates PCM)
    f.write_all(&channels.to_le_bytes())?;
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&block_align.to_le_bytes())?;
    f.write_all(&bits.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&0u32.to_le_bytes())?; // Subchunk2Size — patched on drop
    Ok(())
}

impl AudinSampleSink for WavDumpSink {
    fn on_format(&mut self, format: &AudioFormat) {
        let dir = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!("macrdp-mic-{}-{}.wav", std::process::id(), nanos));
        match File::create(&path) {
            Ok(mut f) => match write_wav_header(&mut f, format) {
                Ok(()) => {
                    info!(
                        path = %path.display(),
                        channels = format.n_channels,
                        rate = format.n_samples_per_sec,
                        bits = format.bits_per_sample,
                        "MS-RDPEAI mic dump: writing received PCM to WAV (MACRDP_MIC_DUMP)"
                    );
                    self.file = Some(f);
                    self.path = Some(path);
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "MS-RDPEAI mic dump: WAV header write failed")
                }
            },
            Err(e) => {
                warn!(path = %path.display(), error = %e, "MS-RDPEAI mic dump: could not create WAV")
            }
        }
    }

    fn on_data(&mut self, data: &[u8]) {
        let Some(f) = self.file.as_mut() else {
            return;
        };
        if f.write_all(data).is_err() {
            return;
        }
        self.data_bytes = self.data_bytes.saturating_add(data.len() as u32);
        self.packets = self.packets.wrapping_add(1);
        // Keep the size headers current so the dump plays back mid-stream.
        if self.packets.is_multiple_of(FINALIZE_EVERY_PACKETS) {
            if let Some(f) = self.file.as_mut() {
                patch_wav_sizes(f, self.data_bytes);
            }
        }
    }
}

impl Drop for WavDumpSink {
    fn drop(&mut self) {
        let Some(mut f) = self.file.take() else {
            return;
        };
        patch_wav_sizes(&mut f, self.data_bytes);
        let _ = f.flush();
        info!(
            bytes = self.data_bytes,
            path = self
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            "MS-RDPEAI mic dump: finalized WAV"
        );
    }
}
