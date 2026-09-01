//! Phase-2 mic feed (`SharedMemSink`): write the received MS-RDPEAI PCM into the
//! POSIX shared-memory ring that the "macrdp Microphone" CoreAudio HAL plug-in
//! (`audioplugin/macrdp_mic.c`) reads in `DoIOOperation`. macrdp is the sole
//! producer, the plug-in (inside `coreaudiod`) the sole consumer — a
//! single-producer/single-consumer ring, proven to cross the user boundary
//! (macrdp runs as the logged-in user, `coreaudiod` as `_coreaudiod`) in P2b-0.
//!
//! **This layout MUST stay byte-identical to `audioplugin/macrdp_mic_ring.h`.**
//! The plug-in maps the same segment and interprets the same struct; a field
//! reorder or size change on either side silently corrupts the audio.
//!
//! P2b-1 scope: convert 16-bit PCM (mono → upmixed stereo, or stereo) at the ring
//! rate (44.1 kHz) into the ring. Resampling a differing client rate, a jitter
//! buffer, and drift handling are P2c.

use std::ffi::CStr;
use std::os::raw::c_void;
use std::ptr::addr_of_mut;
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

use ironrdp_rdpsnd::pdu::AudioFormat;
use ironrdp_server::AudinSampleSink;
use tracing::{info, warn};

/// POSIX shm object name — must equal `MACRDP_MIC_SHM_NAME` in the C header.
const SHM_NAME: &CStr = c"/macrdp_mic_ring";
/// 'MRDP' — must equal `MACRDP_MIC_MAGIC`.
const MAGIC: u32 = 0x4d52_4450;
const VERSION: u32 = 1;
/// Interleaved channel count in the ring — must equal `MACRDP_MIC_CHANNELS`.
const RING_CHANNELS: u32 = 2;
/// Ring capacity in frames (power of two) — must equal `MACRDP_MIC_RING_FRAMES`.
const RING_FRAMES: u32 = 65536;
const RING_SAMPLES: usize = (RING_FRAMES * RING_CHANNELS) as usize;
/// The rate the plug-in advertises + consumes at. A client streaming a different
/// rate is out of scope for P2b-1 (P2c resamples); we warn and write anyway.
const RING_RATE: u32 = 44100;

/// The shared ring — a byte-for-byte mirror of `MacrdpMicRing` in
/// `audioplugin/macrdp_mic_ring.h`. `#[repr(C)]` + the same field order/types is
/// what makes the two views agree.
#[repr(C)]
struct MicRing {
    magic: AtomicU32,
    version: u32,
    sample_rate: u32,
    channels: u32,
    ring_frames: u32,
    reserved: u32,
    write_frames: AtomicU64,
    read_frames: AtomicU64,
    samples: [f32; RING_SAMPLES],
}

/// Writes received mic PCM into the shared ring. The mapping lives for the life
/// of the sink (one per connection); `Drop` unmaps it. The segment itself is
/// intentionally NOT `shm_unlink`ed so a reconnect reuses it and the write cursor
/// continues (avoiding a reader-side underrun stall on reconnect).
pub struct SharedMemSink {
    ring: *mut MicRing,
    fd: i32,
    src_channels: u16,
    src_bits: u16,
    src_rate: u32,
    logged_unsupported: bool,
}

// The raw pointer is only ever touched from the single audin processing task
// (the trait is `Send`, called serially). SPSC ordering with the reader is via
// the atomics. Asserting Send lets the sink live behind the `Box<dyn …>` seam.
unsafe impl Send for SharedMemSink {}

