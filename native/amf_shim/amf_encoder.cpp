// Encoder for the pipeline: BGRA D3D11 texture -> AMFVideoConverter -> AVC or HEVC encoder -> Annex B packets.
// HEVC (P9-3) takes the same configuration: its own property names, and the AVC enum values mapped to its own.
// The frame never leaves video memory: textures are wrapped with CreateSurfaceFromDX11Native and never mapped.

#include "amf_shim.h"
#include "amf_util.h"

#include <windows.h>
#include <d3d11.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <deque>
#include <exception>
#include <string>

#include "components/ColorSpace.h"
#include "components/VideoConverter.h"
#include "components/VideoEncoderHEVC.h"
#include "components/VideoEncoderVCE.h"
#include "core/Factory.h"
#include "core/Trace.h"

using namespace rcs_amf;

namespace {

constexpr double kConvertTimeoutMs = 100.0;
constexpr double kInputFullTimeoutMs = 500.0;
constexpr size_t kSurfaceCache = 8;
constexpr size_t kMaxStash = 16;

char* copy_out(const std::string& text) {
    auto out = static_cast<char*>(malloc(text.size() + 1));
    if (out) memcpy(out, text.c_str(), text.size() + 1);
    return out;
}

struct Readback {
    const char* key;
    const wchar_t* name;
};

const Readback kEncoderReadback[] = {
    {"usage", AMF_VIDEO_ENCODER_USAGE},
    {"profile", AMF_VIDEO_ENCODER_PROFILE},
    {"profile_level", AMF_VIDEO_ENCODER_PROFILE_LEVEL},
    {"quality_preset", AMF_VIDEO_ENCODER_QUALITY_PRESET},
    {"rate_control_method", AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD},
    {"target_bitrate", AMF_VIDEO_ENCODER_TARGET_BITRATE},
    {"peak_bitrate", AMF_VIDEO_ENCODER_PEAK_BITRATE},
    {"vbv_buffer_size", AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE},
    {"initial_vbv_fullness", AMF_VIDEO_ENCODER_INITIAL_VBV_BUFFER_FULLNESS},
    {"frame_rate", AMF_VIDEO_ENCODER_FRAMERATE},
    {"b_pic_pattern", AMF_VIDEO_ENCODER_B_PIC_PATTERN},
    {"idr_period", AMF_VIDEO_ENCODER_IDR_PERIOD},
    {"header_insertion_spacing", AMF_VIDEO_ENCODER_HEADER_INSERTION_SPACING},
    {"low_latency_mode", AMF_VIDEO_ENCODER_LOWLATENCY_MODE},
    {"max_num_ref_frames", AMF_VIDEO_ENCODER_MAX_NUM_REFRAMES},
    {"cabac", AMF_VIDEO_ENCODER_CABAC_ENABLE},
    {"enforce_hrd", AMF_VIDEO_ENCODER_ENFORCE_HRD},
    {"filler_data", AMF_VIDEO_ENCODER_FILLER_DATA_ENABLE},
    {"intra_refresh_mbs_per_slot", AMF_VIDEO_ENCODER_INTRA_REFRESH_NUM_MBS_PER_SLOT},
    {"intra_refresh_mode", L"IntraRefreshMode"},
    {"vbaq", AMF_VIDEO_ENCODER_ENABLE_VBAQ},
    {"max_au_size", AMF_VIDEO_ENCODER_MAX_AU_SIZE},
    {"query_timeout", AMF_VIDEO_ENCODER_QUERY_TIMEOUT},
    {"output_color_profile", AMF_VIDEO_ENCODER_OUTPUT_COLOR_PROFILE},
    {"output_transfer", AMF_VIDEO_ENCODER_OUTPUT_TRANSFER_CHARACTERISTIC},
    {"output_primaries", AMF_VIDEO_ENCODER_OUTPUT_COLOR_PRIMARIES},
    {"full_range", AMF_VIDEO_ENCODER_FULL_RANGE_COLOR},
};

const Readback kHevcReadback[] = {
    {"usage", AMF_VIDEO_ENCODER_HEVC_USAGE},
    {"profile", AMF_VIDEO_ENCODER_HEVC_PROFILE},
    {"tier", AMF_VIDEO_ENCODER_HEVC_TIER},
    {"profile_level", AMF_VIDEO_ENCODER_HEVC_PROFILE_LEVEL},
    {"quality_preset", AMF_VIDEO_ENCODER_HEVC_QUALITY_PRESET},
    {"rate_control_method", AMF_VIDEO_ENCODER_HEVC_RATE_CONTROL_METHOD},
    {"target_bitrate", AMF_VIDEO_ENCODER_HEVC_TARGET_BITRATE},
    {"peak_bitrate", AMF_VIDEO_ENCODER_HEVC_PEAK_BITRATE},
    {"vbv_buffer_size", AMF_VIDEO_ENCODER_HEVC_VBV_BUFFER_SIZE},
    {"frame_rate", AMF_VIDEO_ENCODER_HEVC_FRAMERATE},
    {"gop_size", AMF_VIDEO_ENCODER_HEVC_GOP_SIZE},
    {"gops_per_idr", AMF_VIDEO_ENCODER_HEVC_NUM_GOPS_PER_IDR},
    {"header_insertion_mode", AMF_VIDEO_ENCODER_HEVC_HEADER_INSERTION_MODE},
    {"low_latency_mode", AMF_VIDEO_ENCODER_HEVC_LOWLATENCY_MODE},
    {"max_num_ref_frames", AMF_VIDEO_ENCODER_HEVC_MAX_NUM_REFRAMES},
    {"enforce_hrd", AMF_VIDEO_ENCODER_HEVC_ENFORCE_HRD},
    {"filler_data", AMF_VIDEO_ENCODER_HEVC_FILLER_DATA_ENABLE},
    {"vbaq", AMF_VIDEO_ENCODER_HEVC_ENABLE_VBAQ},
    {"max_au_size", AMF_VIDEO_ENCODER_HEVC_MAX_AU_SIZE},
    {"query_timeout", AMF_VIDEO_ENCODER_HEVC_QUERY_TIMEOUT},
    {"output_color_profile", AMF_VIDEO_ENCODER_HEVC_OUTPUT_COLOR_PROFILE},
    {"output_transfer", AMF_VIDEO_ENCODER_HEVC_OUTPUT_TRANSFER_CHARACTERISTIC},
    {"output_primaries", AMF_VIDEO_ENCODER_HEVC_OUTPUT_COLOR_PRIMARIES},
    {"full_range", AMF_VIDEO_ENCODER_HEVC_NOMINAL_RANGE},
};

// AVC enum values of the configuration to the HEVC ones; -1 when HEVC has no such value.
amf_int64 hevc_rate_control(int32_t avc) {
    switch (avc) {
        case AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP: return AMF_VIDEO_ENCODER_HEVC_RATE_CONTROL_METHOD_CONSTANT_QP;
        case AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR: return AMF_VIDEO_ENCODER_HEVC_RATE_CONTROL_METHOD_CBR;
        case AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR:
            return AMF_VIDEO_ENCODER_HEVC_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR;
        case AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_LATENCY_CONSTRAINED_VBR:
            return AMF_VIDEO_ENCODER_HEVC_RATE_CONTROL_METHOD_LATENCY_CONSTRAINED_VBR;
        default: return -1;
    }
}

amf_int64 hevc_quality_preset(int32_t avc) {
    switch (avc) {
        case AMF_VIDEO_ENCODER_QUALITY_PRESET_BALANCED: return AMF_VIDEO_ENCODER_HEVC_QUALITY_PRESET_BALANCED;
        case AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED: return AMF_VIDEO_ENCODER_HEVC_QUALITY_PRESET_SPEED;
        case AMF_VIDEO_ENCODER_QUALITY_PRESET_QUALITY: return AMF_VIDEO_ENCODER_HEVC_QUALITY_PRESET_QUALITY;
        default: return -1;
    }
}

const Readback kConverterReadback[] = {
    {"output_format", AMF_VIDEO_CONVERTER_OUTPUT_FORMAT},
    {"memory_type", AMF_VIDEO_CONVERTER_MEMORY_TYPE},
    {"output_size", AMF_VIDEO_CONVERTER_OUTPUT_SIZE},
    {"color_profile", AMF_VIDEO_CONVERTER_COLOR_PROFILE},
    {"input_color_range", AMF_VIDEO_CONVERTER_INPUT_COLOR_RANGE},
    {"output_color_range", AMF_VIDEO_CONVERTER_OUTPUT_COLOR_RANGE},
};

}  // namespace

