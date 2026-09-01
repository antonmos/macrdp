// macrdp_mic.c — "macrdp Microphone": a CoreAudio AudioServerPlugIn HAL plug-in
// presenting a single virtual audio INPUT device.
//
// This is the Phase-2 macOS side of microphone / audio-input redirection
// (MS-RDPEAI): the RDP client redirects its mic to macrdp (the server), and this
// plug-in surfaces that audio to macOS apps (Zoom / FaceTime / QuickTime / Teams)
// as a real input device — the audio analogue of the camera redirection system
// extension. Written from scratch (MIT/Apache); the abundant reference
// implementations (BlackHole, Apple's NullAudio sample) are GPL / sample-code, so
// none of their code is used here.
//
// Loaded by coreaudiod as a CFPlugIn `.driver` bundle in
// /Library/Audio/Plug-Ins/HAL/. It needs NO entitlement (unlike the USB
// host-controller / DriverKit route) — it installs like the IFD handler: a file
// copy into a system dir (one privileged step) plus a coreaudiod restart.
//
// P2a (this file's current behavior): the device plays an internal, low-amplitude
// 440 Hz test tone, so the whole novel path can be verified — the bundle builds
// without Xcode, coreaudiod loads it, and "macrdp Microphone" appears in Audio
// MIDI Setup and app input pickers, delivering the tone — BEFORE any macrdp feed
// is wired. P2b will replace the tone in DoIOOperation with a read from a
// shared-memory ring that macrdp's AudinSampleSink writes the received PCM into.

#include <CoreAudio/AudioServerPlugIn.h>
#include <CoreFoundation/CoreFoundation.h>
#include <mach/mach_time.h>
#include <os/log.h>
#include <pthread.h>
#include <string.h>
#include <math.h>
#include <stdatomic.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <sys/mman.h>
#include <sys/stat.h>

#include "macrdp_mic_ring.h"

// ---------------------------------------------------------------------------
// Device shape. One interleaved Float32 input stream. Stereo for the broadest
// app compatibility (a mono source is upmixed on the feed side in P2b). 44100 Hz
// matches the PCM the Phase-0 client streamed; resampling to whatever the client
// actually sends lands in P2c.
// ---------------------------------------------------------------------------
#define kSampleRate        44100.0
#define kChannelsPerFrame  2u
#define kBitsPerChannel    32u
#define kBytesPerFrame     (kChannelsPerFrame * (kBitsPerChannel / 8u))
// Frames between zero timestamps = the virtual ring size (also the shm ring in
// P2b). A power of two keeps the P2b index masking cheap. ~1.49 s at 44100.
#define kRingFrames        65536u

#define kDeviceUID     "macrdpMicrophone_UID"
#define kDeviceName    "macrdp Microphone"
#define kManufacturer  "macrdp"
#define kBoxModelUID   "macrdpMicrophone_Model"

// COM HRESULTs (not defined on macOS outside the Windows headers).
#ifndef S_OK
#define S_OK          ((HRESULT)0x00000000L)
#endif
#ifndef E_NOINTERFACE
#define E_NOINTERFACE ((HRESULT)0x80000004L)
#endif

// Object IDs. The plug-in object's ID is the well-known kAudioObjectPlugInObject;
// our device and stream get fixed small IDs that don't collide with it.
enum {
    kObjectID_PlugIn       = kAudioObjectPlugInObject,
    kObjectID_Device       = 2,
    kObjectID_Stream_Input = 3,
};

// ---------------------------------------------------------------------------
// Plug-in global state. coreaudiod is single-instance and drives IO from one
// real-time thread; the property/control calls come from other threads, so the
// mutable running-state is guarded by a mutex (the IO fill itself only reads
// immutable constants + the anchored clock, so it takes no lock).
// ---------------------------------------------------------------------------
static AudioServerPlugInHostRef gHost = NULL;
static os_log_t gLog = NULL;
static UInt32 gRefCount = 0;

static pthread_mutex_t gStateMutex = PTHREAD_MUTEX_INITIALIZER;
static UInt32 gIOClientsRunning = 0;   // # of StartIO not yet balanced by StopIO
static Boolean gDeviceRunning = false;

// Host-clock anchor for GetZeroTimeStamp (set on the first StartIO).
static UInt64 gAnchorHostTime = 0;
static Float64 gHostTicksPerFrame = 0.0;
static UInt64 gClockSeed = 1;

// Shared-memory feed from macrdp (P2b). Mapped at StartIO (off the real-time IO
// thread), read in DoIOOperation. NULL until macrdp is running and streaming a
// mic — DoIOOperation then falls back to the internal test tone.
static MacrdpMicRing* gRing = NULL;
static int gRingFd = -1;

static void ensure_log(void) {
    if (gLog == NULL) {
        gLog = os_log_create("com.clintcan.macrdp.mic", "plugin");
    }
}

