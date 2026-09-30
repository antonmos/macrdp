//! Mic feed (`SharedMemSink`): write the received MS-RDPEAI PCM into the POSIX
//! shared-memory ring that the "macrdp Microphone" CoreAudio HAL plug-in
//! (`audioplugin/macrdp_mic.c`) reads in `DoIOOperation`. macrdp is the sole
//! writer, the plug-in (inside `coreaudiod`, as the `_coreaudiod` role account)
//! the sole reader.
//!
//! **This layout MUST stay byte-identical to `audioplugin/macrdp_mic_ring.h`.**
//! The tests below parse that header's `#define`s and pin the struct offsets; the
//! header pins the same offsets with `_Static_assert`.
//!
//! Security model (ring version 2). The segment is created **only once the client
//! has negotiated the mic** — a DVC can't open before the session is fully
//! connected, so an unauthenticated connection never creates it — **exclusively**
//! (any stale segment is unlinked first, and a segment that can't be replaced is
//! refused rather than reused), **mode 0644**, and it is **wiped and unlinked**
//! when the session ends. The plug-in opens it read-only and keeps its read
//! position privately, so no other account can write (inject) audio. Other local
//! accounts can still read it while a session is live — POSIX shm has no finer
//! grant than owner/group/other, and the reader is a different user; see
//! `docs/macos-gotchas.md`.
//!
//! Converts 16-bit PCM (mono → upmixed stereo, or stereo) at the ring rate
//! (44.1 kHz). A client streaming another rate plays at the wrong pitch — the
//! protocol layer must only accept 44.1 kHz until a resampler lands.

use std::ffi::{CStr, CString};
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
/// Must equal `MACRDP_MIC_VERSION`.
const VERSION: u32 = 2;
/// The rate the plug-in advertises + consumes at — must equal `MACRDP_MIC_SAMPLE_RATE`.
const RING_RATE: u32 = 44100;
/// Interleaved channel count in the ring — must equal `MACRDP_MIC_CHANNELS`.
const RING_CHANNELS: u32 = 2;
/// Ring capacity in frames (power of two) — must equal `MACRDP_MIC_RING_FRAMES`.
const RING_FRAMES: u32 = 65536;
const RING_SAMPLES: usize = (RING_FRAMES * RING_CHANNELS) as usize;
/// Owner read/write, everyone else read-only: the plug-in (another user) reads,
/// nobody but the owner writes.
const SEGMENT_MODE: libc::mode_t = 0o644;

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
    session_id: u64,
    samples: [f32; RING_SAMPLES],
}

/// A mapped segment this sink created. Dropping it wipes the audio, clears the
/// magic (the reader's "feed over" signal), and unlinks the name — but only if
/// the name still refers to *this* segment: a newer session may have replaced it.
/// Segments are told apart by `session_id`, not the inode: `fstat` reports
/// `st_ino` 0 for every POSIX shm object on macOS.
struct Segment {
    name: CString,
    ring: *mut MicRing,
    fd: i32,
    session_id: u64,
}

/// A random, nonzero segment identifier.
fn new_session_id() -> u64 {
    loop {
        let mut id = 0u64;
        // SAFETY: fills exactly the 8 bytes of `id`.
        unsafe { libc::arc4random_buf((&mut id as *mut u64).cast(), 8) };
        if id != 0 {
            return id;
        }
    }
}

/// The `session_id` of the segment `name` currently refers to, if it exists and
/// is large enough to hold a header.
fn session_id_of(name: &CStr) -> Option<u64> {
    let len = std::mem::size_of::<MicRing>();
    // SAFETY: a read-only mapping, read once and unmapped before return.
    unsafe {
        let fd = libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0);
        if fd < 0 {
            return None;
        }
        let mut st: libc::stat = std::mem::zeroed();
        let big_enough = libc::fstat(fd, &mut st) == 0 && st.st_size as usize >= len;
        let p = if big_enough {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        } else {
            libc::MAP_FAILED
        };
        libc::close(fd);
        if p == libc::MAP_FAILED {
            return None;
        }
        let id = std::ptr::addr_of!((*(p as *const MicRing)).session_id).read_volatile();
        libc::munmap(p, len);
        Some(id)
    }
}

