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

#ifdef __cplusplus
}
#endif