impl SharedMemSink {
    /// Map (creating if needed) the shared ring. Returns `None` on any failure so
    /// the factory can fall back to the inert (`None`) sink — a mic feature that
    /// can't set up its feed must not kill the session.
    pub fn new() -> Option<Self> {
        // SAFETY: standard POSIX shm bring-up; every return path is checked.
        unsafe {
            let fd = libc::shm_open(SHM_NAME.as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o666);
            if fd < 0 {
                warn!(errno = errno(), "mic feed: shm_open failed");
                return None;
            }
            // Force 0666 past the umask so coreaudiod (a different user) can open it.
            libc::fchmod(fd, 0o666);
            let size = std::mem::size_of::<MicRing>();
            if libc::ftruncate(fd, size as libc::off_t) != 0 {
                warn!(errno = errno(), "mic feed: ftruncate failed");
                libc::close(fd);
                return None;
            }
            let p = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                warn!(errno = errno(), "mic feed: mmap failed");
                libc::close(fd);
                return None;
            }
            let ring = p as *mut MicRing;

            // Initialize the header. Reuse an already-valid segment's cursors so a
            // reconnect continues writing (a fresh write_frames=0 while the reader
            // holds a large read_frames would underrun-stall the plug-in).
            let was_valid = (*ring).magic.load(Ordering::Acquire) == MAGIC
                && addr_of_mut!((*ring).ring_frames).read_volatile() == RING_FRAMES;

            addr_of_mut!((*ring).version).write(VERSION);
            addr_of_mut!((*ring).sample_rate).write(RING_RATE);
            addr_of_mut!((*ring).channels).write(RING_CHANNELS);
            addr_of_mut!((*ring).ring_frames).write(RING_FRAMES);
            addr_of_mut!((*ring).reserved).write(0);
            if !was_valid {
                (*ring).write_frames.store(0, Ordering::Relaxed);
                (*ring).read_frames.store(0, Ordering::Relaxed);
            }
            // Publish the header before the magic so a reader that sees the magic
            // (acquire) sees a consistent header (release pairs with its acquire).
            fence(Ordering::Release);
            (*ring).magic.store(MAGIC, Ordering::Release);

            info!(
                shm = ?SHM_NAME,
                reused = was_valid,
                "mic feed: shared ring mapped (Float32 stereo {RING_RATE} Hz, {RING_FRAMES} frames)"
            );
            Some(Self {
                ring,
                fd,
                src_channels: 1,
                src_bits: 16,
                src_rate: RING_RATE,
                logged_unsupported: false,
            })
        }
    }
}

impl AudinSampleSink for SharedMemSink {
    fn on_format(&mut self, format: &AudioFormat) {
        self.src_channels = format.n_channels.max(1);
        self.src_bits = format.bits_per_sample;
        self.src_rate = format.n_samples_per_sec;
        self.logged_unsupported = false;
        if self.src_rate != RING_RATE {
            warn!(
                src_rate = self.src_rate,
                ring_rate = RING_RATE,
                "mic feed: client rate != device rate — pitch will be off until P2c resampling"
            );
        }
        info!(
            channels = self.src_channels,
            rate = self.src_rate,
            bits = self.src_bits,
            "mic feed: negotiated capture format"
        );
    }

    fn on_data(&mut self, data: &[u8]) {
        // P2b-1: 16-bit PCM only. Other depths need a converter (P2c) — log once.
        if self.src_bits != 16 {
            if !self.logged_unsupported {
                warn!(
                    bits = self.src_bits,
                    "mic feed: only 16-bit PCM handled so far — dropping"
                );
                self.logged_unsupported = true;
            }
            return;
        }
        let src_ch = self.src_channels as usize;
        let bytes_per_frame = 2 * src_ch;
        if bytes_per_frame == 0 {
            return;
        }
        let frames = data.len() / bytes_per_frame;
        if frames == 0 {
            return;
        }

        let ring = self.ring;
        // SAFETY: `ring` is a live MAP_SHARED mapping for the sink's lifetime;
        // `samples` is written only here (sole producer) via a raw pointer (no
        // `&` to the plain `[f32]` is formed), and `write_frames` is published
        // with Release so the reader's Acquire sees the samples.
        unsafe {
            let cap = RING_FRAMES as u64;
            let samples = addr_of_mut!((*ring).samples) as *mut f32;
            let wf = &(*ring).write_frames;
            let mut w = wf.load(Ordering::Relaxed); // sole writer owns it
            for f in 0..frames {
                let base = f * bytes_per_frame;
                let (l, r) = if src_ch == 1 {
                    let s = i16::from_le_bytes([data[base], data[base + 1]]) as f32 / 32768.0;
                    (s, s)
                } else {
                    let l = i16::from_le_bytes([data[base], data[base + 1]]) as f32 / 32768.0;
                    let r = i16::from_le_bytes([data[base + 2], data[base + 3]]) as f32 / 32768.0;
                    (l, r)
                };
                let idx = ((w % cap) as usize) * 2;
                *samples.add(idx) = l;
                *samples.add(idx + 1) = r;
                w += 1;
            }
            wf.store(w, Ordering::Release);
        }
    }
}