impl Segment {
    /// Create the segment exclusively, size it, map it, and publish the header.
    /// `None` on any failure, logged — a mic that can't set up its feed must not
    /// kill the session.
    fn create(name: &CStr) -> Option<Self> {
        let size = std::mem::size_of::<MicRing>();
        // SAFETY: POSIX shm bring-up; every return path is checked and every
        // resource acquired so far is released on failure.
        unsafe {
            // Remove a stale segment first (a previous session that crashed, or
            // one this process is replacing), so ours is always freshly created
            // and never one another account set up in advance.
            if libc::shm_unlink(name.as_ptr()) != 0 && errno() != libc::ENOENT {
                warn!(
                    errno = errno(),
                    "mic feed: an existing shared ring could not be removed (owned by another \
                     account?) — not feeding the mic"
                );
                return None;
            }
            let fd = libc::shm_open(
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                libc::c_uint::from(SEGMENT_MODE),
            );
            if fd < 0 {
                warn!(
                    errno = errno(),
                    "mic feed: could not create the shared ring exclusively — not feeding the mic"
                );
                return None;
            }
            let fail = |what: &str| {
                warn!(
                    errno = errno(),
                    "mic feed: {what} failed — not feeding the mic"
                );
                libc::close(fd);
                libc::shm_unlink(name.as_ptr());
            };
            // macOS applies shm_open's mode as given — no umask — and doesn't
            // support fchmod on shm objects (EINVAL), so the mode can't be set
            // afterwards; verify it instead, along with the owner.
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut st) != 0 {
                fail("fstat");
                return None;
            }
            if st.st_uid != libc::geteuid() || st.st_mode & 0o777 != SEGMENT_MODE {
                warn!(
                    uid = st.st_uid,
                    mode = format_args!("{:o}", st.st_mode & 0o777),
                    "mic feed: the new shared ring has an unexpected owner or mode — not feeding the mic"
                );
                libc::close(fd);
                libc::shm_unlink(name.as_ptr());
                return None;
            }
            // A freshly created segment is 0-length, and macOS allows exactly one
            // ftruncate on it — which is this one.
            if libc::ftruncate(fd, size as libc::off_t) != 0 {
                fail("ftruncate");
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
                fail("mmap");
                return None;
            }
            let ring = p as *mut MicRing;
            // A new segment is zero-filled; write the header, then publish the
            // magic LAST so a reader that sees it (acquire) sees the header.
            addr_of_mut!((*ring).version).write(VERSION);
            addr_of_mut!((*ring).sample_rate).write(RING_RATE);
            addr_of_mut!((*ring).channels).write(RING_CHANNELS);
            addr_of_mut!((*ring).ring_frames).write(RING_FRAMES);
            let session_id = new_session_id();
            addr_of_mut!((*ring).session_id).write(session_id);
            fence(Ordering::Release);
            (*ring).magic.store(MAGIC, Ordering::Release);

            info!(
                mode = format_args!("{SEGMENT_MODE:o}"),
                "mic feed: shared ring created (Float32 stereo {RING_RATE} Hz, {RING_FRAMES} frames)"
            );
            Some(Self {
                name: name.to_owned(),
                ring,
                fd,
                session_id,
            })
        }
    }

    /// Whether the shm name still refers to this segment.
    fn name_is_ours(&self) -> bool {
        session_id_of(&self.name) == Some(self.session_id)
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: `ring` is our live mapping until the munmap below.
        unsafe {
            // Wipe first: the reader may keep this mapping briefly, and the audio
            // must not outlive the session.
            (*self.ring).magic.store(0, Ordering::Release);
            std::ptr::write_bytes(
                addr_of_mut!((*self.ring).samples) as *mut f32,
                0,
                RING_SAMPLES,
            );
            if self.name_is_ours() {
                libc::shm_unlink(self.name.as_ptr());
            }
            libc::munmap(self.ring as *mut c_void, std::mem::size_of::<MicRing>());
            libc::close(self.fd);
        }
        info!("mic feed: shared ring wiped and released");
    }
}

