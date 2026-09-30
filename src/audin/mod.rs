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

#[cfg(test)]
mod tests {
    //! The format negotiation of the vendored `AudinServer`, driven through the
    //! MS-RDPEAI messages a client sends. (The vendored crate's own tests can't
    //! run from here — it isn't a workspace member.)

    use std::sync::{Arc, Mutex};

    use ironrdp_dvc::DvcProcessor as _;
    use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
    use ironrdp_server::{AudinSampleSink, AudinServer};

    const VERSION: u8 = 0x01;
    const FORMATS: u8 = 0x02;
    const OPEN: u8 = 0x03;
    const DATA: u8 = 0x06;
    const FORMAT_CHANGE: u8 = 0x07;

    #[derive(Default)]
    struct Seen {
        formats: Vec<AudioFormat>,
        data_packets: usize,
    }

    struct RecordingSink(Arc<Mutex<Seen>>);

    impl AudinSampleSink for RecordingSink {
        fn on_format(&mut self, format: &AudioFormat) {
            self.0.lock().unwrap().formats.push(format.clone());
        }
        fn on_data(&mut self, _data: &[u8]) {
            self.0.lock().unwrap().data_packets += 1;
        }
    }

    fn server() -> (AudinServer, Arc<Mutex<Seen>>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        (
            AudinServer::new(Some(Box::new(RecordingSink(seen.clone())))),
            seen,
        )
    }

    fn pcm(channels: u16, rate: u32, bits: u16) -> AudioFormat {
        let block_align = channels * (bits / 8);
        AudioFormat {
            format: WaveFormat::PCM,
            n_channels: channels,
            n_samples_per_sec: rate,
            n_avg_bytes_per_sec: rate * u32::from(block_align),
            n_block_align: block_align,
            bits_per_sample: bits,
            data: None,
        }
    }

    fn message(id: u8, body: &[u8]) -> Vec<u8> {
        std::iter::once(id).chain(body.iter().copied()).collect()
    }

    /// A client Formats PDU: NumFormats | cbSizeFormatsPacket | AUDIO_FORMAT[].
    fn formats_message(formats: &[AudioFormat]) -> Vec<u8> {
        let encoded: Vec<u8> = formats
            .iter()
            .flat_map(|f| ironrdp_core::encode_vec(f).unwrap())
            .collect();
        let mut body = (formats.len() as u32).to_le_bytes().to_vec();
        body.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
        body.extend_from_slice(&encoded);
        message(FORMATS, &body)
    }

    /// Feed one client message; return the server's replies, encoded.
    fn feed(server: &mut AudinServer, payload: &[u8]) -> Vec<Vec<u8>> {
        server
            .process(0, payload)
            .unwrap()
            .iter()
            .map(|m| ironrdp_core::encode_vec(&**m).unwrap())
            .collect()
    }

    /// The server's own Formats list, decoded from its reply to the client's Version.
    fn advertised(server: &mut AudinServer) -> Vec<AudioFormat> {
        let reply = feed(server, &message(VERSION, &1u32.to_le_bytes()));
        assert_eq!(reply.len(), 1);
        assert_eq!(reply[0][0], FORMATS);
        let body = &reply[0][1..];
        let count = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
        let mut cursor = ironrdp_core::ReadCursor::new(&body[8..]);
        (0..count)
            .map(|_| <AudioFormat as ironrdp_core::Decode>::decode(&mut cursor).unwrap())
            .collect()
    }

    /// The `initialFormat` index in an Open PDU.
    fn open_index(open: &[u8]) -> u32 {
        assert_eq!(open[0], OPEN);
        u32::from_le_bytes(open[5..9].try_into().unwrap())
    }

    /// Only the device's own format is advertised: 16-bit PCM at 44.1 kHz.
    #[test]
    fn only_44_1_khz_16_bit_pcm_is_advertised() {
        let (mut server, _) = server();
        let formats = advertised(&mut server);
        assert_eq!(formats.len(), 2, "mono and stereo");
        for f in &formats {
            assert_eq!(f.format, WaveFormat::PCM);
            assert_eq!(f.n_samples_per_sec, 44_100);
            assert_eq!(f.bits_per_sample, 16);
        }
    }

    /// A client may list formats the server didn't advertise, in any order. The
    /// server opens the first acceptable one — by its real index, not 0.
    #[test]
    fn the_first_acceptable_client_format_is_opened_by_its_index() {
        let (mut server, seen) = server();
        let reply = feed(
            &mut server,
            &formats_message(&[pcm(2, 48_000, 16), pcm(1, 44_100, 8), pcm(1, 44_100, 16)]),
        );
        assert_eq!(reply.len(), 1, "an Open PDU");
        assert_eq!(
            open_index(&reply[0]),
            2,
            "the index of the 16-bit 44.1 kHz entry"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.formats, vec![pcm(1, 44_100, 16)]);
    }

    /// No acceptable format: the mic isn't opened, and any data is not fed on.
    #[test]
    fn nothing_is_opened_without_an_acceptable_format() {
        let (mut server, seen) = server();
        let reply = feed(&mut server, &formats_message(&[pcm(2, 48_000, 16)]));
        assert!(reply.is_empty(), "no Open PDU");
        feed(&mut server, &message(DATA, &[0u8; 8]));
        let seen = seen.lock().unwrap();
        assert!(seen.formats.is_empty());
        assert_eq!(seen.data_packets, 0);
    }

    /// A mid-stream Format Change to an unsupported entry stops the feed rather
    /// than passing on audio in the wrong format; a change back resumes it.
    #[test]
    fn a_format_change_is_followed_only_to_an_acceptable_format() {
        let (mut server, seen) = server();
        feed(
            &mut server,
            &formats_message(&[pcm(1, 44_100, 16), pcm(2, 48_000, 16)]),
        );
        feed(&mut server, &message(DATA, &[0u8; 8]));
        assert_eq!(seen.lock().unwrap().data_packets, 1);

        feed(&mut server, &message(FORMAT_CHANGE, &1u32.to_le_bytes()));
        feed(&mut server, &message(DATA, &[0u8; 8]));
        assert_eq!(
            seen.lock().unwrap().data_packets,
            1,
            "48 kHz audio is dropped"
        );

        feed(&mut server, &message(FORMAT_CHANGE, &0u32.to_le_bytes()));
        feed(&mut server, &message(DATA, &[0u8; 8]));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.data_packets, 2, "the feed resumes");
        assert_eq!(seen.formats.last(), Some(&pcm(1, 44_100, 16)));
    }

    /// A client reporting a higher protocol version still gets the version-1
    /// exchange (version 1 is the minimum, so there is nothing to negotiate).
    #[test]
    fn a_newer_client_version_is_served_at_version_1() {
        let (mut server, _) = server();
        let reply = feed(&mut server, &message(VERSION, &2u32.to_le_bytes()));
        assert_eq!(reply.len(), 1);
        assert_eq!(reply[0][0], FORMATS);
    }
}
