// macrdp_mic_ring.h — the shared-memory ring layout for feeding "macrdp
// Microphone" from macrdp (a separate process) into the CoreAudio HAL plug-in
// that lives inside coreaudiod.
//
// This is the audio analogue of the camera path's IOSurface handoff: macrdp
// receives the client's mic PCM over MS-RDPEAI and writes it here; the plug-in's
// DoIOOperation reads it out on the audio clock. macrdp is the sole writer, the
// plug-in the sole reader.
//
// Ownership and permissions (version 2). The segment is a POSIX shared-memory
// object (shm_open) named below. macrdp — running as the logged-in user —
// creates it exclusively (unlinking any stale one first), mode 0644, and
// unlinks it again when the session ends. The plug-in runs inside coreaudiod as
// the _coreaudiod role account, so it can only get in through the "other"
// permission bits: it opens the segment READ-ONLY and keeps its read position in
// private memory, which is what lets the segment be 0644 rather than 0666 — no
// other account can write (inject) audio. POSIX shm offers no finer grant than
// owner/group/other, so other local accounts can still READ it while a session is
// live; see docs/macos-gotchas.md.
//
// Keep the Rust writer (src/audin/shm_sink.rs) in sync. Its unit tests parse the
// #defines below and pin the struct offsets, and the _Static_asserts at the end
// pin them on this side, so a change on one side alone fails a build.

#ifndef MACRDP_MIC_RING_H
#define MACRDP_MIC_RING_H

#include <stddef.h>
#include <stdint.h>
#include <stdatomic.h>

// POSIX shm name (<= 31 chars incl. the leading slash, per macOS).
#define MACRDP_MIC_SHM_NAME "/macrdp_mic_ring"

// 'MRDP' — header sanity so a reader never interprets an uninitialized / foreign
// mapping as audio. The writer clears it to 0 when its session ends, which tells
// the reader the feed is over.
#define MACRDP_MIC_MAGIC 0x4d524450u
// 2: read-only reader, private read cursor, exclusive 0644 segment.
#define MACRDP_MIC_VERSION 2u

// Interleaved Float32 at the device rate; matches the device stream format. A
// mono source is upmixed to stereo on the writer side.
#define MACRDP_MIC_SAMPLE_RATE 44100u
#define MACRDP_MIC_CHANNELS 2u
// Ring capacity in frames — power of two so index = pos & (frames - 1).
// 65536 frames ≈ 1.49 s at 44.1 kHz: ~512 KiB of samples.
#define MACRDP_MIC_RING_FRAMES 65536u

// The whole shared object. `write_frames` is published by the writer with
// release and read by the reader with acquire. The rest of the header is written
// once at creation, before the magic is published. The reader never trusts the
// header's sizes for indexing — it validates them against the #defines above and
// then indexes with the compile-time constants.
typedef struct {
    uint32_t magic;         // MACRDP_MIC_MAGIC while a session is live, else 0
    uint32_t version;       // MACRDP_MIC_VERSION
    uint32_t sample_rate;   // MACRDP_MIC_SAMPLE_RATE
    uint32_t channels;      // MACRDP_MIC_CHANNELS
    uint32_t ring_frames;   // MACRDP_MIC_RING_FRAMES
    uint32_t reserved;
    _Atomic uint64_t write_frames;  // total frames the writer has produced
    // Random, nonzero, fixed per segment. Identifies which segment the name
    // currently refers to — fstat() reports st_ino 0 for every POSIX shm object
    // on macOS, so the inode can't. (Was the shared read cursor in version 1.)
    uint64_t session_id;
    // Interleaved Float32 samples, ring_frames * channels of them.
    float samples[MACRDP_MIC_RING_FRAMES * MACRDP_MIC_CHANNELS];
} MacrdpMicRing;

_Static_assert(offsetof(MacrdpMicRing, magic) == 0, "ring layout");
_Static_assert(offsetof(MacrdpMicRing, version) == 4, "ring layout");
_Static_assert(offsetof(MacrdpMicRing, sample_rate) == 8, "ring layout");
_Static_assert(offsetof(MacrdpMicRing, channels) == 12, "ring layout");
_Static_assert(offsetof(MacrdpMicRing, ring_frames) == 16, "ring layout");
_Static_assert(offsetof(MacrdpMicRing, reserved) == 20, "ring layout");
_Static_assert(offsetof(MacrdpMicRing, write_frames) == 24, "ring layout");
_Static_assert(offsetof(MacrdpMicRing, session_id) == 32, "ring layout");
_Static_assert(offsetof(MacrdpMicRing, samples) == 40, "ring layout");
_Static_assert(sizeof(MacrdpMicRing) == 40 + MACRDP_MIC_RING_FRAMES * MACRDP_MIC_CHANNELS * 4,
               "ring layout");
_Static_assert((MACRDP_MIC_RING_FRAMES & (MACRDP_MIC_RING_FRAMES - 1)) == 0,
               "ring capacity must be a power of two");

#endif // MACRDP_MIC_RING_H