/// Writes received mic PCM into the shared ring. The segment is created lazily,
/// on the first negotiated format, and released when the sink drops (session end).
pub struct SharedMemSink {
    name: CString,
    segment: Option<Segment>,
    src_channels: u16,
    src_bits: u16,
    src_rate: u32,
    logged_unsupported: bool,
}

// The raw pointer inside `Segment` is only ever touched from the single audin
// processing task (the trait is `Send`, called serially). Ordering with the reader
// is via the atomics. Asserting Send lets the sink live behind the `Box<dyn …>` seam.
unsafe impl Send for SharedMemSink {}

impl SharedMemSink {
    /// A sink that creates nothing until the client negotiates a format.
    pub fn new() -> Self {
        Self::with_name(SHM_NAME)
    }

    fn with_name(name: &CStr) -> Self {
        Self {
            name: name.to_owned(),
            segment: None,
            src_channels: 1,
            src_bits: 16,
            src_rate: RING_RATE,
            logged_unsupported: false,
        }
    }
}

impl AudinSampleSink for SharedMemSink {
    fn on_format(&mut self, format: &AudioFormat) {
        self.src_channels = format.n_channels.max(1);
        self.src_bits = format.bits_per_sample;
        self.src_rate = format.n_samples_per_sec;
        self.logged_unsupported = false;
        if self.segment.is_none() {
            self.segment = Segment::create(&self.name);
        }
        if self.src_rate != RING_RATE {
            warn!(
                src_rate = self.src_rate,
                ring_rate = RING_RATE,
                "mic feed: client rate != device rate — the mic will play at the wrong pitch"
            );
        }
        info!(
            channels = self.src_channels,
            rate = self.src_rate,
            bits = self.src_bits,
            feeding = self.segment.is_some(),
            "mic feed: negotiated capture format"
        );
    }