struct RcsAmfEncoder {
    RcsAmfEncoderConfig config{};
    amf::AMFFactory* factory = nullptr;
    amf::AMFTrace* trace = nullptr;
    amf::AMFContextPtr context;
    amf::AMFComponentPtr converter;
    amf::AMFComponentPtr encoder;
    std::string runtime_version;
    std::string error;

    // Wrappers of the capture pool textures, so a frame costs no new AMF surface once the pool is known.
    void* cached_texture[kSurfaceCache] = {};
    amf::AMFSurfacePtr cached_surface[kSurfaceCache];
    size_t cache_next = 0;

    std::deque<amf::AMFBufferPtr> stash;  // outputs taken out while the input queue was full
    amf::AMFBufferPtr current;            // the buffer behind the last packet

    bool fail(const char* what, AMF_RESULT r) {
        char buffer[256];
        std::string text = trace ? utf8(trace->GetResultText(r)) : std::string();
        snprintf(buffer, sizeof buffer, "%s: %d %s", what, static_cast<int>(r), text.c_str());
        error = buffer;
        return false;
    }

    bool hevc() const { return config.codec == RCS_AMF_CODEC_HEVC; }
    bool start(ID3D11Device* device, const RcsAmfEncoderConfig& c);
    bool start_avc(const RcsAmfEncoderConfig& c);
    bool start_hevc(const RcsAmfEncoderConfig& c);
    amf::AMFSurface* wrap(void* texture, AMF_RESULT& r);
};

