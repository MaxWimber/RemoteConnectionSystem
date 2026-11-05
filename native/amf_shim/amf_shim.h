// C API over the AMD AMF C++ runtime. The Rust side never touches AMF interfaces directly.
#pragma once

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RcsAmfProbeOptions {
    int32_t width;
    int32_t height;
    int32_t fps;
    int32_t frames;
    int64_t bitrate;
} RcsAmfProbeOptions;

// Loads amfrt64.dll from System32, reports runtime, components and capabilities, and encodes synthetic frames
// with every usage on the given ID3D11Device. Returns a UTF-8 JSON document, free it with rcs_amf_free.
char* rcs_amf_probe(void* d3d11_device, const RcsAmfProbeOptions* options);

// Ultra Low Latency probe: property catalogs after Usage, Init with Usage alone and with one property at a time,
// a greedy accepted set, and AMF trace lines for every failed Init. Same ownership rules as rcs_amf_probe.
char* rcs_amf_ull_probe(void* d3d11_device, const RcsAmfProbeOptions* options);

void rcs_amf_free(char* text);

// ---- Encoder: BGRA D3D11 texture -> AMFVideoConverter (NV12, BT.709, limited range) -> AVC encoder -> Annex B.
// Every frame stays in video memory: the texture is wrapped with CreateSurfaceFromDX11Native, never mapped.

typedef struct RcsAmfEncoderConfig {
    int32_t input_width;   // size of the BGRA textures passed to submit
    int32_t input_height;
    int32_t width;         // encoded size, the converter scales when it differs
    int32_t height;
    int32_t fps;
    int64_t bitrate;       // bits per second, CBR target and peak
    int64_t vbv_bits;      // 0 keeps the default of the usage
    int32_t usage;         // AMF_VIDEO_ENCODER_USAGE_ENUM
    int32_t profile;       // AMF_VIDEO_ENCODER_PROFILE_ENUM: 66, 77, 100, 256 constrained baseline, 257 constrained high
    int32_t quality_preset;     // -1 keeps the default, otherwise AMF_VIDEO_ENCODER_QUALITY_PRESET_ENUM
    int32_t idr_period;         // frames between IDR frames
    int32_t intra_refresh_mbs;  // -1 keeps the default, 0 off, otherwise macroblocks per slot
    int32_t query_timeout_ms;   // 0 means QueryOutput never waits
    int32_t enforce_hrd;        // -1 keeps the default, otherwise bool
    int32_t filler_data;        // -1 keeps the default, otherwise bool
    int32_t rate_control;       // AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_ENUM; target and peak are both bitrate
    int32_t vbaq;               // -1 keeps the default, otherwise bool
    int64_t max_au_bits;        // 0 keeps the default, otherwise the largest access unit in bits
} RcsAmfEncoderConfig;

typedef struct RcsAmfSubmitInfo {
    double convert_ms;      // CPU time from converter submit to its NV12 output
    int32_t input_memory;   // AMF_MEMORY_TYPE of the wrapped texture
    int32_t output_memory;  // AMF_MEMORY_TYPE of the NV12 surface handed to the encoder
    int32_t input_full;     // times the encoder queue was full and outputs were taken out first
} RcsAmfSubmitInfo;

typedef struct RcsAmfPacket {
    const uint8_t* data;  // valid until the next query or destroy
    size_t size;
    int64_t pts;          // the pts passed to submit
    int32_t output_type;  // AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_ENUM, 0 is IDR
    int32_t memory;       // AMF_MEMORY_TYPE of the output buffer
} RcsAmfPacket;

typedef struct RcsAmfEncoder RcsAmfEncoder;

enum { RCS_AMF_OK = 0, RCS_AMF_AGAIN = 1, RCS_AMF_ERROR = -1 };

// Creates the converter and the encoder on the device. On failure returns null and writes a message.
RcsAmfEncoder* rcs_amf_encoder_create(void* d3d11_device, const RcsAmfEncoderConfig* config, char* message,
                                      size_t message_size);

// Converts and submits one texture. force_idr makes this frame an IDR with SPS and PPS.
int32_t rcs_amf_encoder_submit(RcsAmfEncoder* encoder, void* d3d11_texture, int64_t pts, int32_t force_idr,
                               RcsAmfSubmitInfo* info);

// RCS_AMF_OK with a packet, RCS_AMF_AGAIN when none is ready within the query timeout, or RCS_AMF_ERROR.
int32_t rcs_amf_encoder_query(RcsAmfEncoder* encoder, RcsAmfPacket* packet);

int32_t rcs_amf_encoder_set_bitrate(RcsAmfEncoder* encoder, int64_t bitrate);

// Text of the last error of this encoder, valid until the next call on it.
const char* rcs_amf_encoder_error(RcsAmfEncoder* encoder);

// Encoder and converter settings read back after Init, as JSON. Free with rcs_amf_free.
char* rcs_amf_encoder_settings(RcsAmfEncoder* encoder);

void rcs_amf_encoder_destroy(RcsAmfEncoder* encoder);

#ifdef __cplusplus
}
#endif