impl Drop for SharedMemSink {
    fn drop(&mut self) {
        // SAFETY: unmap the mapping we created; leave the segment for reuse.
        unsafe {
            if !self.ring.is_null() {
                libc::munmap(self.ring as *mut c_void, std::mem::size_of::<MicRing>());
            }
            if self.fd >= 0 {
                libc::close(self.fd);
            }
        }
        info!("mic feed: shared ring unmapped");
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Rust `MicRing` must be byte-for-byte identical to the C `MacrdpMicRing`
    /// in `audioplugin/macrdp_mic_ring.h` — the two processes map the same bytes.
    /// These offsets/size are the C layout (6×u32 header, two 8-byte atomics at
    /// 24/32, samples at 40).
    #[test]
    fn layout_matches_c_header() {
        assert_eq!(std::mem::offset_of!(MicRing, magic), 0);
        assert_eq!(std::mem::offset_of!(MicRing, version), 4);
        assert_eq!(std::mem::offset_of!(MicRing, sample_rate), 8);
        assert_eq!(std::mem::offset_of!(MicRing, channels), 12);
        assert_eq!(std::mem::offset_of!(MicRing, ring_frames), 16);
        assert_eq!(std::mem::offset_of!(MicRing, reserved), 20);
        assert_eq!(std::mem::offset_of!(MicRing, write_frames), 24);
        assert_eq!(std::mem::offset_of!(MicRing, read_frames), 32);
        assert_eq!(std::mem::offset_of!(MicRing, samples), 40);
        // 40-byte header + 65536 frames × 2 ch × 4 bytes.
        assert_eq!(std::mem::size_of::<MicRing>(), 40 + 65536 * 2 * 4);
    }

    /// Manual bring-up check for the REAL `SharedMemSink` against the installed
    /// "macrdp Microphone" plug-in, WITHOUT an RDP client. Feeds a 330 Hz tone
    /// (distinct from the plug-in's 440 Hz fallback and P2b-0's 220 Hz writer) in
    /// real-time; run it, then record the device and confirm 330 Hz:
    ///   cargo test -p macrdp feed_synthetic_tone -- --ignored --nocapture &
    ///   ffmpeg -f avfoundation -i ":<idx>" -t 3 /tmp/out.wav
    #[test]
    #[ignore = "manual: feeds the shared ring in real-time for external recording"]
    fn feed_synthetic_tone() {
        use ironrdp_rdpsnd::pdu::WaveFormat;
        use std::time::{Duration, Instant};

        let mut sink = SharedMemSink::new().expect("map shared ring");
        sink.on_format(&AudioFormat {
            format: WaveFormat::PCM,
            n_channels: 1,
            n_samples_per_sec: RING_RATE,
            n_avg_bytes_per_sec: RING_RATE * 2,
            n_block_align: 2,
            bits_per_sample: 16,
            data: None,
        });

        let rate = RING_RATE as f64;
        let two_pi_f = 2.0 * std::f64::consts::PI * 330.0 / rate;
        let start = Instant::now();
        let mut written: u64 = 0;
        let total_secs = 15.0;
        while start.elapsed().as_secs_f64() < total_secs {
            let target = (start.elapsed().as_secs_f64() * rate) as u64;
            if target > written {
                let n = (target - written) as usize;
                let mut buf = Vec::with_capacity(n * 2); // mono i16 LE
                for i in 0..n {
                    let s =
                        (0.1 * ((two_pi_f * (written + i as u64) as f64).sin()) * 32767.0) as i16;
                    buf.extend_from_slice(&s.to_le_bytes());
                }
                sink.on_data(&buf);
                written = target;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        println!("feed_synthetic_tone: wrote {written} frames of 330 Hz");
    }
}
