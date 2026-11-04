// Helpers shared by the probe and the encoder: UTF-8, timing, a small JSON writer and AMF variant output.
#pragma once

#include <windows.h>

#include <cstdint>
#include <cstdio>
#include <string>
#include <vector>

#include "core/Variant.h"

namespace rcs_amf {

inline std::string utf8(const wchar_t* text) {
    if (!text) return {};
    int size = WideCharToMultiByte(CP_UTF8, 0, text, -1, nullptr, 0, nullptr, nullptr);
    if (size <= 1) return {};
    std::string out(static_cast<size_t>(size - 1), '\0');
    WideCharToMultiByte(CP_UTF8, 0, text, -1, out.data(), size, nullptr, nullptr);
    return out;
}

inline double now_ms() {
    static const double frequency = [] {
        LARGE_INTEGER f;
        QueryPerformanceFrequency(&f);
        return static_cast<double>(f.QuadPart);
    }();
    LARGE_INTEGER counter;
    QueryPerformanceCounter(&counter);
    return static_cast<double>(counter.QuadPart) * 1000.0 / frequency;
}

class Json {
public:
    std::string text;

    void begin_object(const char* key = nullptr) { open(key, '{'); }
    void end_object() { close('}'); }
    void begin_array(const char* key = nullptr) { open(key, '['); }
    void end_array() { close(']'); }
    void string(const char* key, const std::string& value) { prefix(key); quote(value); }
    void wstring(const char* key, const wchar_t* value) { string(key, utf8(value)); }
    void integer(const char* key, int64_t value) { prefix(key); text += std::to_string(value); }
    void boolean(const char* key, bool value) { prefix(key); text += value ? "true" : "false"; }
    void null(const char* key) { prefix(key); text += "null"; }
    void number(const char* key, double value) {
        char buffer[32];
        snprintf(buffer, sizeof buffer, "%.4f", value);
        prefix(key);
        text += buffer;
    }

private:
    std::vector<bool> first_;

    void open(const char* key, char bracket) {
        prefix(key);
        text += bracket;
        first_.push_back(true);
    }
    void close(char bracket) {
        text += bracket;
        first_.pop_back();
    }
    void prefix(const char* key) {
        if (!first_.empty()) {
            if (!first_.back()) text += ',';
            first_.back() = false;
        }
        if (key) {
            quote(key);
            text += ':';
        }
    }
    void quote(const std::string& value) {
        text += '"';
        for (unsigned char c : value) {
            switch (c) {
            case '"': text += "\\\""; break;
            case '\\': text += "\\\\"; break;
            case '\n': text += "\\n"; break;
            case '\r': text += "\\r"; break;
            case '\t': text += "\\t"; break;
            default:
                if (c < 0x20) {
                    char buffer[8];
                    snprintf(buffer, sizeof buffer, "\\u%04x", c);
                    text += buffer;
                } else {
                    text += static_cast<char>(c);
                }
            }
        }
        text += '"';
    }
};


inline void variant(Json& j, const char* key, const amf::AMFVariantStruct& v) {
    char buffer[96];
    switch (v.type) {
    case amf::AMF_VARIANT_EMPTY: j.null(key); return;
    case amf::AMF_VARIANT_BOOL: j.boolean(key, v.boolValue != 0); return;
    case amf::AMF_VARIANT_INT64: j.integer(key, v.int64Value); return;
    case amf::AMF_VARIANT_DOUBLE: j.number(key, v.doubleValue); return;
    case amf::AMF_VARIANT_FLOAT: j.number(key, v.floatValue); return;
    case amf::AMF_VARIANT_STRING: j.string(key, v.stringValue ? v.stringValue : ""); return;
    case amf::AMF_VARIANT_WSTRING: j.wstring(key, v.wstringValue); return;
    case amf::AMF_VARIANT_SIZE:
        snprintf(buffer, sizeof buffer, "%dx%d", v.sizeValue.width, v.sizeValue.height);
        break;
    case amf::AMF_VARIANT_RATE:
        snprintf(buffer, sizeof buffer, "%u/%u", v.rateValue.num, v.rateValue.den);
        break;
    case amf::AMF_VARIANT_RATIO:
        snprintf(buffer, sizeof buffer, "%u:%u", v.ratioValue.num, v.ratioValue.den);
        break;
    case amf::AMF_VARIANT_RECT:
        snprintf(buffer, sizeof buffer, "%d,%d,%d,%d", v.rectValue.left, v.rectValue.top, v.rectValue.right,
                 v.rectValue.bottom);
        break;
    case amf::AMF_VARIANT_POINT:
        snprintf(buffer, sizeof buffer, "%d,%d", v.pointValue.x, v.pointValue.y);
        break;
    default:
        snprintf(buffer, sizeof buffer, "<variant type %d>", static_cast<int>(v.type));
        break;
    }
    j.string(key, buffer);
}

}  // namespace rcs_amf