// Map the macrdp mic ring if it exists (the writer creates + sizes + initializes
// it). Reader-only: never creates the segment, never resizes it — so a device
// opened before macrdp is running simply finds nothing and plays the tone. Runs
// on StartIO, i.e. NOT the real-time IO thread, so the blocking syscalls are ok.
static void ring_map(void) {
    if (gRing != NULL) {
        return;
    }
    int fd = shm_open(MACRDP_MIC_SHM_NAME, O_RDWR, 0666);
    if (fd < 0) {
        os_log(gLog, "macrdp-mic: no feed ring (%{public}s: %d) — using test tone",
               MACRDP_MIC_SHM_NAME, errno);
        return;
    }
    struct stat st;
    if (fstat(fd, &st) != 0 || (size_t)st.st_size < sizeof(MacrdpMicRing)) {
        os_log(gLog, "macrdp-mic: feed ring too small (%lld < %zu) — test tone",
               (long long)st.st_size, sizeof(MacrdpMicRing));
        close(fd);
        return;
    }
    void* p = mmap(NULL, sizeof(MacrdpMicRing), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) {
        os_log(gLog, "macrdp-mic: feed ring mmap failed (%d) — test tone", errno);
        close(fd);
        return;
    }
    gRingFd = fd;
    gRing = (MacrdpMicRing*)p;
    os_log(gLog, "macrdp-mic: feed ring mapped (magic=0x%x rate=%u ch=%u) — reading mic feed",
           gRing->magic, gRing->sample_rate, gRing->channels);
}

static void ring_unmap(void) {
    if (gRing != NULL) {
        munmap((void*)gRing, sizeof(MacrdpMicRing));
        gRing = NULL;
    }
    if (gRingFd >= 0) {
        close(gRingFd);
        gRingFd = -1;
    }
}

// ===========================================================================
// Property helpers — one small getter per object. Each returns the size it
// would write (for GetPropertyDataSize) and, when outData is non-NULL, writes
// the value (for GetPropertyData). Keeping size + value in one place means the
// two entry points can never disagree about a property's length.
// ===========================================================================

static AudioStreamBasicDescription stream_format(void) {
    AudioStreamBasicDescription f;
    memset(&f, 0, sizeof(f));
    f.mSampleRate = kSampleRate;
    f.mFormatID = kAudioFormatLinearPCM;
    f.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;
    f.mFramesPerPacket = 1;
    f.mChannelsPerFrame = kChannelsPerFrame;
    f.mBitsPerChannel = kBitsPerChannel;
    f.mBytesPerFrame = kBytesPerFrame;
    f.mBytesPerPacket = kBytesPerFrame;
    return f;
}

// ===========================================================================
// IUnknown
// ===========================================================================
static HRESULT MacRDPMic_QueryInterface(void* inDriver, REFIID inUUID, LPVOID* outInterface);
static ULONG MacRDPMic_AddRef(void* inDriver);
static ULONG MacRDPMic_Release(void* inDriver);

// Driver interface method decls.
static OSStatus MacRDPMic_Initialize(AudioServerPlugInDriverRef inDriver, AudioServerPlugInHostRef inHost);
static OSStatus MacRDPMic_CreateDevice(AudioServerPlugInDriverRef inDriver, CFDictionaryRef inDescription, const AudioServerPlugInClientInfo* inClientInfo, AudioObjectID* outDeviceObjectID);
static OSStatus MacRDPMic_DestroyDevice(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID);
static OSStatus MacRDPMic_AddDeviceClient(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, const AudioServerPlugInClientInfo* inClientInfo);
static OSStatus MacRDPMic_RemoveDeviceClient(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, const AudioServerPlugInClientInfo* inClientInfo);
static OSStatus MacRDPMic_PerformDeviceConfigurationChange(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt64 inChangeAction, void* inChangeInfo);
static OSStatus MacRDPMic_AbortDeviceConfigurationChange(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt64 inChangeAction, void* inChangeInfo);
static Boolean MacRDPMic_HasProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress);
static OSStatus MacRDPMic_IsPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable);
static OSStatus MacRDPMic_GetPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize);
static OSStatus MacRDPMic_GetPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData);
static OSStatus MacRDPMic_SetPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData);
static OSStatus MacRDPMic_StartIO(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID);
static OSStatus MacRDPMic_StopIO(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID);
static OSStatus MacRDPMic_GetZeroTimeStamp(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, Float64* outSampleTime, UInt64* outHostTime, UInt64* outSeed);
static OSStatus MacRDPMic_WillDoIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, Boolean* outWillDo, Boolean* outWillDoInPlace);
static OSStatus MacRDPMic_BeginIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo);
static OSStatus MacRDPMic_DoIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, AudioObjectID inStreamObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo, void* ioMainBuffer, void* ioSecondaryBuffer);
static OSStatus MacRDPMic_EndIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo);