namespace {

template <typename T>
bool set(RcsAmfEncoder* e, amf::AMFComponent* component, const wchar_t* name, const T& value) {
    AMF_RESULT r = component->SetProperty(name, value);
    if (r == AMF_OK) return true;
    return e->fail(utf8(name).c_str(), r);
}

}  // namespace

bool RcsAmfEncoder::start(ID3D11Device* device, const RcsAmfEncoderConfig& c) {
    config = c;
    // System32 only: the runtime comes with the driver, and the search must not pick up a planted copy.
    HMODULE dll = LoadLibraryExW(AMF_DLL_NAME, nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!dll) {
        error = "amfrt64.dll not found in System32, win32 error " + std::to_string(GetLastError());
        return false;
    }
    auto query_version = reinterpret_cast<AMFQueryVersion_Fn>(GetProcAddress(dll, AMF_QUERY_VERSION_FUNCTION_NAME));
    auto init = reinterpret_cast<AMFInit_Fn>(GetProcAddress(dll, AMF_INIT_FUNCTION_NAME));
    if (!init) {
        error = "AMFInit not exported";
        return false;
    }
    amf_uint64 runtime = 0;
    if (query_version) query_version(&runtime);
    char version[64];
    snprintf(version, sizeof version, "%llu.%llu.%llu", AMF_GET_MAJOR_VERSION(runtime), AMF_GET_MINOR_VERSION(runtime),
             AMF_GET_SUBMINOR_VERSION(runtime));
    runtime_version = version;
    AMF_RESULT r = init(AMF_FULL_VERSION, &factory);
    if ((r != AMF_OK || !factory) && runtime != 0) r = init(runtime, &factory);
    if (r != AMF_OK || !factory) return fail("AMFInit", r);
    factory->GetTrace(&trace);

    if ((r = factory->CreateContext(&context)) != AMF_OK) return fail("CreateContext", r);
    r = context->InitDX11(device, amf::AMF_DX11_1);
    if (r != AMF_OK) r = context->InitDX11(device, amf::AMF_DX11_0);
    if (r != AMF_OK) return fail("InitDX11", r);

    // Desktop Duplication gives full range sRGB BGRA. The converter applies the BT.709 matrix with studio range.
    if ((r = factory->CreateComponent(context, AMFVideoConverter, &converter)) != AMF_OK)
        return fail("create converter", r);
    if (!set(this, converter, AMF_VIDEO_CONVERTER_MEMORY_TYPE, static_cast<amf_int64>(amf::AMF_MEMORY_DX11)) ||
        !set(this, converter, AMF_VIDEO_CONVERTER_OUTPUT_FORMAT, static_cast<amf_int64>(amf::AMF_SURFACE_NV12)) ||
        !set(this, converter, AMF_VIDEO_CONVERTER_OUTPUT_SIZE, ::AMFConstructSize(c.width, c.height)) ||
        !set(this, converter, AMF_VIDEO_CONVERTER_COLOR_PROFILE,
             static_cast<amf_int64>(AMF_VIDEO_CONVERTER_COLOR_PROFILE_709)) ||
        !set(this, converter, AMF_VIDEO_CONVERTER_INPUT_COLOR_RANGE, static_cast<amf_int64>(AMF_COLOR_RANGE_FULL)) ||
        !set(this, converter, AMF_VIDEO_CONVERTER_OUTPUT_COLOR_RANGE, static_cast<amf_int64>(AMF_COLOR_RANGE_STUDIO)))
        return false;
    if ((r = converter->Init(amf::AMF_SURFACE_BGRA, c.input_width, c.input_height)) != AMF_OK)
        return fail("converter Init", r);

    if (!(c.codec == RCS_AMF_CODEC_HEVC ? start_hevc(c) : start_avc(c))) return false;
    if ((r = encoder->Init(amf::AMF_SURFACE_NV12, c.width, c.height)) != AMF_OK) return fail("encoder Init", r);
    return true;
}