    fn on_data(&mut self, data: &[u8]) {
        let Some(segment) = &self.segment else {
            return;
        };
        // 16-bit PCM only. Other depths need a converter — log once.
        if self.src_bits != 16 {
            if !self.logged_unsupported {
                warn!(
                    bits = self.src_bits,
                    "mic feed: only 16-bit PCM is handled — dropping"
                );
                self.logged_unsupported = true;
            }
            return;
        }
        let src_ch = self.src_channels as usize;
        let bytes_per_frame = 2 * src_ch;
        let frames = data.len() / bytes_per_frame;
        if frames == 0 {
            return;
        }

        let ring = segment.ring;
        // SAFETY: `ring` is a live MAP_SHARED mapping for the segment's lifetime;
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

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_rdpsnd::pdu::WaveFormat;

    const HEADER: &str = include_str!("../../audioplugin/macrdp_mic_ring.h");

    /// The value of `#define NAME value` in the C header, with any `u` suffix
    /// and quotes stripped.
    fn define(name: &str) -> String {
        HEADER
            .lines()
            .find_map(|line| {
                let rest = line.strip_prefix("#define ")?.strip_prefix(name)?;
                rest.starts_with(char::is_whitespace).then(|| {
                    rest.trim()
                        .trim_end_matches('u')
                        .trim_matches('"')
                        .to_owned()
                })
            })
            .unwrap_or_else(|| panic!("#define {name} not found in macrdp_mic_ring.h"))
    }

    fn define_u32(name: &str) -> u32 {
        let v = define(name);
        match v.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).unwrap(),
            None => v.parse().unwrap(),
        }
    }

    /// The Rust constants must equal the C header's — both processes interpret
    /// the same bytes. Parsed from the header itself, so editing one side alone
    /// fails this test.
    #[test]
    fn constants_match_c_header() {
        assert_eq!(define("MACRDP_MIC_SHM_NAME"), SHM_NAME.to_str().unwrap());
        assert_eq!(define_u32("MACRDP_MIC_MAGIC"), MAGIC);
        assert_eq!(define_u32("MACRDP_MIC_VERSION"), VERSION);
        assert_eq!(define_u32("MACRDP_MIC_SAMPLE_RATE"), RING_RATE);
        assert_eq!(define_u32("MACRDP_MIC_CHANNELS"), RING_CHANNELS);
        assert_eq!(define_u32("MACRDP_MIC_RING_FRAMES"), RING_FRAMES);
    }

    /// The struct layout — the header `_Static_assert`s the same offsets.
    #[test]
    fn layout_matches_c_header() {
        assert_eq!(std::mem::offset_of!(MicRing, magic), 0);
        assert_eq!(std::mem::offset_of!(MicRing, version), 4);
        assert_eq!(std::mem::offset_of!(MicRing, sample_rate), 8);
        assert_eq!(std::mem::offset_of!(MicRing, channels), 12);
        assert_eq!(std::mem::offset_of!(MicRing, ring_frames), 16);
        assert_eq!(std::mem::offset_of!(MicRing, reserved), 20);
        assert_eq!(std::mem::offset_of!(MicRing, write_frames), 24);
        assert_eq!(std::mem::offset_of!(MicRing, session_id), 32);
        assert_eq!(std::mem::offset_of!(MicRing, samples), 40);
        assert_eq!(std::mem::size_of::<MicRing>(), 40 + RING_SAMPLES * 4);
    }

    /// A per-test shm name (≤ 31 chars on macOS), so tests never touch the real
    /// ring a running macrdp or the installed plug-in may be using.
    fn test_name(tag: &str) -> CString {
        CString::new(format!("/mrdp_t_{}_{tag}", std::process::id())).unwrap()
    }

    fn pcm_mono_44k() -> AudioFormat {
        AudioFormat {
            format: WaveFormat::PCM,
            n_channels: 1,
            n_samples_per_sec: RING_RATE,
            n_avg_bytes_per_sec: RING_RATE * 2,
            n_block_align: 2,
            bits_per_sample: 16,
            data: None,
        }
    }

    /// `fstat` of the named segment, or `None` if it doesn't exist.
    fn stat_named(name: &CStr) -> Option<libc::stat> {
        unsafe {
            let fd = libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0);
            if fd < 0 {
                return None;
            }
            let mut st: libc::stat = std::mem::zeroed();
            let ok = libc::fstat(fd, &mut st) == 0;
            libc::close(fd);
            ok.then_some(st)
        }
    }

    /// Nothing is created before the client negotiates the mic — in particular,
    /// not for a connection that never gets that far.
    #[test]
    fn nothing_is_created_before_a_format_is_negotiated() {
        let name = test_name("lazy");
        unsafe { libc::shm_unlink(name.as_ptr()) };
        let mut sink = SharedMemSink::with_name(&name);
        sink.on_data(&[0u8; 64]);
        assert!(stat_named(&name).is_none(), "no segment before a format");
        sink.on_format(&pcm_mono_44k());
        assert!(stat_named(&name).is_some(), "created on the first format");
    }

    /// The segment is 0644 (no one but the owner can write), owned by us, and
    /// wiped + unlinked when the session ends.
    #[test]
    fn segment_is_0644_and_removed_at_session_end() {
        let name = test_name("mode");
        let mut sink = SharedMemSink::with_name(&name);
        sink.on_format(&pcm_mono_44k());
        let st = stat_named(&name).expect("segment exists");
        assert_eq!(st.st_mode & 0o777, 0o644);
        assert_eq!(st.st_uid, unsafe { libc::geteuid() });
        drop(sink);
        assert!(stat_named(&name).is_none(), "unlinked at session end");
    }

    /// A stale segment (a crashed session's, or one set up in advance) is
    /// replaced by a fresh one, never reused.
    #[test]
    fn a_stale_segment_is_replaced() {
        let name = test_name("stale");
        unsafe {
            libc::shm_unlink(name.as_ptr());
            let fd = libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o666);
            assert!(fd >= 0);
            libc::close(fd);
        }
        assert_eq!(
            stat_named(&name).unwrap().st_mode & 0o777,
            0o666,
            "the stale one"
        );
        let mut sink = SharedMemSink::with_name(&name);
        sink.on_format(&pcm_mono_44k());
        let st = stat_named(&name).expect("replaced");
        assert_eq!(
            st.st_mode & 0o777,
            0o644,
            "a new segment, not the stale one"
        );
        assert_eq!(
            session_id_of(&name),
            sink.segment.as_ref().map(|s| s.session_id),
            "the name refers to this sink's segment"
        );
    }

    /// When a new session replaces the segment (a takeover), the old session
    /// ending must not unlink the new session's segment.
    #[test]
    fn an_ending_session_leaves_its_successors_segment() {
        let name = test_name("succ");
        let mut old = SharedMemSink::with_name(&name);
        old.on_format(&pcm_mono_44k());
        let mut new = SharedMemSink::with_name(&name);
        new.on_format(&pcm_mono_44k());
        let new_id = new.segment.as_ref().unwrap().session_id;
        assert_ne!(old.segment.as_ref().unwrap().session_id, new_id);
        drop(old);
        assert_eq!(
            session_id_of(&name),
            Some(new_id),
            "the successor's segment survives"
        );
        drop(new);
        assert!(stat_named(&name).is_none());
    }

    /// The written audio lands in the ring, and the old session's audio is wiped
    /// when it ends even though a reader may still have it mapped.
    #[test]
    fn audio_is_written_then_wiped() {
        let name = test_name("wipe");
        let mut sink = SharedMemSink::with_name(&name);
        sink.on_format(&pcm_mono_44k());
        let sample: i16 = 16384; // 0.5
        let pcm: Vec<u8> = std::iter::repeat_n(sample.to_le_bytes(), 4)
            .flatten()
            .collect();
        sink.on_data(&pcm);

        // Map it as the reader would (read-only), and keep the mapping past the drop.
        let reader = unsafe {
            let fd = libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0);
            assert!(fd >= 0);
            let p = libc::mmap(
                std::ptr::null_mut(),
                std::mem::size_of::<MicRing>(),
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            );
            libc::close(fd);
            assert_ne!(p, libc::MAP_FAILED);
            p as *const MicRing
        };
        unsafe {
            assert_eq!((*reader).magic.load(Ordering::Acquire), MAGIC);
            assert_eq!((*reader).version, VERSION);
            assert_ne!((*reader).session_id, 0);
            assert_eq!((*reader).write_frames.load(Ordering::Acquire), 4);
            assert_eq!((*reader).samples[0], 0.5);
            assert_eq!((*reader).samples[1], 0.5, "mono is upmixed");
        }
        drop(sink);
        unsafe {
            assert_eq!(
                (*reader).magic.load(Ordering::Acquire),
                0,
                "feed-over signal"
            );
            assert_eq!((*reader).samples[0], 0.0, "audio wiped");
            libc::munmap(reader as *mut c_void, std::mem::size_of::<MicRing>());
        }
    }

    /// Manual bring-up check for the REAL `SharedMemSink` against the installed
    /// "macrdp Microphone" plug-in, WITHOUT an RDP client. Feeds a 330 Hz tone in
    /// real time; run it, then record the device and confirm 330 Hz:
    ///   cargo test -p macrdp feed_synthetic_tone -- --ignored --nocapture &
    ///   ffmpeg -f avfoundation -i ":<idx>" -t 3 /tmp/out.wav
    #[test]
    #[ignore = "manual: feeds the shared ring in real-time for external recording"]
    fn feed_synthetic_tone() {
        use std::time::{Duration, Instant};

        let mut sink = SharedMemSink::new();
        sink.on_format(&pcm_mono_44k());

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