// The single interface instance + the ref coreaudiod holds (pointer-to-pointer).
static AudioServerPlugInDriverInterface gInterface = {
    NULL,
    MacRDPMic_QueryInterface,
    MacRDPMic_AddRef,
    MacRDPMic_Release,
    MacRDPMic_Initialize,
    MacRDPMic_CreateDevice,
    MacRDPMic_DestroyDevice,
    MacRDPMic_AddDeviceClient,
    MacRDPMic_RemoveDeviceClient,
    MacRDPMic_PerformDeviceConfigurationChange,
    MacRDPMic_AbortDeviceConfigurationChange,
    MacRDPMic_HasProperty,
    MacRDPMic_IsPropertySettable,
    MacRDPMic_GetPropertyDataSize,
    MacRDPMic_GetPropertyData,
    MacRDPMic_SetPropertyData,
    MacRDPMic_StartIO,
    MacRDPMic_StopIO,
    MacRDPMic_GetZeroTimeStamp,
    MacRDPMic_WillDoIOOperation,
    MacRDPMic_BeginIOOperation,
    MacRDPMic_DoIOOperation,
    MacRDPMic_EndIOOperation,
};
static AudioServerPlugInDriverInterface* gInterfacePtr = &gInterface;
static AudioServerPlugInDriverRef gDriverRef = &gInterfacePtr;

// ---------------------------------------------------------------------------
// CFPlugIn factory — the entry point named in Info.plist's CFPlugInFactories.
// coreaudiod calls it with the requested TYPE UUID; we return our single
// interface if it wants the AudioServerPlugIn type.
// ---------------------------------------------------------------------------
void* MacRDPMic_Create(CFAllocatorRef inAllocator, CFUUIDRef inRequestedTypeUUID);
void* MacRDPMic_Create(CFAllocatorRef inAllocator, CFUUIDRef inRequestedTypeUUID) {
    (void)inAllocator;
    ensure_log();
    if (inRequestedTypeUUID != NULL &&
        CFEqual(inRequestedTypeUUID, kAudioServerPlugInTypeUUID)) {
        os_log(gLog, "macrdp-mic: factory handing out the AudioServerPlugIn interface");
        gRefCount++;
        return gDriverRef;
    }
    return NULL;
}

static HRESULT MacRDPMic_QueryInterface(void* inDriver, REFIID inUUID, LPVOID* outInterface) {
    if (inDriver != gDriverRef || outInterface == NULL) {
        return E_NOINTERFACE;
    }
    CFUUIDRef requested = CFUUIDCreateFromUUIDBytes(NULL, inUUID);
    HRESULT result = E_NOINTERFACE;
    if (requested != NULL &&
        (CFEqual(requested, IUnknownUUID) ||
         CFEqual(requested, kAudioServerPlugInDriverInterfaceUUID))) {
        gRefCount++;
        *outInterface = gDriverRef;
        result = S_OK;
    }
    if (requested != NULL) {
        CFRelease(requested);
    }
    return result;
}

static ULONG MacRDPMic_AddRef(void* inDriver) {
    if (inDriver != gDriverRef) {
        return 0;
    }
    if (gRefCount != UINT32_MAX) {
        gRefCount++;
    }
    return gRefCount;
}

