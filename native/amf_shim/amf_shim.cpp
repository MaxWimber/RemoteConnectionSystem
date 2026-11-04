// Probe of the AMD AMF runtime: versions, components, capabilities and a short encode with every usage.
// The runtime is loaded from System32 at run time, so the program also starts on machines without AMD.

#include "amf_shim.h"
#include "amf_util.h"

#include <windows.h>
#include <d3d11.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <deque>
#include <exception>
#include <mutex>
#include <string>
#include <vector>

#include "components/ColorSpace.h"
#include "components/VideoConverter.h"
#include "components/VideoEncoderAV1.h"
#include "components/VideoEncoderHEVC.h"
#include "components/VideoEncoderVCE.h"
#include "core/Factory.h"
#include "core/Trace.h"

#pragma comment(lib, "version.lib")

using namespace rcs_amf;

namespace {

constexpr int kSourceFrames = 8;
constexpr double kConvertTimeoutMs = 100.0;
constexpr double kOutputWaitMs = 40.0;
constexpr double kDrainTimeoutMs = 500.0;

amf::AMFTrace* g_trace = nullptr;

std::string version_text(amf_uint64 v) {
    char buffer[64];
    snprintf(buffer, sizeof buffer, "%llu.%llu.%llu.%llu", AMF_GET_MAJOR_VERSION(v), AMF_GET_MINOR_VERSION(v),
             AMF_GET_SUBMINOR_VERSION(v), AMF_GET_BUILD_VERSION(v));
    return buffer;
}

std::string file_version(const wchar_t* path) {
    DWORD handle = 0;
    DWORD size = GetFileVersionInfoSizeW(path, &handle);
    if (size == 0) return {};
    std::vector<BYTE> data(size);
    if (!GetFileVersionInfoW(path, 0, size, data.data())) return {};
    VS_FIXEDFILEINFO* info = nullptr;
    UINT length = 0;
    if (!VerQueryValueW(data.data(), L"\\", reinterpret_cast<void**>(&info), &length) || !info) return {};
    char buffer[64];
    snprintf(buffer, sizeof buffer, "%u.%u.%u.%u", HIWORD(info->dwFileVersionMS), LOWORD(info->dwFileVersionMS),
             HIWORD(info->dwFileVersionLS), LOWORD(info->dwFileVersionLS));
    return buffer;
}

void result(Json& j, const char* key, AMF_RESULT r) {
    j.begin_object(key);
    j.integer("code", r);
    if (g_trace) j.wstring("text", g_trace->GetResultText(r));
    j.end_object();
}
void io_caps(Json& j, const char* key, amf::AMFIOCaps* caps) {
    j.begin_object(key);
    amf_int32 low = 0, high = 0;
    caps->GetWidthRange(&low, &high);
    j.integer("width_min", low);
    j.integer("width_max", high);
    caps->GetHeightRange(&low, &high);
    j.integer("height_min", low);
    j.integer("height_max", high);
    j.integer("vert_align", caps->GetVertAlign());
    j.begin_array("formats");
    for (amf_int32 i = 0; i < caps->GetNumOfFormats(); ++i) {
        amf::AMF_SURFACE_FORMAT format{};
        amf_bool native = false;
        if (caps->GetFormatAt(i, &format, &native) != AMF_OK) continue;
        j.begin_object();
        j.integer("format", format);
        j.boolean("native", native != 0);
        j.end_object();
    }
    j.end_array();
    j.begin_array("memory_types");
    for (amf_int32 i = 0; i < caps->GetNumOfMemoryTypes(); ++i) {
        amf::AMF_MEMORY_TYPE type{};
        amf_bool native = false;
        if (caps->GetMemoryTypeAt(i, &type, &native) != AMF_OK) continue;
        j.begin_object();
        j.integer("memory", type);
        j.boolean("native", native != 0);
        j.end_object();
    }
    j.end_array();
    j.boolean("interlaced", caps->IsInterlacedSupported() != 0);
    j.end_object();
}

void property_catalog(Json& j, amf::AMFComponent* component) {
    j.begin_array("properties");
    amf_size count = component->GetPropertiesInfoCount();
    for (amf_size i = 0; i < count; ++i) {
        const amf::AMFPropertyInfo* info = nullptr;
        if (component->GetPropertyInfo(i, &info) != AMF_OK || !info) continue;
        j.begin_object();
        j.wstring("name", info->name);
        j.integer("type", info->type);
        j.integer("access", info->accessType);
        variant(j, "default", info->defaultValue);
        variant(j, "min", info->minValue);
        variant(j, "max", info->maxValue);
        amf::AMFVariant current;
        if (component->GetProperty(info->name, &current) == AMF_OK) variant(j, "current", current);
        if (info->pEnumDescription) {
            j.begin_array("enum");
            for (const amf::AMFEnumDescriptionEntry* e = info->pEnumDescription; e->name; ++e) {
                j.begin_object();
                j.integer("value", e->value);
                j.wstring("name", e->name);
                j.end_object();
            }
            j.end_array();
        }
        j.end_object();
    }
    j.end_array();
}

void describe_component(Json& j, amf::AMFFactory* factory, amf::AMFContext* context, const wchar_t* id) {
    j.begin_object();
    j.wstring("id", id);
    amf::AMFComponentPtr component;
    AMF_RESULT r = factory->CreateComponent(context, id, &component);
    result(j, "create", r);
    if (r == AMF_OK && component) {
        amf::AMFCapsPtr caps;
        r = component->GetCaps(&caps);
        result(j, "get_caps", r);
        if (r == AMF_OK && caps) {
            j.integer("acceleration", caps->GetAccelerationType());
            j.begin_object("caps");
            for (amf_size i = 0; i < caps->GetPropertyCount(); ++i) {
                wchar_t name[256] = {};
                amf::AMFVariant value;
                if (caps->GetPropertyAt(i, name, 256, &value) == AMF_OK) variant(j, utf8(name).c_str(), value);
            }
            j.end_object();
            amf::AMFIOCapsPtr input;
            if (caps->GetInputCaps(&input) == AMF_OK && input) io_caps(j, "input", input);
            amf::AMFIOCapsPtr output;
            if (caps->GetOutputCaps(&output) == AMF_OK && output) io_caps(j, "output", output);
        }
        property_catalog(j, component);
        component->Terminate();
    }
    j.end_object();
}

// A gradient that moves between frames plus a block of noise, so the encoder sees both flat and detailed areas.
void fill_pattern(std::vector<uint32_t>& pixels, int width, int height, int index) {
    for (int y = 0; y < height; ++y) {
        for (int x = 0; x < width; ++x) {
            uint32_t r = static_cast<uint32_t>(x + index * 24) & 255;
            uint32_t g = static_cast<uint32_t>(y + index * 12) & 255;
            uint32_t b = static_cast<uint32_t>((x ^ y) + index * 8) & 255;
            pixels[static_cast<size_t>(y) * width + x] = 0xFF000000u | (r << 16) | (g << 8) | b;
        }
    }
    int box_w = width / 4, box_h = height / 4;
    int box_x = (index * 97) % (width - box_w), box_y = (index * 53) % (height - box_h);
    uint32_t seed = 0x9E3779B9u * static_cast<uint32_t>(index + 1);
    for (int y = box_y; y < box_y + box_h; ++y) {
        for (int x = box_x; x < box_x + box_w; ++x) {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            pixels[static_cast<size_t>(y) * width + x] = 0xFF000000u | (seed & 0xFFFFFFu);
        }
    }
}

void nal_summary(Json& j, const uint8_t* data, size_t size) {
    j.boolean("annex_b", size >= 4 && data[0] == 0 && data[1] == 0 && (data[2] == 1 || (data[2] == 0 && data[3] == 1)));
    int sps_profile = -1, sps_constraints = -1, sps_level = -1, count = 0;
    j.begin_array("nal_types");
    for (size_t i = 0; i + 3 < size && count < 32; ++i) {
        if (data[i] != 0 || data[i + 1] != 0 || data[i + 2] != 1) continue;
        int type = data[i + 3] & 0x1F;
        j.integer(nullptr, type);
        ++count;
        if (type == 7 && i + 6 < size && sps_profile < 0) {
            sps_profile = data[i + 4];
            sps_constraints = data[i + 5];
            sps_level = data[i + 6];
        }
        i += 2;
    }
    j.end_array();
    if (sps_profile >= 0) {
        j.integer("sps_profile_idc", sps_profile);
        j.integer("sps_constraint_flags", sps_constraints);
        j.integer("sps_level_idc", sps_level);
    }
}

template <typename T>
void set_property(Json& j, amf::AMFComponent* component, const char* key, const wchar_t* name, const T& value) {
    result(j, key, component->SetProperty(name, value));
}

// Encoder settings that the probes switch on one at a time. Usage is always set first, outside this list,
// because setting it resets the other properties to the defaults of that usage.
enum class Prop {
    LowLatency,
    ProfileHigh,
    ProfileConstrainedBaseline,
    RateControlCbr,
    TargetBitrate,
    PeakBitrate,
    FrameRate,
    BPicNone,
    IdrPeriod,
    Color709,
    LimitedRange,
    VuiBt709,
    QualitySpeed,
    VbvTwoFrames,
    EnforceHrd,
    FillerData,
    PreencodeOff,
};

const char* prop_key(Prop p) {
    switch (p) {
    case Prop::LowLatency: return "low_latency_mode";
    case Prop::ProfileHigh: return "profile";
    case Prop::ProfileConstrainedBaseline: return "profile_constrained_baseline";
    case Prop::RateControlCbr: return "rate_control_method";
    case Prop::TargetBitrate: return "target_bitrate";
    case Prop::PeakBitrate: return "peak_bitrate";
    case Prop::FrameRate: return "frame_rate";
    case Prop::BPicNone: return "b_pic_pattern";
    case Prop::IdrPeriod: return "idr_period";
    case Prop::Color709: return "output_color_profile";
    case Prop::LimitedRange: return "full_range";
    case Prop::VuiBt709: return "vui_bt709";
    case Prop::QualitySpeed: return "quality_preset_speed";
    case Prop::VbvTwoFrames: return "vbv_two_frames";
    case Prop::EnforceHrd: return "enforce_hrd";
    case Prop::FillerData: return "filler_data";
    case Prop::PreencodeOff: return "preencode_off";
    }
    return "unknown";
}

void apply_prop(Json& j, amf::AMFComponent* e, Prop p, const RcsAmfProbeOptions& o) {
    const char* key = prop_key(p);
    switch (p) {
    case Prop::LowLatency: set_property(j, e, key, AMF_VIDEO_ENCODER_LOWLATENCY_MODE, true); break;
    case Prop::ProfileHigh:
        set_property(j, e, key, AMF_VIDEO_ENCODER_PROFILE, static_cast<amf_int64>(AMF_VIDEO_ENCODER_PROFILE_HIGH));
        break;
    case Prop::ProfileConstrainedBaseline:
        set_property(j, e, key, AMF_VIDEO_ENCODER_PROFILE,
                     static_cast<amf_int64>(AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_BASELINE));
        break;
    case Prop::RateControlCbr:
        set_property(j, e, key, AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD,
                     static_cast<amf_int64>(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR));
        break;
    case Prop::TargetBitrate:
        set_property(j, e, key, AMF_VIDEO_ENCODER_TARGET_BITRATE, static_cast<amf_int64>(o.bitrate));
        break;
    case Prop::PeakBitrate:
        set_property(j, e, key, AMF_VIDEO_ENCODER_PEAK_BITRATE, static_cast<amf_int64>(o.bitrate));
        break;
    case Prop::FrameRate:
        set_property(j, e, key, AMF_VIDEO_ENCODER_FRAMERATE, ::AMFConstructRate(static_cast<amf_uint32>(o.fps), 1));
        break;
    case Prop::BPicNone: set_property(j, e, key, AMF_VIDEO_ENCODER_B_PIC_PATTERN, static_cast<amf_int64>(0)); break;
    case Prop::IdrPeriod:
        set_property(j, e, key, AMF_VIDEO_ENCODER_IDR_PERIOD, static_cast<amf_int64>(o.fps * 10));
        break;
    case Prop::Color709:
        set_property(j, e, key, AMF_VIDEO_ENCODER_OUTPUT_COLOR_PROFILE,
                     static_cast<amf_int64>(AMF_VIDEO_CONVERTER_COLOR_PROFILE_709));
        break;
    case Prop::LimitedRange: set_property(j, e, key, AMF_VIDEO_ENCODER_FULL_RANGE_COLOR, false); break;
    case Prop::VuiBt709:
        j.begin_object(key);
        set_property(j, e, "transfer", AMF_VIDEO_ENCODER_OUTPUT_TRANSFER_CHARACTERISTIC,
                     static_cast<amf_int64>(AMF_COLOR_TRANSFER_CHARACTERISTIC_BT709));
        set_property(j, e, "primaries", AMF_VIDEO_ENCODER_OUTPUT_COLOR_PRIMARIES,
                     static_cast<amf_int64>(AMF_COLOR_PRIMARIES_BT709));
        j.end_object();
        break;
    case Prop::QualitySpeed:
        set_property(j, e, key, AMF_VIDEO_ENCODER_QUALITY_PRESET,
                     static_cast<amf_int64>(AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED));
        break;
    case Prop::VbvTwoFrames:
        set_property(j, e, key, AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE, static_cast<amf_int64>(o.bitrate * 2 / o.fps));
        break;
    case Prop::EnforceHrd: set_property(j, e, key, AMF_VIDEO_ENCODER_ENFORCE_HRD, true); break;
    case Prop::FillerData: set_property(j, e, key, AMF_VIDEO_ENCODER_FILLER_DATA_ENABLE, true); break;
    case Prop::PreencodeOff:
        set_property(j, e, key, AMF_VIDEO_ENCODER_PREENCODE_ENABLE,
                     static_cast<amf_int64>(AMF_VIDEO_ENCODER_PREENCODE_DISABLED));
        break;
    }
}

// The settings of the first probe, kept so its results stay comparable.
const std::vector<Prop> kProbeProps = {Prop::ProfileHigh, Prop::RateControlCbr, Prop::TargetBitrate,
                                       Prop::PeakBitrate,  Prop::FrameRate,      Prop::BPicNone,
                                       Prop::IdrPeriod,    Prop::Color709,       Prop::LimitedRange};

// Collects AMF trace lines, so a failed Init comes with the runtime's own explanation.
class TraceCollector : public amf::AMFTraceWriter {
public:
    static constexpr size_t kMaxLines = 200;