bool RcsAmfEncoder::start_avc(const RcsAmfEncoderConfig& c) {
    AMF_RESULT r = factory->CreateComponent(context, AMFVideoEncoderVCE_AVC, &encoder);
    if (r != AMF_OK) return fail("create encoder", r);
    // Usage first: it resets every other property to the defaults of that usage.
    if (!set(this, encoder, AMF_VIDEO_ENCODER_USAGE, static_cast<amf_int64>(c.usage)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_PROFILE, static_cast<amf_int64>(c.profile)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD, static_cast<amf_int64>(c.rate_control)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_TARGET_BITRATE, static_cast<amf_int64>(c.bitrate)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_PEAK_BITRATE, static_cast<amf_int64>(c.bitrate)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_FRAMERATE, ::AMFConstructRate(static_cast<amf_uint32>(c.fps), 1)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_B_PIC_PATTERN, static_cast<amf_int64>(0)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_IDR_PERIOD, static_cast<amf_int64>(c.idr_period)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEADER_INSERTION_SPACING, static_cast<amf_int64>(c.idr_period)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_QUERY_TIMEOUT, static_cast<amf_int64>(c.query_timeout_ms)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_OUTPUT_COLOR_PROFILE,
             static_cast<amf_int64>(AMF_VIDEO_CONVERTER_COLOR_PROFILE_709)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_OUTPUT_TRANSFER_CHARACTERISTIC,
             static_cast<amf_int64>(AMF_COLOR_TRANSFER_CHARACTERISTIC_BT709)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_OUTPUT_COLOR_PRIMARIES,
             static_cast<amf_int64>(AMF_COLOR_PRIMARIES_BT709)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_FULL_RANGE_COLOR, false))
        return false;
    if (c.vbv_bits > 0 && !set(this, encoder, AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE, static_cast<amf_int64>(c.vbv_bits)))
        return false;
    if (c.quality_preset >= 0 &&
        !set(this, encoder, AMF_VIDEO_ENCODER_QUALITY_PRESET, static_cast<amf_int64>(c.quality_preset)))
        return false;
    if (c.intra_refresh_mbs >= 0 && !set(this, encoder, AMF_VIDEO_ENCODER_INTRA_REFRESH_NUM_MBS_PER_SLOT,
                                         static_cast<amf_int64>(c.intra_refresh_mbs)))
        return false;
    // "IntraRefreshMode" is not in the AVC header of SDK 1.5.3; the name and values come from the property catalog
    // of the runtime on the server (rcs-bench probe): 0 Disabled, 1 GopAligned, 2 Continuous.
    if (c.intra_refresh_mode >= 0 &&
        !set(this, encoder, L"IntraRefreshMode", static_cast<amf_int64>(c.intra_refresh_mode)))
        return false;
    if (c.vbaq >= 0 && !set(this, encoder, AMF_VIDEO_ENCODER_ENABLE_VBAQ, c.vbaq != 0)) return false;
    if (c.max_au_bits > 0 &&
        !set(this, encoder, AMF_VIDEO_ENCODER_MAX_AU_SIZE, static_cast<amf_int64>(c.max_au_bits)))
        return false;
    if (c.enforce_hrd >= 0 && !set(this, encoder, AMF_VIDEO_ENCODER_ENFORCE_HRD, c.enforce_hrd != 0)) return false;
    if (c.filler_data >= 0 && !set(this, encoder, AMF_VIDEO_ENCODER_FILLER_DATA_ENABLE, c.filler_data != 0))
        return false;
    return true;
}

