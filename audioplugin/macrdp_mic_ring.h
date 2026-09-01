// macrdp_mic_ring.h — the shared-memory ring layout for feeding "macrdp
// Microphone" from macrdp (a separate process) into the CoreAudio HAL plug-in
// that lives inside coreaudiod.
//
// This is the audio analogue of the camera path's IOSurface handoff: macrdp
// receives the client's mic PCM over MS-RDPEAI and writes it here; the plug-in's
// DoIOOperation reads it out on the audio clock. A single-producer /
// single-consumer ring — macrdp is the sole writer, the plug-in the sole reader.
//
// The segment is a POSIX shared-memory object (shm_open) named below, created
// 0666 so it works across the user boundary (macrdp runs as the logged-in user;
// coreaudiod runs as the _coreaudiod role account). Both sides open it O_RDWR |
// O_CREAT and mmap MAP_SHARED; whichever touches it first initializes the header.
//
// The Rust writer side (P2b) mirrors this layout exactly — keep the two in sync.

#ifndef MACRDP_MIC_RING_H
#define MACRDP_MIC_RING_H

#include <stdint.h>
#include <stdatomic.h>

// POSIX shm name (<= 31 chars incl. the leading slash, per macOS).
#define MACRDP_MIC_SHM_NAME "/macrdp_mic_ring"

// 'MRDP' — header sanity so a reader never interprets an uninitialized / foreign
// mapping as audio.
#define MACRDP_MIC_MAGIC 0x4d524450u
#define MACRDP_MIC_VERSION 1u

// Interleaved Float32; matches the device stream format. A mono source is
// upmixed to stereo on the writer side.
#define MACRDP_MIC_CHANNELS 2u
// Ring capacity in frames — power of two so index = pos & (frames - 1).
// 65536 frames ≈ 1.49 s at 44.1 kHz: ~512 KiB of samples, ample slack for the
// mic↔audio clock drift before P2c's resampler lands.
#define MACRDP_MIC_RING_FRAMES 65536u

// The whole shared object. `write_frames` is owned by the writer (macrdp),
// `read_frames` by the reader (the plug-in); each is published with release and
// read with acquire so the peer sees a consistent count. Everything else in the
// header is written once at init and then read-only.
typedef struct {
    uint32_t magic;         // MACRDP_MIC_MAGIC once initialized
    uint32_t version;       // MACRDP_MIC_VERSION
    uint32_t sample_rate;   // frames/sec of the samples in the ring (device rate)
    uint32_t channels;      // interleaved channel count (MACRDP_MIC_CHANNELS)
    uint32_t ring_frames;   // capacity (MACRDP_MIC_RING_FRAMES)
    uint32_t reserved;
    _Atomic uint64_t write_frames;  // total frames the writer has produced
    _Atomic uint64_t read_frames;   // total frames the reader has consumed
    // Interleaved Float32 samples, ring_frames * channels of them.
    float samples[MACRDP_MIC_RING_FRAMES * MACRDP_MIC_CHANNELS];
} MacrdpMicRing;

#endif // MACRDP_MIC_RING_H