    void AMF_CDECL_CALL Write(const wchar_t* scope, const wchar_t* message) override {
        std::lock_guard<std::mutex> lock(mutex_);
        std::string line = utf8(scope);
        line += ": ";
        line += utf8(message);
        while (!line.empty() && (line.back() == '\n' || line.back() == '\r')) line.pop_back();
        lines_.push_back(std::move(line));
        if (lines_.size() > kMaxLines) {
            lines_.pop_front();
            ++dropped_;
        }
    }
    void AMF_CDECL_CALL Flush() override {}

    void clear() {
        std::lock_guard<std::mutex> lock(mutex_);
        lines_.clear();
        dropped_ = 0;
    }
    void write(Json& j, const char* key) {
        if (g_trace) g_trace->TraceFlush();
        std::lock_guard<std::mutex> lock(mutex_);
        j.begin_array(key);
        for (const std::string& line : lines_) j.string(nullptr, line);
        j.end_array();
        if (dropped_) j.integer("trace_dropped", static_cast<int64_t>(dropped_));
    }

private:
    std::mutex mutex_;
    std::deque<std::string> lines_;  // the last lines: the error comes at the end of Init
    size_t dropped_ = 0;
};

constexpr const wchar_t* kTraceWriterId = L"RcsCollector";

// Registers the collector at debug level for the scope and restores the previous level, also on exceptions.
class TraceScope {
public:
    explicit TraceScope(TraceCollector* collector) {
        if (!g_trace) return;
        previous_level_ = g_trace->GetGlobalLevel();
        g_trace->RegisterWriter(kTraceWriterId, collector, true);
        g_trace->SetWriterLevel(kTraceWriterId, AMF_TRACE_DEBUG);
        g_trace->SetGlobalLevel(AMF_TRACE_DEBUG);
    }
    ~TraceScope() {
        if (!g_trace) return;
        g_trace->SetGlobalLevel(previous_level_);
        g_trace->UnregisterWriter(kTraceWriterId);
    }
    TraceScope(const TraceScope&) = delete;
    TraceScope& operator=(const TraceScope&) = delete;

private:
    amf_int32 previous_level_ = AMF_TRACE_INFO;
};

struct Readback {
    const char* key;
    const wchar_t* name;
};

const Readback kReadback[] = {
    {"usage", AMF_VIDEO_ENCODER_USAGE},
    {"profile", AMF_VIDEO_ENCODER_PROFILE},
    {"profile_level", AMF_VIDEO_ENCODER_PROFILE_LEVEL},
    {"quality_preset", AMF_VIDEO_ENCODER_QUALITY_PRESET},
    {"rate_control_method", AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD},
    {"target_bitrate", AMF_VIDEO_ENCODER_TARGET_BITRATE},
    {"peak_bitrate", AMF_VIDEO_ENCODER_PEAK_BITRATE},
    {"vbv_buffer_size", AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE},
    {"frame_rate", AMF_VIDEO_ENCODER_FRAMERATE},
    {"b_pic_pattern", AMF_VIDEO_ENCODER_B_PIC_PATTERN},
    {"idr_period", AMF_VIDEO_ENCODER_IDR_PERIOD},
    {"header_insertion_spacing", AMF_VIDEO_ENCODER_HEADER_INSERTION_SPACING},
    {"low_latency_mode", AMF_VIDEO_ENCODER_LOWLATENCY_MODE},
    {"max_num_ref_frames", AMF_VIDEO_ENCODER_MAX_NUM_REFRAMES},
    {"cabac", AMF_VIDEO_ENCODER_CABAC_ENABLE},
    {"enforce_hrd", AMF_VIDEO_ENCODER_ENFORCE_HRD},
    {"filler_data", AMF_VIDEO_ENCODER_FILLER_DATA_ENABLE},
    {"preencode", AMF_VIDEO_ENCODER_PREENCODE_ENABLE},
    {"intra_refresh_mbs_per_slot", AMF_VIDEO_ENCODER_INTRA_REFRESH_NUM_MBS_PER_SLOT},
    {"query_timeout", AMF_VIDEO_ENCODER_QUERY_TIMEOUT},
    {"deblocking_filter", AMF_VIDEO_ENCODER_DE_BLOCKING_FILTER},
    {"output_color_profile", AMF_VIDEO_ENCODER_OUTPUT_COLOR_PROFILE},
};

struct EncodeStats {
    std::vector<double> convert_ms;
    std::vector<double> encode_ms;
    std::deque<double> pending;
    int64_t bytes = 0;
    int outputs = 0;
    int idr = 0;
    int i_frames = 0;
    int p_frames = 0;
    int b_frames = 0;
    int input_full = 0;
    int largest = 0;
    bool first_described = false;
};

void take_output(Json& j, EncodeStats& stats, amf::AMFData* data) {
    double done = now_ms();
    if (!stats.pending.empty()) {
        stats.encode_ms.push_back(done - stats.pending.front());
        stats.pending.pop_front();
    }
    amf::AMFBufferPtr buffer(data);
    if (!buffer) return;
    ++stats.outputs;
    auto size = static_cast<int>(buffer->GetSize());
    stats.bytes += size;
    if (size > stats.largest) stats.largest = size;
    amf::AMFVariant type;
    if (buffer->GetProperty(AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE, &type) == AMF_OK) {
        switch (type.int64Value) {
        case AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_IDR: ++stats.idr; break;
        case AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_I: ++stats.i_frames; break;
        case AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_P: ++stats.p_frames; break;
        case AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_B: ++stats.b_frames; break;
        default: break;
        }
    }
    if (!stats.first_described) {
        stats.first_described = true;
        j.begin_object("first_output");
        j.integer("bytes", size);
        nal_summary(j, static_cast<const uint8_t*>(buffer->GetNative()), static_cast<size_t>(size));
        j.end_object();
    }
}

// Pulls every output that is ready, waiting at most `wait_ms` for the first one.
AMF_RESULT collect_outputs(Json& j, EncodeStats& stats, amf::AMFComponent* encoder, double wait_ms) {
    double deadline = now_ms() + wait_ms;
    for (;;) {
        amf::AMFDataPtr data;
        AMF_RESULT r = encoder->QueryOutput(&data);
        if (data) {
            take_output(j, stats, data);
            continue;
        }
        if (r != AMF_OK && r != AMF_REPEAT) return r;
        if (stats.pending.empty() || now_ms() >= deadline) return AMF_OK;
        SwitchToThread();
    }
}

void write_series(Json& j, const char* key, const std::vector<double>& values) {
    j.begin_array(key);
    for (double v : values) j.number(nullptr, v);
    j.end_array();
}

struct RunConfig {
    int usage = AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY;
    int first_usage = -1;             // set before `usage` when not negative
    std::vector<Prop> before_usage;  // applied before `usage`
    std::vector<Prop> props;
    int width = 0;   // encoded size, the converter scales the sources to it
    int height = 0;
    int frames = 0;  // 0 stops after Init
    TraceCollector* trace = nullptr;
};

bool has_prop(const std::vector<Prop>& props, Prop p) {
    for (Prop q : props)
        if (q == p) return true;
    return false;
}

AMF_RESULT encode_run(Json& j, amf::AMFFactory* factory, amf::AMFContext* context,
                const std::vector<amf::AMFSurfacePtr>& sources, const RcsAmfProbeOptions& o, const RunConfig& c) {
    j.begin_object();
    j.integer("usage", c.usage);
    j.boolean("low_latency_mode_set", has_prop(c.props, Prop::LowLatency));
    j.begin_array("props");
    for (Prop p : c.props) j.string(nullptr, prop_key(p));
    j.end_array();
    j.integer("width", c.width);
    j.integer("height", c.height);
    if (c.trace) c.trace->clear();

    amf::AMFComponentPtr converter;
    AMF_RESULT r = factory->CreateComponent(context, AMFVideoConverter, &converter);
    result(j, "converter_create", r);
    if (r != AMF_OK) {
        j.end_object();
        return r;
    }
    converter->SetProperty(AMF_VIDEO_CONVERTER_MEMORY_TYPE, static_cast<amf_int64>(amf::AMF_MEMORY_DX11));
    converter->SetProperty(AMF_VIDEO_CONVERTER_OUTPUT_FORMAT, static_cast<amf_int64>(amf::AMF_SURFACE_NV12));
    converter->SetProperty(AMF_VIDEO_CONVERTER_OUTPUT_SIZE, ::AMFConstructSize(c.width, c.height));
    converter->SetProperty(AMF_VIDEO_CONVERTER_COLOR_PROFILE,
                           static_cast<amf_int64>(AMF_VIDEO_CONVERTER_COLOR_PROFILE_709));
    converter->SetProperty(AMF_VIDEO_CONVERTER_OUTPUT_COLOR_RANGE, static_cast<amf_int64>(AMF_COLOR_RANGE_STUDIO));
    r = converter->Init(amf::AMF_SURFACE_BGRA, o.width, o.height);
    result(j, "converter_init", r);
    if (r != AMF_OK) {
        j.end_object();
        return r;
    }

    amf::AMFComponentPtr encoder;
    r = factory->CreateComponent(context, AMFVideoEncoderVCE_AVC, &encoder);
    result(j, "encoder_create", r);
    if (r != AMF_OK) {
        converter->Terminate();
        j.end_object();
        return r;
    }
    j.begin_object("set");
    if (c.first_usage >= 0)
        set_property(j, encoder, "first_usage", AMF_VIDEO_ENCODER_USAGE, static_cast<amf_int64>(c.first_usage));
    if (!c.before_usage.empty()) {
        j.begin_object("before_usage");
        for (Prop p : c.before_usage) apply_prop(j, encoder, p, o);
        j.end_object();
    }
    set_property(j, encoder, "usage", AMF_VIDEO_ENCODER_USAGE, static_cast<amf_int64>(c.usage));
    for (Prop p : c.props) apply_prop(j, encoder, p, o);
    j.end_object();

    double init_started = now_ms();
    r = encoder->Init(amf::AMF_SURFACE_NV12, c.width, c.height);
    j.number("init_ms", now_ms() - init_started);
    result(j, "init", r);
    if (r != AMF_OK || c.frames == 0) {
        if (c.trace) c.trace->write(j, "trace");
        if (r == AMF_OK) encoder->Terminate();
        converter->Terminate();
        j.end_object();
        return r;
    }

    j.begin_object("readback");
    for (const Readback& item : kReadback) {
        amf::AMFVariant value;
        if (encoder->GetProperty(item.name, &value) == AMF_OK) variant(j, item.key, value);
    }
    j.end_object();

    EncodeStats stats;
    const char* failure = nullptr;
    AMF_RESULT failure_code = AMF_OK;
    double started = now_ms();
    for (int frame = 0; frame < c.frames && !failure; ++frame) {
        double t0 = now_ms();
        r = converter->SubmitInput(sources[static_cast<size_t>(frame) % sources.size()]);
        if (r != AMF_OK) {
            failure = "converter_submit";
            failure_code = r;
            break;
        }
        amf::AMFDataPtr nv12;
        for (;;) {
            r = converter->QueryOutput(&nv12);
            if (nv12) break;
            if (r != AMF_OK && r != AMF_REPEAT) break;
            if (now_ms() - t0 > kConvertTimeoutMs) break;
            SwitchToThread();
        }
        if (!nv12) {
            failure = "converter_output";
            failure_code = r;
            break;
        }
        double t1 = now_ms();
        stats.convert_ms.push_back(t1 - t0);

        for (;;) {
            r = encoder->SubmitInput(nv12);
            if (r != AMF_INPUT_FULL) break;
            ++stats.input_full;
            AMF_RESULT q = collect_outputs(j, stats, encoder, 5.0);
            if (q != AMF_OK) {
                r = q;
                break;
            }
            if (now_ms() - t1 > kDrainTimeoutMs) break;
        }
        if (r != AMF_OK) {
            failure = "encoder_submit";
            failure_code = r;
            break;
        }
        stats.pending.push_back(t1);
        r = collect_outputs(j, stats, encoder, kOutputWaitMs);
        if (r != AMF_OK) {
            failure = "encoder_output";
            failure_code = r;
        }
    }
    double submitted = now_ms();
    encoder->Drain();
    while (!stats.pending.empty() && now_ms() - submitted < kDrainTimeoutMs) {
        amf::AMFDataPtr data;
        r = encoder->QueryOutput(&data);
        if (data) {
            take_output(j, stats, data);
        } else if (r == AMF_EOF) {
            break;
        } else {
            SwitchToThread();
        }
    }

    if (failure) {
        j.string("failure", failure);
        result(j, "failure_result", failure_code);
    }
    j.integer("frames_submitted", static_cast<int64_t>(stats.convert_ms.size()));
    j.integer("outputs", stats.outputs);
    j.integer("lost_outputs", static_cast<int64_t>(stats.pending.size()));
    j.integer("idr", stats.idr);
    j.integer("i_frames", stats.i_frames);
    j.integer("p_frames", stats.p_frames);
    j.integer("b_frames", stats.b_frames);
    j.integer("input_full", stats.input_full);
    j.integer("bytes", stats.bytes);
    j.integer("largest_frame", stats.largest);
    j.number("wall_ms", submitted - started);
    write_series(j, "convert_ms", stats.convert_ms);
    write_series(j, "encode_ms", stats.encode_ms);
    if (c.trace && failure) c.trace->write(j, "trace");

    encoder->Terminate();
    converter->Terminate();
    j.end_object();
    return AMF_OK;
}

void upload_sources(Json& j, ID3D11Device* device, amf::AMFContext* context, const RcsAmfProbeOptions& o,
                    std::vector<amf::AMFSurfacePtr>& sources) {
    ID3D11DeviceContext* immediate = nullptr;
    device->GetImmediateContext(&immediate);
    std::vector<uint32_t> pixels(static_cast<size_t>(o.width) * o.height);
    j.begin_object("sources");
    for (int i = 0; i < kSourceFrames; ++i) {
        amf::AMFSurfacePtr surface;
        AMF_RESULT r = context->AllocSurface(amf::AMF_MEMORY_DX11, amf::AMF_SURFACE_BGRA, o.width, o.height, &surface);
        if (r != AMF_OK) {
            result(j, "alloc", r);
            break;
        }
        auto texture = static_cast<ID3D11Texture2D*>(surface->GetPlaneAt(0)->GetNative());
        if (i == 0) {
            D3D11_TEXTURE2D_DESC desc{};
            texture->GetDesc(&desc);
            j.integer("array_size", desc.ArraySize);
            j.integer("usage", desc.Usage);
            j.integer("bind_flags", desc.BindFlags);
            j.integer("misc_flags", desc.MiscFlags);
        }
        fill_pattern(pixels, o.width, o.height, i);
        immediate->UpdateSubresource(texture, 0, nullptr, pixels.data(), static_cast<UINT>(o.width) * 4, 0);
        sources.push_back(surface);
    }
    immediate->Flush();
    immediate->Release();
    j.integer("count", static_cast<int64_t>(sources.size()));
    j.end_object();
}

struct Runtime {
    amf::AMFFactory* factory = nullptr;
    amf::AMFContextPtr context;
};

// Loads amfrt64.dll, starts AMF and binds a context to the device. Writes every step into the open object.
bool open_runtime(Json& j, ID3D11Device* device, Runtime& rt) {
    j.string("header_version", version_text(AMF_FULL_VERSION));

    // System32 only: the runtime comes with the driver, and the search must not pick up a planted copy.
    HMODULE dll = LoadLibraryExW(AMF_DLL_NAME, nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    j.begin_object("dll");
    j.boolean("loaded", dll != nullptr);
    if (!dll) {
        j.integer("win32_error", GetLastError());
    } else {
        wchar_t path[MAX_PATH] = {};
        GetModuleFileNameW(dll, path, MAX_PATH);
        j.wstring("path", path);
        j.string("file_version", file_version(path));
    }
    j.end_object();
    if (!dll) return false;

    auto query_version = reinterpret_cast<AMFQueryVersion_Fn>(GetProcAddress(dll, AMF_QUERY_VERSION_FUNCTION_NAME));
    auto init = reinterpret_cast<AMFInit_Fn>(GetProcAddress(dll, AMF_INIT_FUNCTION_NAME));
    amf_uint64 runtime = 0;
    if (query_version) result(j, "query_version", query_version(&runtime));
    j.string("runtime_version", version_text(runtime));
    if (!init) {
        j.string("error", "AMFInit not exported");
        return false;
    }
    AMF_RESULT r = init(AMF_FULL_VERSION, &rt.factory);
    result(j, "init", r);
    if ((r != AMF_OK || !rt.factory) && runtime != 0) {
        rt.factory = nullptr;
        r = init(runtime, &rt.factory);
        result(j, "init_with_runtime_version", r);
    }
    if (r != AMF_OK || !rt.factory) return false;
    rt.factory->GetTrace(&g_trace);

    r = rt.factory->CreateContext(&rt.context);
    result(j, "create_context", r);
    if (r != AMF_OK) return false;
    r = rt.context->InitDX11(device, amf::AMF_DX11_1);
    result(j, "init_dx11_1", r);
    if (r != AMF_OK) {
        r = rt.context->InitDX11(device, amf::AMF_DX11_0);
        result(j, "init_dx11_0", r);
    }
    return r == AMF_OK;
}

void probe(Json& j, ID3D11Device* device, const RcsAmfProbeOptions& o) {
    j.begin_object();
    Runtime rt;
    if (!open_runtime(j, device, rt)) {
        j.end_object();
        return;
    }

    j.begin_array("components");
    for (const wchar_t* id : {AMFVideoEncoderVCE_AVC, AMFVideoEncoder_HEVC, AMFVideoEncoder_AV1, AMFVideoConverter}) {
        describe_component(j, rt.factory, rt.context, id);
    }
    j.end_array();

    std::vector<amf::AMFSurfacePtr> sources;
    upload_sources(j, device, rt.context, o, sources);
    if (!sources.empty()) {
        j.begin_array("encode_runs");
        for (int usage : {AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY, AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY,
                          AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY_HIGH_QUALITY, AMF_VIDEO_ENCODER_USAGE_WEBCAM,
                          AMF_VIDEO_ENCODER_USAGE_TRANSCODING, AMF_VIDEO_ENCODER_USAGE_HIGH_QUALITY}) {
            for (bool low_latency : {false, true}) {
                RunConfig c;
                c.usage = usage;
                if (low_latency) c.props.push_back(Prop::LowLatency);
                c.props.insert(c.props.end(), kProbeProps.begin(), kProbeProps.end());
                c.width = o.width;
                c.height = o.height;
                c.frames = o.frames;
                encode_run(j, rt.factory, rt.context, sources, o, c);
            }
        }
        j.end_array();
    }
    sources.clear();
    rt.context->Terminate();
    j.end_object();
}

// Name, access and value of every property right after Usage is set, before Init.
void usage_catalog(Json& j, const char* key, const Runtime& rt, int usage) {
    j.begin_object(key);
    amf::AMFComponentPtr encoder;
    AMF_RESULT r = rt.factory->CreateComponent(rt.context, AMFVideoEncoderVCE_AVC, &encoder);
    result(j, "create", r);
    if (r == AMF_OK) {
        result(j, "set_usage", encoder->SetProperty(AMF_VIDEO_ENCODER_USAGE, static_cast<amf_int64>(usage)));
        j.begin_object("properties");
        for (amf_size i = 0; i < encoder->GetPropertiesInfoCount(); ++i) {
            const amf::AMFPropertyInfo* info = nullptr;
            if (encoder->GetPropertyInfo(i, &info) != AMF_OK || !info || !info->name) continue;
            std::string name = utf8(info->name);
            if (name.rfind("TL", 0) == 0) continue;  // per-layer copies of the rate control settings
            j.begin_object(name.c_str());
            j.integer("access", info->accessType);
            amf::AMFVariant current;
            if (encoder->GetProperty(info->name, &current) == AMF_OK) variant(j, "current", current);
            j.end_object();
        }
        j.end_object();
        encoder->Terminate();
    }
    j.end_object();
}

// Finds which settings stop Ultra Low Latency from initialising, see Q-01.
void ull_probe(Json& j, ID3D11Device* device, const RcsAmfProbeOptions& o) {
    j.begin_object();
    Runtime rt;
    if (!open_runtime(j, device, rt)) {
        j.end_object();
        return;
    }

    TraceCollector trace;
    TraceScope trace_scope(&trace);

    usage_catalog(j, "catalog_ultra_low_latency", rt, AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY);
    usage_catalog(j, "catalog_low_latency", rt, AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY);

    std::vector<amf::AMFSurfacePtr> sources;
    upload_sources(j, device, rt.context, o, sources);
    if (!sources.empty()) {
        auto config = [&](int usage, std::vector<Prop> props, int frames, int width = 0, int height = 0) {
            RunConfig c;
            c.usage = usage;
            c.props = std::move(props);
            c.width = width ? width : o.width;
            c.height = height ? height : o.height;
            c.frames = frames;
            c.trace = &trace;
            return c;
        };
        const int ull = AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY;
        const int ll = AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY;

        j.begin_array("baseline");
        encode_run(j, rt.factory, rt.context, sources, o, config(ull, {}, o.frames));
        encode_run(j, rt.factory, rt.context, sources, o, config(ll, {}, o.frames));
        j.end_array();

        // Usage plus one property, Init only.
        const std::vector<Prop> candidates = {
            Prop::TargetBitrate, Prop::FrameRate,     Prop::ProfileHigh,  Prop::ProfileConstrainedBaseline,
            Prop::IdrPeriod,     Prop::BPicNone,      Prop::Color709,     Prop::LimitedRange,
            Prop::VuiBt709,      Prop::LowLatency,    Prop::QualitySpeed, Prop::VbvTwoFrames,
            Prop::RateControlCbr, Prop::PeakBitrate,  Prop::EnforceHrd,   Prop::FillerData,
        };
        j.begin_array("single");
        for (Prop p : candidates) encode_run(j, rt.factory, rt.context, sources, o, config(ull, {p}, 0));
        j.end_array();

        // Adds the properties in the order of the target settings and keeps each one Init accepts.
        const std::vector<Prop> order = {
            Prop::TargetBitrate, Prop::FrameRate,      Prop::ProfileHigh, Prop::IdrPeriod,
            Prop::BPicNone,      Prop::Color709,       Prop::LimitedRange, Prop::VuiBt709,
            Prop::LowLatency,    Prop::QualitySpeed,   Prop::VbvTwoFrames, Prop::RateControlCbr,
            Prop::PeakBitrate,   Prop::EnforceHrd,     Prop::FillerData,
        };
        std::vector<Prop> accepted;
        j.begin_array("greedy");
        for (Prop p : order) {
            std::vector<Prop> attempt = accepted;
            attempt.push_back(p);
            if (encode_run(j, rt.factory, rt.context, sources, o, config(ull, attempt, 0)) == AMF_OK) {
                accepted = std::move(attempt);
            }
        }
        j.end_array();
        j.begin_array("accepted");
        for (Prop p : accepted) j.string(nullptr, prop_key(p));
        j.end_array();

        // The accepted set with frames, and the same set on Low Latency for comparison.
        j.begin_array("final");
        encode_run(j, rt.factory, rt.context, sources, o, config(ull, accepted, o.frames));
        encode_run(j, rt.factory, rt.context, sources, o, config(ll, accepted, o.frames));
        j.end_array();

        // Smaller sizes with Usage only, in case the mode is limited by resolution.
        j.begin_array("sizes");
        for (auto size : {std::pair<int, int>{1280, 720}, std::pair<int, int>{640, 360}}) {
            encode_run(j, rt.factory, rt.context, sources, o, config(ull, {}, 0, size.first, size.second));
        }
        j.end_array();

        // Init of Ultra Low Latency reads RateControlPreanalysisEnable, which that usage makes inaccessible.
        // Tries to give the property a value before the usage switch, or to switch from Low Latency.
        j.begin_array("workarounds");
        {
            RunConfig c = config(ull, {}, o.frames);
            c.before_usage = {Prop::PreencodeOff};
            encode_run(j, rt.factory, rt.context, sources, o, c);
        }
        {
            RunConfig c = config(ull, {}, o.frames);
            c.first_usage = ll;
            encode_run(j, rt.factory, rt.context, sources, o, c);
        }
        {
            RunConfig c = config(ull, {}, o.frames);
            c.first_usage = ll;
            c.before_usage = {Prop::PreencodeOff};
            encode_run(j, rt.factory, rt.context, sources, o, c);
        }
        encode_run(j, rt.factory, rt.context, sources, o, config(ull, {Prop::PreencodeOff}, o.frames));
        j.end_array();
    }

    sources.clear();
    rt.context->Terminate();
    j.end_object();
}

char* copy_out(const std::string& text) {
    auto out = static_cast<char*>(malloc(text.size() + 1));
    if (out) memcpy(out, text.c_str(), text.size() + 1);
    return out;
}

template <typename F>
char* run_guarded(F&& body) {
    try {
        Json j;
        body(j);
        return copy_out(j.text);
    } catch (const std::exception& e) {
        Json j;
        j.begin_object();
        j.string("error", e.what());
        j.end_object();
        return copy_out(j.text);
    } catch (...) {
        return copy_out("{\"error\":\"unknown C++ exception\"}");
    }
}

}  // namespace

extern "C" char* rcs_amf_probe(void* d3d11_device, const RcsAmfProbeOptions* options) {
    return run_guarded([&](Json& j) { probe(j, static_cast<ID3D11Device*>(d3d11_device), *options); });
}

extern "C" char* rcs_amf_ull_probe(void* d3d11_device, const RcsAmfProbeOptions* options) {
    return run_guarded([&](Json& j) { ull_probe(j, static_cast<ID3D11Device*>(d3d11_device), *options); });
}

extern "C" void rcs_amf_free(char* text) { free(text); }