static ULONG MacRDPMic_Release(void* inDriver) {
    if (inDriver != gDriverRef) {
        return 0;
    }
    // The interface is a static singleton — never actually freed; just track.
    if (gRefCount != 0) {
        gRefCount--;
    }
    return gRefCount;
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------
static OSStatus MacRDPMic_Initialize(AudioServerPlugInDriverRef inDriver, AudioServerPlugInHostRef inHost) {
    if (inDriver != gDriverRef) {
        return kAudioHardwareBadObjectError;
    }
    ensure_log();
    gHost = inHost;

    // Precompute host ticks per audio frame for the GetZeroTimeStamp clock.
    mach_timebase_info_data_t tb;
    mach_timebase_info(&tb);
    Float64 nanosPerFrame = 1.0e9 / kSampleRate;
    gHostTicksPerFrame = nanosPerFrame * (Float64)tb.denom / (Float64)tb.numer;

    os_log(gLog, "macrdp-mic: Initialize (device \"%{public}s\", %{public}.0f Hz, %u ch)",
           kDeviceName, kSampleRate, kChannelsPerFrame);
    return noErr;
}

// This plug-in publishes ONE fixed device — it doesn't support dynamic
// create/destroy from a client.
static OSStatus MacRDPMic_CreateDevice(AudioServerPlugInDriverRef inDriver, CFDictionaryRef inDescription, const AudioServerPlugInClientInfo* inClientInfo, AudioObjectID* outDeviceObjectID) {
    (void)inDriver; (void)inDescription; (void)inClientInfo; (void)outDeviceObjectID;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus MacRDPMic_DestroyDevice(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID) {
    (void)inDriver; (void)inDeviceObjectID;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus MacRDPMic_AddDeviceClient(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, const AudioServerPlugInClientInfo* inClientInfo) {
    (void)inDriver; (void)inDeviceObjectID; (void)inClientInfo;
    return noErr;
}

static OSStatus MacRDPMic_RemoveDeviceClient(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, const AudioServerPlugInClientInfo* inClientInfo) {
    (void)inDriver; (void)inDeviceObjectID; (void)inClientInfo;
    return noErr;
}

static OSStatus MacRDPMic_PerformDeviceConfigurationChange(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt64 inChangeAction, void* inChangeInfo) {
    (void)inDriver; (void)inDeviceObjectID; (void)inChangeAction; (void)inChangeInfo;
    return noErr;
}

static OSStatus MacRDPMic_AbortDeviceConfigurationChange(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt64 inChangeAction, void* inChangeInfo) {
    (void)inDriver; (void)inDeviceObjectID; (void)inChangeAction; (void)inChangeInfo;
    return noErr;
}

// ===========================================================================
// Property system. Dispatched by object ID; each object answers the minimum set
// coreaudiod needs to publish a usable input device.
// ===========================================================================

static Boolean plugin_has_property(const AudioObjectPropertyAddress* a) {
    switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        case kAudioObjectPropertyManufacturer:
        case kAudioObjectPropertyOwnedObjects:
        case kAudioPlugInPropertyDeviceList:
        case kAudioPlugInPropertyTranslateUIDToDevice:
        case kAudioPlugInPropertyResourceBundle:
            return true;
        default:
            return false;
    }
}

static Boolean device_has_property(const AudioObjectPropertyAddress* a) {
    switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        case kAudioObjectPropertyName:
        case kAudioObjectPropertyManufacturer:
        case kAudioObjectPropertyOwnedObjects:
        case kAudioObjectPropertyControlList:
        case kAudioDevicePropertyDeviceUID:
        case kAudioDevicePropertyModelUID:
        case kAudioDevicePropertyTransportType:
        case kAudioDevicePropertyRelatedDevices:
        case kAudioDevicePropertyClockDomain:
        case kAudioDevicePropertyDeviceIsAlive:
        case kAudioDevicePropertyDeviceIsRunning:
        case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
        case kAudioDevicePropertyLatency:
        case kAudioDevicePropertyStreams:
        case kAudioDevicePropertySafetyOffset:
        case kAudioDevicePropertyNominalSampleRate:
        case kAudioDevicePropertyAvailableNominalSampleRates:
        case kAudioDevicePropertyIsHidden:
        case kAudioDevicePropertyZeroTimeStampPeriod:
        case kAudioDevicePropertyPreferredChannelsForStereo:
            return true;
        default:
            return false;
    }
}

static Boolean stream_has_property(const AudioObjectPropertyAddress* a) {
    switch (a->mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        case kAudioObjectPropertyOwnedObjects:
        case kAudioStreamPropertyIsActive:
        case kAudioStreamPropertyDirection:
        case kAudioStreamPropertyTerminalType:
        case kAudioStreamPropertyStartingChannel:
        case kAudioStreamPropertyLatency:
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat:
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyAvailablePhysicalFormats:
            return true;
        default:
            return false;
    }
}

static Boolean MacRDPMic_HasProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress) {
    (void)inDriver; (void)inClientProcessID;
    if (inAddress == NULL) {
        return false;
    }
    switch (inObjectID) {
        case kObjectID_PlugIn:       return plugin_has_property(inAddress);
        case kObjectID_Device:       return device_has_property(inAddress);
        case kObjectID_Stream_Input: return stream_has_property(inAddress);
        default:                     return false;
    }
}

static OSStatus MacRDPMic_IsPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable) {
    (void)inDriver; (void)inClientProcessID;
    if (inAddress == NULL || outIsSettable == NULL) {
        return kAudioHardwareIllegalOperationError;
    }
    // Nothing is client-settable in P2a — the format + rate are fixed. Stream
    // "IsActive" could be settable, but we keep the single stream always active.
    if (!MacRDPMic_HasProperty(inDriver, inObjectID, inClientProcessID, inAddress)) {
        return kAudioHardwareUnknownPropertyError;
    }
    *outIsSettable = false;
    return noErr;
}