bool RcsAmfEncoder::start_hevc(const RcsAmfEncoderConfig& c) {
    AMF_RESULT r = factory->CreateComponent(context, AMFVideoEncoder_HEVC, &encoder);
    if (r != AMF_OK) return fail("create HEVC encoder", r);
    amf_int64 rate_control = hevc_rate_control(c.rate_control);
    if (rate_control < 0) return fail("rate control has no HEVC equivalent", AMF_INVALID_ARG);
    // Usage first, as for AVC. Main profile, Main tier: 8-bit 4:2:0 like the NV12 from the converter. No B-frames on
    // this hardware for HEVC; every IDR carries VPS, SPS and PPS.
    if (!set(this, encoder, AMF_VIDEO_ENCODER_HEVC_USAGE, static_cast<amf_int64>(c.usage)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_PROFILE, static_cast<amf_int64>(AMF_VIDEO_ENCODER_HEVC_PROFILE_MAIN)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_TIER, static_cast<amf_int64>(AMF_VIDEO_ENCODER_HEVC_TIER_MAIN)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_RATE_CONTROL_METHOD, rate_control) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_TARGET_BITRATE, static_cast<amf_int64>(c.bitrate)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_PEAK_BITRATE, static_cast<amf_int64>(c.bitrate)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_FRAMERATE, ::AMFConstructRate(static_cast<amf_uint32>(c.fps), 1)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_GOP_SIZE, static_cast<amf_int64>(c.idr_period)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_NUM_GOPS_PER_IDR, static_cast<amf_int64>(1)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_HEADER_INSERTION_MODE,
             static_cast<amf_int64>(AMF_VIDEO_ENCODER_HEVC_HEADER_INSERTION_MODE_IDR_ALIGNED)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_QUERY_TIMEOUT, static_cast<amf_int64>(c.query_timeout_ms)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_OUTPUT_COLOR_PROFILE,
             static_cast<amf_int64>(AMF_VIDEO_CONVERTER_COLOR_PROFILE_709)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_OUTPUT_TRANSFER_CHARACTERISTIC,
             static_cast<amf_int64>(AMF_COLOR_TRANSFER_CHARACTERISTIC_BT709)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_OUTPUT_COLOR_PRIMARIES,
             static_cast<amf_int64>(AMF_COLOR_PRIMARIES_BT709)) ||
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_NOMINAL_RANGE,
             static_cast<amf_int64>(AMF_VIDEO_ENCODER_HEVC_NOMINAL_RANGE_STUDIO)))
        return false;
    if (c.vbv_bits > 0 &&
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_VBV_BUFFER_SIZE, static_cast<amf_int64>(c.vbv_bits)))
        return false;
    if (c.quality_preset >= 0) {
        amf_int64 preset = hevc_quality_preset(c.quality_preset);
        if (preset < 0) return fail("quality preset has no HEVC equivalent", AMF_INVALID_ARG);
        if (!set(this, encoder, AMF_VIDEO_ENCODER_HEVC_QUALITY_PRESET, preset)) return false;
    }
    if (c.vbaq >= 0 && !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_ENABLE_VBAQ, c.vbaq != 0)) return false;
    if (c.max_au_bits > 0 &&
        !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_MAX_AU_SIZE, static_cast<amf_int64>(c.max_au_bits)))
        return false;
    if (c.enforce_hrd >= 0 && !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_ENFORCE_HRD, c.enforce_hrd != 0))
        return false;
    if (c.filler_data >= 0 && !set(this, encoder, AMF_VIDEO_ENCODER_HEVC_FILLER_DATA_ENABLE, c.filler_data != 0))
        return false;
    return true;
}

