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
/// left `0` while streaming and patched from `data_bytes` on drop, so even an
/// abrupt disconnect leaves a file most players accept (only the unwritten size
/// header, not the audio, is affected).
pub struct WavDumpSink {
    file: Option<File>,
    data_bytes: u32,
    path: Option<PathBuf>,
}

impl WavDumpSink {
    pub fn new() -> Self {
        Self {
            file: None,
            data_bytes: 0,
            path: None,
        }
    }
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
        if let Some(f) = self.file.as_mut() {
            if f.write_all(data).is_ok() {
                self.data_bytes = self.data_bytes.saturating_add(data.len() as u32);
            }
        }
    }
}

impl Drop for WavDumpSink {
    fn drop(&mut self) {
        let Some(mut f) = self.file.take() else {
            return;
        };
        let riff_len = 36u32.saturating_add(self.data_bytes);
        if f.seek(SeekFrom::Start(4)).is_ok() {
            let _ = f.write_all(&riff_len.to_le_bytes());
        }
        if f.seek(SeekFrom::Start(40)).is_ok() {
            let _ = f.write_all(&self.data_bytes.to_le_bytes());
        }
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