// Compute the byte size of a property's value.
static OSStatus property_size(AudioObjectID inObjectID, const AudioObjectPropertyAddress* a, UInt32* outSize) {
    switch (inObjectID) {
        case kObjectID_PlugIn:
            switch (a->mSelector) {
                case kAudioObjectPropertyBaseClass:
                case kAudioObjectPropertyClass:
                case kAudioObjectPropertyOwner:
                    *outSize = sizeof(AudioClassID); return noErr; // (Owner is AudioObjectID, same width)
                case kAudioObjectPropertyManufacturer:
                case kAudioPlugInPropertyResourceBundle:
                    *outSize = sizeof(CFStringRef); return noErr;
                case kAudioObjectPropertyOwnedObjects:
                case kAudioPlugInPropertyDeviceList:
                    *outSize = sizeof(AudioObjectID); return noErr; // one device
                case kAudioPlugInPropertyTranslateUIDToDevice:
                    *outSize = sizeof(AudioObjectID); return noErr;
                default: return kAudioHardwareUnknownPropertyError;
            }
        case kObjectID_Device:
            switch (a->mSelector) {
                case kAudioObjectPropertyBaseClass:
                case kAudioObjectPropertyClass:
                case kAudioObjectPropertyOwner:
                case kAudioDevicePropertyTransportType:
                case kAudioDevicePropertyClockDomain:
                case kAudioDevicePropertyDeviceIsAlive:
                case kAudioDevicePropertyDeviceIsRunning:
                case kAudioDevicePropertyDeviceCanBeDefaultDevice:
                case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
                case kAudioDevicePropertyLatency:
                case kAudioDevicePropertySafetyOffset:
                case kAudioDevicePropertyIsHidden:
                case kAudioDevicePropertyZeroTimeStampPeriod:
                    *outSize = sizeof(UInt32); return noErr;
                case kAudioObjectPropertyName:
                case kAudioObjectPropertyManufacturer:
                case kAudioDevicePropertyDeviceUID:
                case kAudioDevicePropertyModelUID:
                    *outSize = sizeof(CFStringRef); return noErr;
                case kAudioObjectPropertyOwnedObjects:
                case kAudioDevicePropertyRelatedDevices:
                    *outSize = sizeof(AudioObjectID); return noErr; // one stream / self
                case kAudioDevicePropertyStreams:
                    // Scope-sensitive: this is an INPUT-only device, so the
                    // output scope has no streams (else it reports phantom
                    // output channels and apps may try to play into it).
                    *outSize = (a->mScope == kAudioObjectPropertyScopeOutput) ? 0 : sizeof(AudioObjectID);
                    return noErr;
                case kAudioObjectPropertyControlList:
                    *outSize = 0; return noErr; // no controls in P2a
                case kAudioDevicePropertyNominalSampleRate:
                    *outSize = sizeof(Float64); return noErr;
                case kAudioDevicePropertyAvailableNominalSampleRates:
                    *outSize = sizeof(AudioValueRange); return noErr; // one range
                case kAudioDevicePropertyPreferredChannelsForStereo:
                    *outSize = 2 * sizeof(UInt32); return noErr;
                default: return kAudioHardwareUnknownPropertyError;
            }
        case kObjectID_Stream_Input:
            switch (a->mSelector) {
                case kAudioObjectPropertyBaseClass:
                case kAudioObjectPropertyClass:
                case kAudioObjectPropertyOwner:
                case kAudioStreamPropertyIsActive:
                case kAudioStreamPropertyDirection:
                case kAudioStreamPropertyTerminalType:
                case kAudioStreamPropertyStartingChannel:
                case kAudioStreamPropertyLatency:
                    *outSize = sizeof(UInt32); return noErr;
                case kAudioObjectPropertyOwnedObjects:
                    *outSize = 0; return noErr;
                case kAudioStreamPropertyVirtualFormat:
                case kAudioStreamPropertyPhysicalFormat:
                    *outSize = sizeof(AudioStreamBasicDescription); return noErr;
                case kAudioStreamPropertyAvailableVirtualFormats:
                case kAudioStreamPropertyAvailablePhysicalFormats:
                    *outSize = sizeof(AudioStreamRangedDescription); return noErr; // one
                default: return kAudioHardwareUnknownPropertyError;
            }
        default:
            return kAudioHardwareBadObjectError;
    }
}

static OSStatus MacRDPMic_GetPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize) {
    (void)inDriver; (void)inClientProcessID; (void)inQualifierDataSize; (void)inQualifierData;
    if (inAddress == NULL || outDataSize == NULL) {
        return kAudioHardwareIllegalOperationError;
    }
    return property_size(inObjectID, inAddress, outDataSize);
}

