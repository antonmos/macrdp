// micfeed_test.c — bring-up tool: stand in for macrdp and feed the "macrdp
// Microphone" ring a real-time 220 Hz tone, to check the CoreAudio HAL plug-in
// (inside coreaudiod, a different user) reads the feed without an RDP client.
//
// The proof is observable through the audio, not a log: record the device and
// check the pitch — 220 Hz means this feed reached the plug-in; silence means it
// didn't.
//
//   clang -O2 -o /tmp/micfeed_test audioplugin/micfeed_test.c
//   /tmp/micfeed_test 20        # feed for 20 seconds
//
// Follows the same version-2 protocol as macrdp's SharedMemSink (see
// macrdp_mic_ring.h): an exclusive, 0644 segment with a random session id,
// wiped and unlinked on exit. Don't run it while macrdp is streaming a mic — it
// replaces macrdp's segment, and the plug-in follows whichever is newest.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <errno.h>
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

    // Replace any existing segment, then create ours exclusively. macOS applies
    // shm_open's mode as given (no umask) and doesn't support fchmod on shm.
    if (shm_unlink(MACRDP_MIC_SHM_NAME) != 0 && errno != ENOENT) {
        perror("shm_unlink (existing segment)");
        return 1;
    }
    int fd = shm_open(MACRDP_MIC_SHM_NAME, O_RDWR | O_CREAT | O_EXCL, 0644);
    if (fd < 0) { perror("shm_open"); return 1; }
    // A fresh segment: the one ftruncate macOS allows.
    if (ftruncate(fd, sizeof(MacrdpMicRing)) != 0) { perror("ftruncate"); return 1; }

    MacrdpMicRing* ring = mmap(NULL, sizeof(MacrdpMicRing), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (ring == MAP_FAILED) { perror("mmap"); return 1; }

    // A fresh segment is zero-filled. Write the header, then publish the magic
    // LAST so a reader that sees it knows the rest is valid.
    uint64_t session_id = 0;
    while (session_id == 0) {
        arc4random_buf(&session_id, sizeof(session_id));
    }
    ring->version = MACRDP_MIC_VERSION;
    ring->sample_rate = MACRDP_MIC_SAMPLE_RATE;
    ring->channels = MACRDP_MIC_CHANNELS;
    ring->ring_frames = MACRDP_MIC_RING_FRAMES;
    ring->session_id = session_id;
    atomic_store_explicit((_Atomic uint32_t*)&ring->magic, MACRDP_MIC_MAGIC, memory_order_release);

    printf("micfeed_test: feeding %.0f Hz @ %.2f into %s for %.0f s\n",
           FEED_HZ, FEED_AMPL, MACRDP_MIC_SHM_NAME, seconds);

    mach_timebase_info_data_t tb; mach_timebase_info(&tb);
    uint64_t start = mach_absolute_time();
    const uint64_t cap = MACRDP_MIC_RING_FRAMES;
    const double rate = (double)MACRDP_MIC_SAMPLE_RATE;
    const double twoPiFOverFs = 2.0 * M_PI * FEED_HZ / rate;
    uint64_t written = 0;

    while (1) {
        uint64_t now = mach_absolute_time();
        double elapsed = (double)(now - start) * (double)tb.numer / (double)tb.denom / 1.0e9;
        if (elapsed >= seconds) break;

        uint64_t target = (uint64_t)(elapsed * rate);   // frames that "should" exist by now
        while (written < target) {
            uint64_t idx = written & (cap - 1);
            float s = (float)(FEED_AMPL * sin(twoPiFOverFs * (double)written));
            for (uint32_t c = 0; c < MACRDP_MIC_CHANNELS; c++) {
                ring->samples[idx * MACRDP_MIC_CHANNELS + c] = s;
            }
            written++;
        }
        atomic_store_explicit(&ring->write_frames, written, memory_order_release);

        struct timespec ts = { .tv_sec = 0, .tv_nsec = 5 * 1000 * 1000 }; // 5 ms
        nanosleep(&ts, NULL);
    }

    printf("micfeed_test: done, wrote %llu frames\n", (unsigned long long)written);
    // End the feed like macrdp does: clear the magic, wipe, unlink.
    atomic_store_explicit((_Atomic uint32_t*)&ring->magic, 0, memory_order_release);
    memset(ring->samples, 0, sizeof(ring->samples));
    shm_unlink(MACRDP_MIC_SHM_NAME);
    munmap(ring, sizeof(MacrdpMicRing));
    close(fd);
    return 0;
}
