// micfeed_test.c — P2b-0 bring-up tool: stand in for macrdp and feed the
// "macrdp Microphone" ring a real-time 220 Hz tone, to prove the CoreAudio HAL
// plug-in (living inside coreaudiod, a different user) can read shared memory
// written by an external process.
//
// The proof is observable through the audio, not a log: record the device and
// check the pitch — 220 Hz means this feed reached the plug-in; the plug-in's
// 440 Hz fallback tone means the cross-process/cross-user shared memory is
// blocked (coreaudiod sandbox), which would send P2b to a different IPC.
//
//   clang -O2 -o /tmp/micfeed_test audioplugin/micfeed_test.c
//   /tmp/micfeed_test 20        # feed for 20 seconds
//
// This file is throwaway scaffolding; the real writer is macrdp's Rust
// SharedMemSink (P2b-1), which mirrors macrdp_mic_ring.h.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <stdatomic.h>
#include <fcntl.h>
#include <unistd.h>
#include <time.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <mach/mach_time.h>

#include "macrdp_mic_ring.h"

#define FEED_HZ 220.0
#define FEED_AMPL 0.1f

int main(int argc, char** argv) {
    double seconds = (argc > 1) ? atof(argv[1]) : 20.0;

    int fd = shm_open(MACRDP_MIC_SHM_NAME, O_RDWR | O_CREAT, 0666);
    if (fd < 0) { perror("shm_open"); return 1; }
    // 0666 modes are masked by the umask on creation — force them so coreaudiod
    // (a different user) can open the segment.
    fchmod(fd, 0666);
    if (ftruncate(fd, sizeof(MacrdpMicRing)) != 0) { perror("ftruncate"); return 1; }

    MacrdpMicRing* ring = mmap(NULL, sizeof(MacrdpMicRing), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (ring == MAP_FAILED) { perror("mmap"); return 1; }

    // Initialize the header, publishing magic LAST so a reader that sees the
    // magic knows the rest of the header is valid.
    memset(ring, 0, sizeof(*ring));
    ring->version = MACRDP_MIC_VERSION;
    ring->sample_rate = 44100;
    ring->channels = MACRDP_MIC_CHANNELS;
    ring->ring_frames = MACRDP_MIC_RING_FRAMES;
    atomic_store_explicit(&ring->write_frames, 0, memory_order_relaxed);
    atomic_store_explicit(&ring->read_frames, 0, memory_order_relaxed);
    atomic_store_explicit((_Atomic uint32_t*)&ring->magic, MACRDP_MIC_MAGIC, memory_order_release);

    printf("micfeed_test: feeding %.0f Hz @ %.2f into %s for %.0f s (rate=%u ch=%u ring=%u)\n",
           FEED_HZ, FEED_AMPL, MACRDP_MIC_SHM_NAME, seconds,
           ring->sample_rate, ring->channels, ring->ring_frames);

    mach_timebase_info_data_t tb; mach_timebase_info(&tb);
    uint64_t start = mach_absolute_time();
    const uint32_t cap = ring->ring_frames;
    const uint32_t ch = ring->channels;
    const double rate = (double)ring->sample_rate;
    const double twoPiFOverFs = 2.0 * M_PI * FEED_HZ / rate;
    uint64_t written = 0;

    while (1) {
        uint64_t now = mach_absolute_time();
        double elapsed = (double)(now - start) * (double)tb.numer / (double)tb.denom / 1.0e9;
        if (elapsed >= seconds) break;

        uint64_t target = (uint64_t)(elapsed * rate);   // frames that "should" exist by now
        while (written < target) {
            uint64_t idx = written & (uint64_t)(cap - 1);
            float s = (float)(FEED_AMPL * sin(twoPiFOverFs * (double)written));
            for (uint32_t c = 0; c < ch; c++) {
                ring->samples[idx * ch + c] = s;
            }
            written++;
        }
        atomic_store_explicit(&ring->write_frames, written, memory_order_release);

        struct timespec ts = { .tv_sec = 0, .tv_nsec = 5 * 1000 * 1000 }; // 5 ms
        nanosleep(&ts, NULL);
    }

    printf("micfeed_test: done, wrote %llu frames\n", (unsigned long long)written);
    munmap(ring, sizeof(MacrdpMicRing));
    close(fd);
    // Leave the segment around (shm_unlink omitted) so a reader mid-read isn't
    // yanked; the OS reclaims it on reboot, or `rm /dev/shm`-equivalent isn't
    // needed on macOS — use `shmutil`? just re-run to reuse.
    return 0;
}