static OSStatus MacRDPMic_GetPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData) {
    (void)inDriver; (void)inClientProcessID;
    if (inAddress == NULL || outDataSize == NULL || outData == NULL) {
        return kAudioHardwareIllegalOperationError;
    }

    UInt32 need = 0;
    OSStatus st = property_size(inObjectID, inAddress, &need);
    if (st != noErr) {
        return st;
    }
    if (inDataSize < need) {
        return kAudioHardwareBadPropertySizeError;
    }

    switch (inObjectID) {
        case kObjectID_PlugIn:
            switch (inAddress->mSelector) {
                case kAudioObjectPropertyBaseClass: *(AudioClassID*)outData = kAudioObjectClassID; break;
                case kAudioObjectPropertyClass:     *(AudioClassID*)outData = kAudioPlugInClassID; break;
                case kAudioObjectPropertyOwner:     *(AudioObjectID*)outData = kAudioObjectUnknown; break;
                case kAudioObjectPropertyManufacturer:
                    *(CFStringRef*)outData = CFStringCreateWithCString(NULL, kManufacturer, kCFStringEncodingUTF8); break;
                case kAudioPlugInPropertyResourceBundle:
                    *(CFStringRef*)outData = CFStringCreateWithCString(NULL, "", kCFStringEncodingUTF8); break;
                case kAudioObjectPropertyOwnedObjects:
                case kAudioPlugInPropertyDeviceList:
                    *(AudioObjectID*)outData = kObjectID_Device; need = sizeof(AudioObjectID); break;
                case kAudioPlugInPropertyTranslateUIDToDevice:
                    // The qualifier is the requested device-UID CFString.
                    *(AudioObjectID*)outData = kObjectID_Device;
                    if (inQualifierDataSize == sizeof(CFStringRef) && inQualifierData != NULL) {
                        CFStringRef want = *(const CFStringRef*)inQualifierData;
                        CFStringRef ours = CFSTR(kDeviceUID);
                        if (want != NULL && CFStringCompare(want, ours, 0) != kCFCompareEqualTo) {
                            *(AudioObjectID*)outData = kAudioObjectUnknown;
                        }
                    }
                    break;
                default: return kAudioHardwareUnknownPropertyError;
            }
            break;

        case kObjectID_Device:
            switch (inAddress->mSelector) {
                case kAudioObjectPropertyBaseClass: *(AudioClassID*)outData = kAudioObjectClassID; break;
                case kAudioObjectPropertyClass:     *(AudioClassID*)outData = kAudioDeviceClassID; break;
                case kAudioObjectPropertyOwner:     *(AudioObjectID*)outData = kObjectID_PlugIn; break;
                case kAudioObjectPropertyName:
                    *(CFStringRef*)outData = CFStringCreateWithCString(NULL, kDeviceName, kCFStringEncodingUTF8); break;
                case kAudioObjectPropertyManufacturer:
                    *(CFStringRef*)outData = CFStringCreateWithCString(NULL, kManufacturer, kCFStringEncodingUTF8); break;
                case kAudioDevicePropertyDeviceUID:
                    *(CFStringRef*)outData = CFStringCreateWithCString(NULL, kDeviceUID, kCFStringEncodingUTF8); break;
                case kAudioDevicePropertyModelUID:
                    *(CFStringRef*)outData = CFStringCreateWithCString(NULL, kBoxModelUID, kCFStringEncodingUTF8); break;
                case kAudioDevicePropertyTransportType:
                    *(UInt32*)outData = kAudioDeviceTransportTypeVirtual; break;
                case kAudioDevicePropertyClockDomain:
                    *(UInt32*)outData = 0; break;
                case kAudioDevicePropertyDeviceIsAlive:
                    *(UInt32*)outData = 1; break;
                case kAudioDevicePropertyDeviceIsRunning: {
                    pthread_mutex_lock(&gStateMutex);
                    *(UInt32*)outData = gDeviceRunning ? 1 : 0;
                    pthread_mutex_unlock(&gStateMutex);
                    break;
                }
                case kAudioDevicePropertyDeviceCanBeDefaultDevice:
                case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
                    *(UInt32*)outData = 1; break;
                case kAudioDevicePropertyLatency:
                case kAudioDevicePropertySafetyOffset:
                case kAudioDevicePropertyIsHidden:
                    *(UInt32*)outData = 0; break;
                case kAudioDevicePropertyZeroTimeStampPeriod:
                    *(UInt32*)outData = kRingFrames; break;
                case kAudioObjectPropertyOwnedObjects:
                    *(AudioObjectID*)outData = kObjectID_Stream_Input; need = sizeof(AudioObjectID); break;
                case kAudioDevicePropertyStreams:
                    // Output scope: no streams (input-only device). Input/Global: the one input stream.
                    if (inAddress->mScope == kAudioObjectPropertyScopeOutput) {
                        need = 0;
                    } else {
                        *(AudioObjectID*)outData = kObjectID_Stream_Input;
                        need = sizeof(AudioObjectID);
                    }
                    break;
                case kAudioDevicePropertyRelatedDevices:
                    *(AudioObjectID*)outData = kObjectID_Device; need = sizeof(AudioObjectID); break;
                case kAudioObjectPropertyControlList:
                    need = 0; break;
                case kAudioDevicePropertyNominalSampleRate:
                    *(Float64*)outData = kSampleRate; break;
                case kAudioDevicePropertyAvailableNominalSampleRates: {
                    AudioValueRange* r = (AudioValueRange*)outData;
                    r->mMinimum = kSampleRate; r->mMaximum = kSampleRate;
                    need = sizeof(AudioValueRange); break;
                }
                case kAudioDevicePropertyPreferredChannelsForStereo: {
                    UInt32* ch = (UInt32*)outData; ch[0] = 1; ch[1] = 2;
                    need = 2 * sizeof(UInt32); break;
                }
                default: return kAudioHardwareUnknownPropertyError;
            }
            break;

        case kObjectID_Stream_Input:
            switch (inAddress->mSelector) {
                case kAudioObjectPropertyBaseClass: *(AudioClassID*)outData = kAudioObjectClassID; break;
                case kAudioObjectPropertyClass:     *(AudioClassID*)outData = kAudioStreamClassID; break;
                case kAudioObjectPropertyOwner:     *(AudioObjectID*)outData = kObjectID_Device; break;
                case kAudioObjectPropertyOwnedObjects: need = 0; break;
                case kAudioStreamPropertyIsActive:  *(UInt32*)outData = 1; break;
                case kAudioStreamPropertyDirection: *(UInt32*)outData = 1; break; // 1 = input
                case kAudioStreamPropertyTerminalType:
                    *(UInt32*)outData = kAudioStreamTerminalTypeMicrophone; break;
                case kAudioStreamPropertyStartingChannel:
                    *(UInt32*)outData = 1; break;
                case kAudioStreamPropertyLatency:
                    *(UInt32*)outData = 0; break;
                case kAudioStreamPropertyVirtualFormat:
                case kAudioStreamPropertyPhysicalFormat:
                    *(AudioStreamBasicDescription*)outData = stream_format(); break;
                case kAudioStreamPropertyAvailableVirtualFormats:
                case kAudioStreamPropertyAvailablePhysicalFormats: {
                    AudioStreamRangedDescription* d = (AudioStreamRangedDescription*)outData;
                    d->mFormat = stream_format();
                    d->mSampleRateRange.mMinimum = kSampleRate;
                    d->mSampleRateRange.mMaximum = kSampleRate;
                    need = sizeof(AudioStreamRangedDescription); break;
                }
                default: return kAudioHardwareUnknownPropertyError;
            }
            break;

        default:
            return kAudioHardwareBadObjectError;
    }

    *outDataSize = need;
    return noErr;
}