amf::AMFSurface* RcsAmfEncoder::wrap(void* texture, AMF_RESULT& r) {
    for (size_t i = 0; i < kSurfaceCache; ++i) {
        if (cached_texture[i] == texture) {
            r = AMF_OK;
            return cached_surface[i];
        }
    }
    amf::AMFSurfacePtr surface;
    r = context->CreateSurfaceFromDX11Native(texture, &surface, nullptr);
    if (r != AMF_OK) return nullptr;
    size_t slot = cache_next++ % kSurfaceCache;
    cached_texture[slot] = texture;
    cached_surface[slot] = surface;
    return cached_surface[slot];
}

extern "C" RcsAmfEncoder* rcs_amf_encoder_create(void* d3d11_device, const RcsAmfEncoderConfig* config,
                                                 char* message, size_t message_size) {
    auto write = [&](const std::string& text) {
        if (message && message_size) snprintf(message, message_size, "%s", text.c_str());
    };
    try {
        auto* e = new RcsAmfEncoder();
        if (e->start(static_cast<ID3D11Device*>(d3d11_device), *config)) return e;
        write(e->error);
        rcs_amf_encoder_destroy(e);
    } catch (const std::exception& ex) {
        write(ex.what());
    } catch (...) {
        write("unknown C++ exception");
    }
    return nullptr;
}

extern "C" int32_t rcs_amf_encoder_submit(RcsAmfEncoder* e, void* d3d11_texture, int64_t pts, int32_t force_idr,
                                          RcsAmfSubmitInfo* info) {
    try {
        *info = RcsAmfSubmitInfo{};
        AMF_RESULT r = AMF_OK;
        amf::AMFSurface* input = e->wrap(d3d11_texture, r);
        if (!input) return e->fail("CreateSurfaceFromDX11Native", r), RCS_AMF_ERROR;
        input->SetPts(pts);
        info->input_memory = input->GetMemoryType();

        double t0 = now_ms();
        if ((r = e->converter->SubmitInput(input)) != AMF_OK) return e->fail("converter SubmitInput", r), RCS_AMF_ERROR;
        amf::AMFDataPtr converted;
        for (;;) {
            r = e->converter->QueryOutput(&converted);
            if (converted) break;
            if (r != AMF_OK && r != AMF_REPEAT) return e->fail("converter QueryOutput", r), RCS_AMF_ERROR;
            if (now_ms() - t0 > kConvertTimeoutMs) return e->fail("converter timeout", r), RCS_AMF_ERROR;
            SwitchToThread();
        }
        info->convert_ms = now_ms() - t0;
        info->output_memory = converted->GetMemoryType();
        converted->SetPts(pts);
        if (force_idr && e->hevc()) {
            converted->SetProperty(AMF_VIDEO_ENCODER_HEVC_FORCE_PICTURE_TYPE,
                                   static_cast<amf_int64>(AMF_VIDEO_ENCODER_HEVC_PICTURE_TYPE_IDR));
            converted->SetProperty(AMF_VIDEO_ENCODER_HEVC_INSERT_HEADER, true);
        } else if (force_idr) {
            converted->SetProperty(AMF_VIDEO_ENCODER_FORCE_PICTURE_TYPE,
                                   static_cast<amf_int64>(AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR));
            converted->SetProperty(AMF_VIDEO_ENCODER_INSERT_SPS, true);
            converted->SetProperty(AMF_VIDEO_ENCODER_INSERT_PPS, true);
        }

        double t1 = now_ms();
        for (;;) {
            r = e->encoder->SubmitInput(converted);
            if (r != AMF_INPUT_FULL) break;
            ++info->input_full;
            amf::AMFDataPtr out;
            AMF_RESULT q = e->encoder->QueryOutput(&out);
            if (out) {
                if (e->stash.size() >= kMaxStash) return e->fail("output stash full", q), RCS_AMF_ERROR;
                e->stash.emplace_back(out);
            } else if (q != AMF_OK && q != AMF_REPEAT) {
                return e->fail("encoder QueryOutput", q), RCS_AMF_ERROR;
            }
            if (now_ms() - t1 > kInputFullTimeoutMs) return e->fail("encoder input full", r), RCS_AMF_ERROR;
            SwitchToThread();
        }
        if (r != AMF_OK) return e->fail("encoder SubmitInput", r), RCS_AMF_ERROR;
        return RCS_AMF_OK;
    } catch (const std::exception& ex) {
        e->error = ex.what();
        return RCS_AMF_ERROR;
    }
}