static OSStatus MacRDPMic_SetPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData) {
    (void)inDriver; (void)inObjectID; (void)inClientProcessID; (void)inAddress;
    (void)inQualifierDataSize; (void)inQualifierData; (void)inDataSize; (void)inData;
    // Nothing settable in P2a.
    return kAudioHardwareUnsupportedOperationError;
}

// ===========================================================================
// IO
// ===========================================================================
static OSStatus MacRDPMic_StartIO(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID) {
    (void)inDriver; (void)inClientID;
    if (inDeviceObjectID != kObjectID_Device) {
        return kAudioHardwareBadObjectError;
    }
    pthread_mutex_lock(&gStateMutex);
    if (gIOClientsRunning == 0) {
        gAnchorHostTime = mach_absolute_time();
        gClockSeed++;
        gDeviceRunning = true;
        // Map the macrdp feed ring now (off the real-time IO thread). If macrdp
        // isn't streaming, this no-ops and DoIOOperation plays the test tone.
        ring_map();
        os_log(gLog, "macrdp-mic: StartIO (first client) — clock anchored, feed %{public}s",
               gRing != NULL ? "mapped" : "absent (test tone)");
    }
    gIOClientsRunning++;
    pthread_mutex_unlock(&gStateMutex);
    return noErr;
}

static OSStatus MacRDPMic_StopIO(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID) {
    (void)inDriver; (void)inClientID;
    if (inDeviceObjectID != kObjectID_Device) {
        return kAudioHardwareBadObjectError;
    }
    pthread_mutex_lock(&gStateMutex);
    if (gIOClientsRunning > 0) {
        gIOClientsRunning--;
    }
    if (gIOClientsRunning == 0) {
        gDeviceRunning = false;
        ring_unmap();
        os_log(gLog, "macrdp-mic: StopIO (last client) — device idle");
    }
    pthread_mutex_unlock(&gStateMutex);
    return noErr;
}

static OSStatus MacRDPMic_GetZeroTimeStamp(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, Float64* outSampleTime, UInt64* outHostTime, UInt64* outSeed) {
    (void)inDriver; (void)inClientID;
    if (inDeviceObjectID != kObjectID_Device || outSampleTime == NULL || outHostTime == NULL || outSeed == NULL) {
        return kAudioHardwareIllegalOperationError;
    }
    // The virtual clock is driven purely by the host clock: every kRingFrames
    // frames is a new zero timestamp. coreaudiod interpolates between these.
    UInt64 now = mach_absolute_time();
    Float64 elapsedFrames = (Float64)(now - gAnchorHostTime) / gHostTicksPerFrame;
    UInt64 periods = (UInt64)(elapsedFrames / (Float64)kRingFrames);
    *outSampleTime = (Float64)(periods * kRingFrames);
    *outHostTime = gAnchorHostTime + (UInt64)((Float64)(periods * kRingFrames) * gHostTicksPerFrame);
    *outSeed = gClockSeed;
    return noErr;
}