extern "C" int32_t rcs_amf_encoder_query(RcsAmfEncoder* e, RcsAmfPacket* packet) {
    try {
        *packet = RcsAmfPacket{};
        e->current = nullptr;
        if (!e->stash.empty()) {
            e->current = e->stash.front();
            e->stash.pop_front();
        } else {
            amf::AMFDataPtr out;
            AMF_RESULT r = e->encoder->QueryOutput(&out);
            if (!out) {
                if (r == AMF_OK || r == AMF_REPEAT || r == AMF_EOF) return RCS_AMF_AGAIN;
                return e->fail("encoder QueryOutput", r), RCS_AMF_ERROR;
            }
            e->current = amf::AMFBufferPtr(out);
            if (!e->current) return e->fail("encoder output is not a buffer", AMF_INVALID_DATA_TYPE), RCS_AMF_ERROR;
        }
        packet->data = static_cast<const uint8_t*>(e->current->GetNative());
        packet->size = e->current->GetSize();
        packet->pts = e->current->GetPts();
        packet->memory = e->current->GetMemoryType();
        amf::AMFVariant type;
        // Both enums put IDR first, so 0 means IDR for either codec.
        const wchar_t* type_name = e->hevc() ? AMF_VIDEO_ENCODER_HEVC_OUTPUT_DATA_TYPE : AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE;
        packet->output_type = e->current->GetProperty(type_name, &type) == AMF_OK
                                  ? static_cast<int32_t>(type.int64Value)
                                  : -1;
        return RCS_AMF_OK;
    } catch (const std::exception& ex) {
        e->error = ex.what();
        return RCS_AMF_ERROR;
    }
}

extern "C" int32_t rcs_amf_encoder_set_bitrate(RcsAmfEncoder* e, int64_t bitrate) {
    const wchar_t* target = e->hevc() ? AMF_VIDEO_ENCODER_HEVC_TARGET_BITRATE : AMF_VIDEO_ENCODER_TARGET_BITRATE;
    const wchar_t* peak = e->hevc() ? AMF_VIDEO_ENCODER_HEVC_PEAK_BITRATE : AMF_VIDEO_ENCODER_PEAK_BITRATE;
    AMF_RESULT r = e->encoder->SetProperty(target, static_cast<amf_int64>(bitrate));
    if (r == AMF_OK) r = e->encoder->SetProperty(peak, static_cast<amf_int64>(bitrate));
    if (r != AMF_OK) return e->fail("set bitrate", r), RCS_AMF_ERROR;
    return RCS_AMF_OK;
}

extern "C" const char* rcs_amf_encoder_error(RcsAmfEncoder* e) { return e->error.c_str(); }

extern "C" char* rcs_amf_encoder_settings(RcsAmfEncoder* e) {
    try {
        Json j;
        j.begin_object();
        j.string("runtime_version", e->runtime_version);
        j.string("codec", e->hevc() ? "hevc" : "avc");
        j.begin_object("encoder");
        auto read = [&](const Readback& item) {
            amf::AMFVariant value;
            if (e->encoder->GetProperty(item.name, &value) == AMF_OK) variant(j, item.key, value);
        };
        if (e->hevc()) {
            for (const Readback& item : kHevcReadback) read(item);
        } else {
            for (const Readback& item : kEncoderReadback) read(item);
        }
        j.end_object();
        j.begin_object("converter");
        for (const Readback& item : kConverterReadback) {
            amf::AMFVariant value;
            if (e->converter->GetProperty(item.name, &value) == AMF_OK) variant(j, item.key, value);
        }
        j.end_object();
        j.end_object();
        return copy_out(j.text);
    } catch (...) {
        return nullptr;
    }
}

extern "C" void rcs_amf_encoder_destroy(RcsAmfEncoder* e) {
    if (!e) return;
    e->current = nullptr;
    e->stash.clear();
    for (auto& surface : e->cached_surface) surface = nullptr;
    if (e->encoder) e->encoder->Terminate();
    if (e->converter) e->converter->Terminate();
    e->encoder = nullptr;
    e->converter = nullptr;
    if (e->context) e->context->Terminate();
    e->context = nullptr;
    delete e;
}