static OSStatus MacRDPMic_WillDoIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, Boolean* outWillDo, Boolean* outWillDoInPlace) {
    (void)inDriver; (void)inDeviceObjectID; (void)inClientID;
    Boolean willDo = false;
    Boolean inPlace = true;
    if (inOperationID == kAudioServerPlugInIOOperationReadInput) {
        willDo = true;
    }
    if (outWillDo != NULL) { *outWillDo = willDo; }
    if (outWillDoInPlace != NULL) { *outWillDoInPlace = inPlace; }
    return noErr;
}

static OSStatus MacRDPMic_BeginIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo) {
    (void)inDriver; (void)inDeviceObjectID; (void)inClientID; (void)inOperationID; (void)inIOBufferFrameSize; (void)inIOCycleInfo;
    return noErr;
}

static OSStatus MacRDPMic_DoIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, AudioObjectID inStreamObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo, void* ioMainBuffer, void* ioSecondaryBuffer) {
    (void)inDriver; (void)inDeviceObjectID; (void)inStreamObjectID; (void)inClientID; (void)ioSecondaryBuffer;
    if (inOperationID != kAudioServerPlugInIOOperationReadInput || ioMainBuffer == NULL) {
        return noErr;
    }

    Float32* out = (Float32*)ioMainBuffer;

    // P2b: if macrdp's feed ring is mapped and initialized, drain it into the
    // input buffer (single-consumer). Otherwise fall back to the P2a test tone,
    // so the device is never silent-with-no-explanation during bring-up.
    MacrdpMicRing* ring = gRing;
    // Acquire-load the magic so the header the writer published before it (rate,
    // channels, ring_frames) is visible — release/acquire pair with the writer.
    uint32_t magic = (ring != NULL)
        ? atomic_load_explicit((_Atomic uint32_t*)&ring->magic, memory_order_acquire)
        : 0u;
    if (ring != NULL && magic == MACRDP_MIC_MAGIC && ring->ring_frames != 0) {
        const uint32_t cap = ring->ring_frames;       // power of two
        const uint32_t rch = ring->channels;
        uint64_t w = atomic_load_explicit(&ring->write_frames, memory_order_acquire);
        uint64_t r = atomic_load_explicit(&ring->read_frames, memory_order_relaxed);
        uint64_t avail = (w > r) ? (w - r) : 0;

        // Overrun: the writer lapped us (drift / a stall). Drop stale audio and
        // keep only ~half a ring of the freshest, so latency stays bounded.
        if (avail > cap) {
            r = w - (cap / 2);
            avail = cap / 2;
        }
        uint32_t toRead = (avail < (uint64_t)inIOBufferFrameSize) ? (uint32_t)avail : inIOBufferFrameSize;

        for (uint32_t i = 0; i < toRead; i++) {
            uint64_t idx = (r + i) & (uint64_t)(cap - 1);
            const float* src = &ring->samples[idx * rch];
            for (uint32_t c = 0; c < kChannelsPerFrame; c++) {
                // Map device channel c from the ring (mono ring → duplicate ch 0).
                out[i * kChannelsPerFrame + c] = (c < rch) ? src[c] : src[0];
            }
        }
        // Underrun: silence the frames the writer hasn't produced yet.
        for (uint32_t i = toRead; i < inIOBufferFrameSize; i++) {
            for (uint32_t c = 0; c < kChannelsPerFrame; c++) {
                out[i * kChannelsPerFrame + c] = 0.0f;
            }
        }
        atomic_store_explicit(&ring->read_frames, r + toRead, memory_order_release);
        return noErr;
    }

    // Fallback: a gentle 440 Hz tone, keyed to the requested sample time so it's
    // phase-continuous. (P2b-0 bring-up: the feed writer uses a different
    // frequency, so the recorded pitch tells feed-vs-fallback apart.)
    Float64 startFrame = inIOCycleInfo->mInputTime.mSampleTime;
    const Float64 twoPiFOverFs = 2.0 * M_PI * 440.0 / kSampleRate;
    for (UInt32 i = 0; i < inIOBufferFrameSize; i++) {
        Float32 s = (Float32)(0.05 * sin(twoPiFOverFs * (startFrame + (Float64)i)));
        for (UInt32 c = 0; c < kChannelsPerFrame; c++) {
            out[i * kChannelsPerFrame + c] = s;
        }
    }
    return noErr;
}

static OSStatus MacRDPMic_EndIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo) {
    (void)inDriver; (void)inDeviceObjectID; (void)inClientID; (void)inOperationID; (void)inIOBufferFrameSize; (void)inIOCycleInfo;
    return noErr;
}
