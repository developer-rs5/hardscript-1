#ifndef HS_RUNTIME_VALUE_HPP
#define HS_RUNTIME_VALUE_HPP
#include <algorithm>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <condition_variable>
#include <csignal>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#ifdef __GLIBC__
#include <malloc.h>
#endif
#include <fcntl.h>
#include <fstream>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <set>
#include <sstream>
#include <string>
#include <string_view>
#include <thread>
#include <unordered_map>
#include <unistd.h>
#include <utility>
#include <vector>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <dirent.h>
#include <sys/stat.h>
#include <poll.h>
#include <random>

#ifndef HS_NOTHREAD
#include <thread>
#endif

#define HS_VERSION_STRING "0.1.0"

namespace hs {

// ===========================================================================
// Value
// ===========================================================================
struct Val {
    enum class T { Nil, Int, Flt, Bool, Str, Arr, Obj };
    T t = T::Nil;
    int64_t iv = 0;
    double fv = 0;
    bool bv = false;
    std::string sv;
    std::vector<Val> arr;
    std::vector<std::pair<std::string, Val>> obj;

    Val() = default;
    static Val nil() { return Val(); }
    static Val int_(int64_t v) { Val x; x.t = T::Int; x.iv = v; return x; }
    static Val flt(double v) { Val x; x.t = T::Flt; x.fv = v; return x; }
    static Val boolean(bool v) { Val x; x.t = T::Bool; x.bv = v; return x; }
    static Val text(const std::string& v) { Val x; x.t = T::Str; x.sv = v; return x; }
    static Val list(std::vector<Val> v) { Val x; x.t = T::Arr; x.arr = std::move(v); return x; }
    static Val object(std::vector<std::pair<std::string, Val>> v) { Val x; x.t = T::Obj; x.obj = std::move(v); return x; }

    bool is_nil() const { return t == T::Nil; }
    bool is_int() const { return t == T::Int; }
    bool is_flt() const { return t == T::Flt; }
    bool is_num() const { return t == T::Int || t == T::Flt; }
    bool is_bool() const { return t == T::Bool; }
    bool is_str() const { return t == T::Str; }
    bool is_arr() const { return t == T::Arr; }
    bool is_obj() const { return t == T::Obj; }

    double num() const { return is_flt() ? fv : (double)iv; }
    int64_t as_int() const { return is_flt() ? (int64_t)fv : iv; }

    Val& get(const std::string& k) {
        if (t != T::Obj) { *this = object({}); }
        for (auto& kv : obj) if (kv.first == k) return kv.second;
        obj.emplace_back(k, Val::nil());
        return obj.back().second;
    }
    const Val* find(const std::string& k) const {
        for (auto& kv : obj) if (kv.first == k) return &kv.second;
        return nullptr;
    }
    Val* find_mut(const std::string& k) {
        for (auto& kv : obj) if (kv.first == k) return &kv.second;
        return nullptr;
    }
    bool has(const std::string& k) const { return find(k) != nullptr; }
    Val& at(size_t n) {
        if (t != T::Arr) throw std::runtime_error("value is not a list");
        if (n >= arr.size()) throw std::runtime_error("list index out of range");
        return arr[n];
    }
    const Val& atc(size_t n) const {
        if (t != T::Arr) throw std::runtime_error("value is not a list");
        if (n >= arr.size()) throw std::runtime_error("list index out of range");
        return arr[n];
    }
    size_t size() const {
        switch (t) {
            case T::Arr: return arr.size();
            case T::Obj: return obj.size();
            case T::Str: return sv.size();
            default: return 0;
        }
    }
    void push(Val v) { if (t != T::Arr) { *this = list({}); } arr.push_back(std::move(v)); }
    void set(const std::string& k, Val v) {
        if (t != T::Obj) { *this = object({}); }
        for (auto& kv : obj) if (kv.first == k) { kv.second = std::move(v); return; }
        obj.emplace_back(k, std::move(v));
    }
    bool truthy() const {
        switch (t) {
            case T::Nil: return false;
            case T::Int: return iv != 0;
            case T::Flt: return fv != 0;
            case T::Bool: return bv;
            case T::Str: return !sv.empty();
            case T::Arr: return !arr.empty();
            case T::Obj: return !obj.empty();
        }
        return false;
    }
    std::string kind_name() const {
        switch (t) {
            case T::Nil: return "Nil";
            case T::Int: return "Int";
            case T::Flt: return "Float";
            case T::Bool: return "Bool";
            case T::Str: return "Text";
            case T::Arr: return "List";
            case T::Obj: return "Map";
        }
        return "?";
    }
    std::string to_json() const;
    std::string to_text_dbg() const;
};

// ===========================================================================
// JSON
// ===========================================================================
inline size_t json_escaped_length(std::string_view sv) {
    size_t n = 0;
    for (unsigned char c : sv) {
        switch (c) {
            case '"': case '\\': case '\b': case '\f': case '\n': case '\r': case '\t':
                n += 2;
                break;
            default:
                n += c < 0x20 ? 6 : 1;
                break;
        }
    }
    return n;
}

// Allocation-free serializer: writes straight into any sink exposing
// append(const char*, size_t) — Arena::Str or std::string — with no
// temporary strings, no ostringstream, no per-byte snprintf.
template <typename Sink>
inline void json_escape_to(std::string_view sv, Sink& out) {
    static const char HEX[] = "0123456789abcdef";
    for (unsigned char c : sv) {
        switch (c) {
            case '"': out.append("\\\"", 2); break;
            case '\\': out.append("\\\\", 2); break;
            case '\b': out.append("\\b", 2); break;
            case '\f': out.append("\\f", 2); break;
            case '\n': out.append("\\n", 2); break;
            case '\r': out.append("\\r", 2); break;
            case '\t': out.append("\\t", 2); break;
            default:
                if (c < 0x20) {
                    char b[8];
                    b[0] = '\\'; b[1] = 'u'; b[2] = '0'; b[3] = '0';
                    b[4] = HEX[c >> 4]; b[5] = HEX[c & 0xF];
                    out.append(b, 6);
                } else {
                    out.append((const char*)&c, 1);
                }
        }
    }
}

template <typename Sink>
inline void json_append_int(Sink& out, int64_t v) {
    char b[24];
    unsigned long long u = v < 0 ? 0ULL - (unsigned long long)v : (unsigned long long)v;
    size_t n = 0;
    do { b[n++] = char('0' + u % 10); u /= 10; } while (u);
    if (v < 0) b[n++] = '-';
    while (n) out.append(&b[--n], 1);
}

// Mirrors the previous %.15g behaviour (plus a trailing ".0" when the short
// form has no '.', 'e' or 'E'), written from a stack buffer.
template <typename Sink>
inline void json_append_double(Sink& out, double d) {
    char b[48];
    int n = std::snprintf(b, sizeof b, "%.15g", d);
    if (n < 0) n = 0;
    int dot = 0;
    for (int i = 0; i < n; i++) {
        char z = b[i];
        if (z == '.' || z == 'e' || z == 'E') { dot = 1; break; }
    }
    out.append(b, (size_t)n);
    if (!dot) out.append(".0", 2);
}

// Exact serialized byte count of `v` without writing it: used to fill
// Content-Length before streaming a JSON body into the same socket buffer.
inline size_t json_size(const Val& v) {
    switch (v.t) {
        case Val::T::Nil: return 4;
        case Val::T::Bool: return v.bv ? 4 : 5;
        case Val::T::Int: {
            unsigned long long u = v.iv < 0 ? 0ULL - (unsigned long long)v.iv : (unsigned long long)v.iv;
            size_t n = v.iv < 0 ? 1 : 0;
            do { n++; u /= 10; } while (u);
            return n;
        }
        case Val::T::Flt: {
            char b[48];
            int n = std::snprintf(b, sizeof b, "%.15g", v.fv);
            if (n < 0) n = 0;
            int dot = 0;
            for (int i = 0; i < n; i++) if (b[i] == '.' || b[i] == 'e' || b[i] == 'E') dot = 1;
            return (size_t)n + (dot ? 0 : 2);
        }
        case Val::T::Str: return 2 + json_escaped_length(std::string_view(v.sv));
        case Val::T::Arr: {
            size_t n = 1;
            for (size_t k = 0; k < v.arr.size(); k++) { if (k) n++; n += json_size(v.arr[k]); }
            return n + 1;
        }
        case Val::T::Obj: {
            size_t n = 1;
            for (size_t k = 0; k < v.obj.size(); k++) {
                if (k) n++;
                n += 1 + json_escaped_length(std::string_view(v.obj[k].first)) + 2;
                n += json_size(v.obj[k].second);
            }
            return n + 1;
        }
    }
    return 0;
}

template <typename Sink>
inline void val_to_json(const Val& v, Sink& out) {
    switch (v.t) {
        case Val::T::Nil: out.append("null", 4); break;
        case Val::T::Int: json_append_int(out, v.iv); break;
        case Val::T::Flt: json_append_double(out, v.fv); break;
        case Val::T::Bool: out.append(v.bv ? "true" : "false", v.bv ? 4 : 5); break;
        case Val::T::Str: out.append("\"", 1); json_escape_to(std::string_view(v.sv), out); out.append("\"", 1); break;
        case Val::T::Arr: {
            out.append("[", 1);
            for (size_t k = 0; k < v.arr.size(); k++) { if (k) out.append(",", 1); val_to_json(v.arr[k], out); }
            out.append("]", 1);
            break;
        }
        case Val::T::Obj: {
            out.append("{", 1);
            for (size_t k = 0; k < v.obj.size(); k++) {
                if (k) out.append(",", 1);
                out.append("\"", 1);
                json_escape_to(std::string_view(v.obj[k].first), out);
                out.append("\":", 2);
                val_to_json(v.obj[k].second, out);
            }
            out.append("}", 1);
            break;
        }
    }
}

inline std::string to_json(const Val& v) {
    std::string out;
    out.reserve(json_size(v));
    val_to_json<std::string>(v, out);
    return out;
}
inline std::string Val::to_json() const { return ::hs::to_json(*this); }
inline std::string to_text_dbg(const Val& v) {
    switch (v.t) {
        case Val::T::Nil: return "nil";
        case Val::T::Int: return std::to_string(v.iv);
        case Val::T::Flt: { std::ostringstream o; o.precision(15); o << v.fv; return o.str(); }
        case Val::T::Bool: return v.bv ? "true" : "false";
        case Val::T::Str: return v.sv;
        default: return v.to_json();
    }
}
inline std::string Val::to_text_dbg() const { return ::hs::to_text_dbg(*this); }

// ===========================================================================
// ms2.0 Value engine — foundation layer (additive; hs::Val remains the
// default value type until the Phase-2 rewrite. Everything here is plain
// C++17 and provides the ValueKind/tagged-union/SSO/small-vector/immutable-
// constant-pool piece used by the escape-analysis report).
// ===========================================================================

// Tagged-union value engine kind set (Phase 2, ms2.1). Primitives (Nil,
// Bool, Int, Float) live inline inside `Value` with no heap; heap is used
// only for String/Array/Object/Bytes/Function payloads.
enum class ValueKind : uint8_t { Nil, Bool, Int, Float, String, Array, Object, Function, Bytes };

class Value;
struct ValueVec;
struct ValueObj;

class Value {
public:
    // Small String Optimization: strings up to kSso bytes live in the value
    // itself (never allocate); longer strings own a heap std::string. 24 fills
    // the 32-byte value layout exactly (22-24 per the ms2.2 spec).
    static constexpr size_t kSso = 24;

    Value() = default;
    ~Value() { destroy(); }
    Value(const Value& o) { copy_from(o); }
    Value& operator=(const Value& o) { if (this != &o) { destroy(); copy_from(o); } return *this; }
    Value(Value&& o) noexcept { steal(o); }
    Value& operator=(Value&& o) noexcept { if (this != &o) { destroy(); steal(o); } return *this; }

    // ---- tagged-union constructors: primitives inline, no allocation.
    static Value nil() { return Value(); }
    static Value boolean(bool b) { Value v; v.kind_ = ValueKind::Bool; v.u_.b = b; return v; }
    static Value i64(int64_t i) { Value v; v.kind_ = ValueKind::Int; v.u_.i = i; return v; }
    static Value f64(double f) { Value v; v.kind_ = ValueKind::Float; v.u_.f = f; return v; }
    static Value str(std::string_view s) {
        Value v;
        v.kind_ = ValueKind::String;
        if (s.size() <= kSso) { v.fl_ = kInline; v.sso_n_ = (uint8_t)s.size(); std::memcpy(v.u_.sso, s.data(), s.size()); }
        else { v.fl_ = kOwn; v.u_.s = new std::string(s); }
        return v;
    }
    // Non-owning string (points into a const pool / request buffer).
    static Value str_view(std::string_view s) { Value v; v.kind_ = ValueKind::String; v.fl_ = kView; v.u_.view = {s.data(), s.size()}; return v; }
    // Non-owning bytes view.
    static Value bytes(std::string_view s) { Value v; v.kind_ = ValueKind::Bytes; v.fl_ = kView; v.u_.view = {s.data(), s.size()}; return v; }
    // Callable: raw function pointer + a name interned into the const pool
    // (stable address, zero per-value allocation). Out-of-line below.
    static Value fn(void* fp, std::string_view name);
    static Value arr();
    static Value obj();

    ValueKind kind() const { return kind_; }
    bool is(ValueKind k) const { return kind_ == k; }

    int64_t as_i64() const { return kind_ == ValueKind::Int ? u_.i : (kind_ == ValueKind::Float ? (int64_t)u_.f : 0); }
    double as_f64() const { return kind_ == ValueKind::Float ? u_.f : (double)(kind_ == ValueKind::Int ? u_.i : 0); }
    bool as_bool() const { return kind_ == ValueKind::Bool ? u_.b : false; }

    // UTF-8 safe: SSO copies raw bytes without interpreting them, so a
    // multibyte sequence is never truncated mid-codepoint on the inline path.
    std::string_view as_str() const {
        if (kind_ != ValueKind::String && kind_ != ValueKind::Bytes) return {};
        if (fl_ & kOwn) return std::string_view(*u_.s);
        if (fl_ & kInline) return std::string_view(u_.sso, sso_n_);
        if (fl_ & kView) return u_.view.d ? std::string_view(u_.view.d, u_.view.n) : std::string_view();
        return std::string_view();
    }

    void* as_fn() const { return kind_ == ValueKind::Function ? u_.fn.fp : nullptr; }
    const char* fn_name() const { return kind_ == ValueKind::Function ? u_.fn.name : ""; }

    // Arrays / objects (bodies live after the ValueVec/ValueObj boxes).
    size_t size() const;
    const Value* arr_at(size_t i) const;
    const Value* obj_at(size_t i) const;
    const std::string& obj_key(size_t i) const;
    const Value* find(const std::string_view k) const;
    void push(Value&& v);
    void set(std::string key, Value v);

private:
    enum ST : uint8_t { kOwn = 0x01, kInline = 0x02, kView = 0x04 };

    union U {
        U() {}
        int64_t i;
        double f;
        bool b;
        char sso[kSso];
        std::string* s;                           // owned long string
        struct { const char* d; size_t n; } view; // non-owning view
        struct { void* fp; const char* name; } fn; // callable: fn ptr + pooled name
        ValueVec* v;
        ValueObj* o;
    };

    ValueKind kind_ = ValueKind::Nil;
    uint8_t fl_ = 0;   // ST storage selector (string only; containers use kind_)
    uint8_t sso_n_ = 0;
    uint8_t pad_ = 0;
    U u_;

    // Out-of-line (need complete ValueVec/ValueObj): see after the boxes.
    void destroy() noexcept;
    void copy_from(const Value& o);
    void steal(Value& o) noexcept {
        kind_ = o.kind_; fl_ = o.fl_; sso_n_ = o.sso_n_; u_ = o.u_;
        o.kind_ = ValueKind::Nil; o.fl_ = 0; o.sso_n_ = 0;
    }
};

// Owned array box: inline slots for the common small case (0 heap growth),
// falls back to a std::vector only past kSmall elements. Lives behind a
// pointer inside Value so `Value` stays copyable at any depth.
struct ValueVec {
    static constexpr size_t kSmall = 4;
    Value small_[kSmall];
    size_t n_ = 0;
    std::vector<Value> heap_;
    Value* data() { return n_ <= kSmall ? small_ : heap_.data(); }
    const Value* data() const { return n_ <= kSmall ? small_ : heap_.data(); }
    size_t size() const { return n_; }
};

// Owned object box with the same small-vector behaviour; keys use
// std::string's own SSO (short keys never allocate).
struct ValueObj {
    static constexpr size_t kSmall = 4;
    std::pair<std::string, Value> small_[kSmall];
    size_t n_ = 0;
    std::vector<std::pair<std::string, Value>> heap_;
    std::pair<std::string, Value>* data() { return n_ <= kSmall ? small_ : heap_.data(); }
    const std::pair<std::string, Value>* data() const { return n_ <= kSmall ? small_ : heap_.data(); }
    size_t size() const { return n_; }
};

// Out-of-line members that need the complete box types.
inline void Value::destroy() noexcept {
    if (fl_ & kOwn) delete u_.s;
    if (kind_ == ValueKind::Array) delete u_.v;
    if (kind_ == ValueKind::Object) delete u_.o;
    kind_ = ValueKind::Nil; fl_ = 0; sso_n_ = 0;
}
inline Value Value::arr() { Value v; v.kind_ = ValueKind::Array; v.u_.v = new ValueVec(); return v; }
inline Value Value::obj() { Value v; v.kind_ = ValueKind::Object; v.u_.o = new ValueObj(); return v; }
inline void Value::copy_from(const Value& o) {
    kind_ = o.kind_; fl_ = o.fl_; sso_n_ = o.sso_n_;
    switch (kind_) {
        case ValueKind::String:
            if (fl_ & kOwn) u_.s = new std::string(*o.u_.s);
            else if (fl_ & kInline) { std::memcpy(u_.sso, o.u_.sso, sso_n_); }
            else if (fl_ & kView) u_.view = o.u_.view;
            else u_ = o.u_;                       // fully-initialized empty fallback
            break;
        case ValueKind::Bytes: u_.view = o.u_.view; break;
        case ValueKind::Array: u_.v = new ValueVec(*o.u_.v); break;
        case ValueKind::Object: u_.o = new ValueObj(*o.u_.o); break;
        default: u_ = o.u_; break;  // scalar copy (incl. Function: fp + pooled name)
    }
}
inline size_t Value::size() const { return kind_ == ValueKind::Array ? u_.v->size() : (kind_ == ValueKind::Object ? u_.o->size() : 0); }
inline const Value* Value::arr_at(size_t i) const { return kind_ == ValueKind::Array && i < u_.v->size() ? u_.v->data() + i : nullptr; }
inline const Value* Value::obj_at(size_t i) const { return kind_ == ValueKind::Object && i < u_.o->size() ? &u_.o->data()[i].second : nullptr; }
inline const std::string& Value::obj_key(size_t i) const { return u_.o->data()[i].first; }
inline const Value* Value::find(const std::string_view k) const {
    if (kind_ != ValueKind::Object) return nullptr;
    for (size_t i = 0; i < u_.o->size(); i++)
        if (u_.o->data()[i].first == k) return &u_.o->data()[i].second;
    return nullptr;
}
inline void Value::push(Value&& v) {
    if (kind_ != ValueKind::Array) { destroy(); kind_ = ValueKind::Array; u_.v = new ValueVec(); }
    ValueVec& box = *u_.v;
    if (box.n_ < ValueVec::kSmall) { box.small_[box.n_++] = std::move(v); return; }
    if (box.heap_.empty()) {                       // first spill: migrate inline slots
        for (size_t i = 0; i < ValueVec::kSmall; i++) box.heap_.push_back(std::move(box.small_[i]));
        box.heap_.push_back(std::move(v));
        box.n_ = ValueVec::kSmall + 1;
    } else {
        box.heap_.push_back(std::move(v));
        box.n_++;
    }
}
inline void Value::set(std::string key, Value v) {
    if (kind_ != ValueKind::Object) { destroy(); kind_ = ValueKind::Object; u_.o = new ValueObj(); }
    ValueObj& box = *u_.o;
    for (size_t i = 0; i < box.size(); i++)
        if (box.data()[i].first == key) { box.data()[i].second = std::move(v); return; }
    if (box.n_ < ValueObj::kSmall) { box.small_[box.n_++] = {std::move(key), std::move(v)}; return; }
    if (box.heap_.empty()) {
        for (size_t i = 0; i < ValueObj::kSmall; i++) box.heap_.push_back(std::move(box.small_[i]));
        box.heap_.push_back({std::move(key), std::move(v)});
        box.n_ = ValueObj::kSmall + 1;
    } else {
        box.heap_.push_back({std::move(key), std::move(v)});
        box.n_++;
    }
}

inline const char* value_kind_name(ValueKind k) {
    switch (k) {
        case ValueKind::Nil: return "Nil";
        case ValueKind::Bool: return "Bool";
        case ValueKind::Int: return "Int";
        case ValueKind::Float: return "Float";
        case ValueKind::String: return "String";
        case ValueKind::Array: return "Array";
        case ValueKind::Object: return "Object";
        case ValueKind::Function: return "Function";
        case ValueKind::Bytes: return "Bytes";
    }
    return "?";
}

// One-line layout summary used by the layout QA fixture and the final
// value-memory report: sizeof/alignof, the SSO threshold and the container
// inline-slot counts. Kept as a runtime string so the compiler can embed it
// in a generated report without re-deriving the C++ data structure.
inline std::string value_layout_report() {
    char b[192];
    int n = std::snprintf(
        b, sizeof b,
        "sizeof(Value)=%zu alignof(Value)=%zu sso=%zu kinds=%zu array_inline=%zu object_inline=%zu",
        sizeof(Value), alignof(Value), Value::kSso,
        (size_t)ValueKind::Bytes - (size_t)ValueKind::Nil + 1,
        ValueVec::kSmall, ValueObj::kSmall);
    return std::string(b, (size_t)n);
}

inline size_t value_size(const Value& v) {
    switch (v.kind()) {
        case ValueKind::Nil: return 4;
        case ValueKind::Bool: return v.as_bool() ? 4 : 5;
        case ValueKind::Int: {
            int64_t x = v.as_i64();
            unsigned long long u = x < 0 ? 0ULL - (unsigned long long)x : (unsigned long long)x;
            size_t n = x < 0 ? 1 : 0;
            do { n++; u /= 10; } while (u);
            return n;
        }
        case ValueKind::Float: {
            char b[48];
            int n = std::snprintf(b, sizeof b, "%.15g", v.as_f64());
            if (n < 0) n = 0;
            int dot = 0;
            for (int i = 0; i < n; i++) if (b[i] == '.' || b[i] == 'e' || b[i] == 'E') dot = 1;
            return (size_t)n + (dot ? 0 : 2);
        }
        case ValueKind::String:
        case ValueKind::Bytes: return 2 + json_escaped_length(v.as_str());
        case ValueKind::Function: return 4;  // not serializable; serializes as null
        case ValueKind::Array: {
            size_t n = 1;
            for (size_t k = 0; k < v.size(); k++) { if (k) n++; n += value_size(*v.arr_at(k)); }
            return n + 1;
        }
        case ValueKind::Object: {
            size_t n = 1;
            for (size_t k = 0; k < v.size(); k++) {
                if (k) n++;
                n += 1 + json_escaped_length(std::string_view(v.obj_key(k))) + 2;
                n += value_size(*v.obj_at(k));
            }
            return n + 1;
        }
    }
    return 0;
}

template <typename Sink>
inline void value_to_json(const Value& v, Sink& out) {
    switch (v.kind()) {
        case ValueKind::Nil:
        case ValueKind::Function: out.append("null", 4); break;  // fn: not serializable
        case ValueKind::Int: json_append_int(out, v.as_i64()); break;
        case ValueKind::Float: json_append_double(out, v.as_f64()); break;
        case ValueKind::Bool: out.append(v.as_bool() ? "true" : "false", v.as_bool() ? 4 : 5); break;
        case ValueKind::String:
        case ValueKind::Bytes:
            out.append("\"", 1);
            json_escape_to(v.as_str(), out);
            out.append("\"", 1);
            break;
        case ValueKind::Array: {
            out.append("[", 1);
            for (size_t k = 0; k < v.size(); k++) { if (k) out.append(",", 1); value_to_json(*v.arr_at(k), out); }
            out.append("]", 1);
            break;
        }
        case ValueKind::Object: {
            out.append("{", 1);
            for (size_t k = 0; k < v.size(); k++) {
                if (k) out.append(",", 1);
                out.append("\"", 1);
                json_escape_to(std::string_view(v.obj_key(k)), out);
                out.append("\":", 2);
                value_to_json(*v.obj_at(k), out);
            }
            out.append("}", 1);
            break;
        }
    }
}

inline std::string value_to_json_string(const Value& v) {
    std::string out;
    out.reserve(value_size(v));
    value_to_json<std::string>(v, out);
    return out;
}

// ---- ms2.0 immutable constant pool ---------------------------------------
// Append-only string interning with stable addresses (thread-safe reads via
// the returned const char*; writes take a lock). Used at boot to land
// immutable reason/literal strings outside per-request heaps.
class ConstPool {
public:
    const char* intern(std::string_view s) {
        std::lock_guard<std::mutex> lk(m_);
        auto it = idx_.find(s);
        if (it != idx_.end()) return it->second;
        auto store = std::make_unique<std::string>(s);
        const char* p = store->data();
        idx_.emplace(std::string_view(*store), p);
        by_id_.push_back(p);
        strings_.push_back(std::move(store));
        return p;
    }
    std::string_view get(size_t id) const {
        if (id < by_id_.size()) return by_id_[id];
        return {};
    }
    const char* cstr(size_t id) const {
        if (id < by_id_.size()) return by_id_[id];
        return "";
    }
    Value v_str(std::string_view s) { return Value::str_view(intern(s)); }
    size_t size() const { return by_id_.size(); }
    void freeze() {
        std::lock_guard<std::mutex> lk(m_);
        idx_.clear(); // immutable now: interned pointers stay stable w/o the map
    }
private:
    mutable std::mutex m_;
    std::vector<std::unique_ptr<std::string>> strings_;          // stable addresses
    std::unordered_map<std::string_view, const char*> idx_;       // build-time dedup
    std::vector<const char*> by_id_;                              // post-freeze reads
};

inline ConstPool& const_pool() { static ConstPool p; return p; }

// Function values intern their name into the const pool so the stored
// `const char*` is stable for the process lifetime and never allocated per
// value. Defined here (not in the class) because it needs the pool.
inline Value Value::fn(void* fp, std::string_view name) {
    Value v;
    v.kind_ = ValueKind::Function;
    v.u_.fn.fp = fp;
    v.u_.fn.name = const_pool().intern(name);
    return v;
}

// ---- ms2.0 bridge: hs::Val <-> Value --------------------------------
inline Value value_from_val(const Val& v) {
    switch (v.t) {
        case Val::T::Nil: return Value::nil();
        case Val::T::Bool: return Value::boolean(v.bv);
        case Val::T::Int: return Value::i64(v.iv);
        case Val::T::Flt: return Value::f64(v.fv);
        case Val::T::Str: return Value::str(std::string_view(v.sv));
        case Val::T::Arr: {
            Value a = Value::arr();
            for (auto& e : v.arr) a.push(value_from_val(e));
            return a;
        }
        case Val::T::Obj: {
            Value o = Value::obj();
            for (auto& kv : v.obj) o.set(kv.first, value_from_val(kv.second));
            return o;
        }
    }
    return Value::nil();
}

inline Val val_from_value(const Value& v) {
    switch (v.kind()) {
        case ValueKind::Nil: return Val::nil();
        case ValueKind::Bool: return Val::boolean(v.as_bool());
        case ValueKind::Int: return Val::int_(v.as_i64());
        case ValueKind::Float: return Val::flt(v.as_f64());
        case ValueKind::String: return Val::text(std::string(v.as_str()));
        case ValueKind::Bytes: return Val::text(std::string(v.as_str()));
        case ValueKind::Function: return Val::nil();  // no Val counterpart
        case ValueKind::Array: {
            std::vector<Val> a;
            a.reserve(v.size());
            for (size_t i = 0; i < v.size(); i++) a.push_back(val_from_value(*v.arr_at(i)));
            return Val::list(std::move(a));
        }
        case ValueKind::Object: {
            std::vector<std::pair<std::string, Val>> o;
            o.reserve(v.size());
            for (size_t i = 0; i < v.size(); i++) o.emplace_back(v.obj_key(i), val_from_value(*v.obj_at(i)));
            return Val::object(std::move(o));
        }
    }
    return Val::nil();
}

// JSON parser (zero-copy: works directly on stored bytes)
inline Val parse_json(std::string_view s) {
    size_t p = 0;
    auto ws = [&]() { while (p < s.size() && (s[p] == ' ' || s[p] == '\t' || s[p] == '\n' || s[p] == '\r')) p++; };
    std::function<std::string()> parse_str = [&]() -> std::string {
        if (p >= s.size() || s[p] != '"') throw std::runtime_error("expected JSON string");
        p++;
        std::string out;
        while (p < s.size() && s[p] != '"') {
            char c = s[p];
            if (c == '\\') {
                p++;
                char e = s[p++];
                switch (e) {
                    case '"': out += '"'; break;
                    case '\\': out += '\\'; break;
                    case '/': out += '/'; break;
                    case 'b': out += '\b'; break;
                    case 'f': out += '\f'; break;
                    case 'n': out += '\n'; break;
                    case 'r': out += '\r'; break;
                    case 't': out += '\t'; break;
                    case 'u': {
                        if (p + 4 > s.size()) throw std::runtime_error("bad \\u escape");
                        unsigned cp = 0;
                        for (int i = 0; i < 4; i++) {
                            char h = s[p + i];
                            cp = (cp << 4) | (h >= 'a' ? (h - 'a' + 10) : (h >= 'A' ? (h - 'A' + 10) : (h - '0')));
                        }
                        p += 4;
                        if (cp < 0x80) out += (char)cp;
                        else if (cp < 0x800) { out += (char)(0xC0 | (cp >> 6)); out += (char)(0x80 | (cp & 0x3F)); }
                        else { out += (char)(0xE0 | (cp >> 12)); out += (char)(0x80 | ((cp >> 6) & 0x3F)); out += (char)(0x80 | (cp & 0x3F)); }
                        break;
                    }
                    default: throw std::runtime_error("bad JSON escape");
                }
            } else { out += c; p++; }
        }
        if (p >= s.size()) throw std::runtime_error("unterminated JSON string");
        p++;
        return out;
    };
    std::function<Val()> parse = [&]() -> Val {
        ws();
        if (p >= s.size()) throw std::runtime_error("unexpected end of JSON");
        char c = s[p];
        if (c == '{') {
            p++;
            std::vector<std::pair<std::string, Val>> obj;
            ws();
            if (p < s.size() && s[p] == '}') { p++; return Val::object(std::move(obj)); }
            while (true) {
                ws();
                std::string k = parse_str();
                ws();
                if (p >= s.size() || s[p] != ':') throw std::runtime_error("expected ':' in JSON object");
                p++;
                Val v = parse();
                obj.emplace_back(std::move(k), std::move(v));
                ws();
                if (p < s.size() && s[p] == ',') { p++; continue; }
                if (p < s.size() && s[p] == '}') { p++; break; }
                throw std::runtime_error("expected ',' or '}' in JSON object");
            }
            return Val::object(std::move(obj));
        }
        if (c == '[') {
            p++;
            std::vector<Val> arr;
            ws();
            if (p < s.size() && s[p] == ']') { p++; return Val::list(std::move(arr)); }
            while (true) {
                arr.push_back(parse());
                ws();
                if (p < s.size() && s[p] == ',') { p++; continue; }
                if (p < s.size() && s[p] == ']') { p++; break; }
                throw std::runtime_error("expected ',' or ']' in JSON array");
            }
            return Val::list(std::move(arr));
        }
        if (c == '"') return Val::text(parse_str());
        if (s.compare(p, 4, "true") == 0) { p += 4; return Val::boolean(true); }
        if (s.compare(p, 5, "false") == 0) { p += 5; return Val::boolean(false); }
        if (s.compare(p, 4, "null") == 0) { p += 4; return Val::nil(); }
        if (c == '-' || (c >= '0' && c <= '9')) {
            size_t start = p;
            if (c == '-') p++;
            bool isf = false;
            while (p < s.size()) {
                char d = s[p];
                if (d >= '0' && d <= '9') p++;
                else if (d == '.' || d == 'e' || d == 'E' || d == '+' || d == '-') { if (d == '.') isf = true; p++; }
                else break;
            }
            std::string num{s.substr(start, p - start)};
            if (isf) {
                try { return Val::flt(std::stod(num)); }
                catch (...) { throw std::runtime_error("invalid number in JSON: " + num); }
            }
            try { return Val::int_(std::stoll(num)); }
            catch (...) {
                try { return Val::flt(std::stod(num)); }
                catch (...) { throw std::runtime_error("invalid number in JSON: " + num); }
            }
        }
        throw std::runtime_error(std::string("unexpected character in JSON: '") + c + "'");
    };
    Val v = parse();
    ws();
    if (p != s.size()) throw std::runtime_error("trailing data after JSON value");
    return v;
}
inline Val parse_json(const std::string& s) { return parse_json(std::string_view(s)); }

// ===========================================================================
// Operators
// ===========================================================================
inline Val op_add(const Val& a, const Val& b) {
    if (a.is_num() && b.is_num())
        return a.is_int() && b.is_int() ? Val::int_(a.iv + b.iv) : Val::flt(a.num() + b.num());
    if (a.is_str() || b.is_str()) return Val::text(to_text_dbg(a) + to_text_dbg(b));
    if (a.is_arr() && b.is_arr()) {
        Val out = a;
        for (auto& x : b.arr) out.arr.push_back(x);
        return out;
    }
    throw std::runtime_error(std::string("cannot add ") + a.kind_name() + " and " + b.kind_name());
}
inline Val op_sub(const Val& a, const Val& b) {
    if (!a.is_num() || !b.is_num()) throw std::runtime_error("cannot subtract " + std::string(a.kind_name()) + " and " + b.kind_name());
    return a.is_int() && b.is_int() ? Val::int_(a.iv - b.iv) : Val::flt(a.num() - b.num());
}
inline Val op_mul(const Val& a, const Val& b) {
    if (!a.is_num() || !b.is_num()) throw std::runtime_error("cannot multiply " + std::string(a.kind_name()) + " and " + b.kind_name());
    return a.is_int() && b.is_int() ? Val::int_(a.iv * b.iv) : Val::flt(a.num() * b.num());
}
inline Val op_div(const Val& a, const Val& b) {
    if (!a.is_num() || !b.is_num()) throw std::runtime_error("cannot divide " + std::string(a.kind_name()) + " and " + b.kind_name());
    double d = b.num();
    if (d == 0) throw std::runtime_error("division by zero");
    if (a.is_int() && b.is_int() && b.iv != 0 && a.iv % b.iv == 0) return Val::int_(a.iv / b.iv);
    return Val::flt(a.num() / b.num());
}
inline Val op_mod(const Val& a, const Val& b) {
    if (!a.is_int() || !b.is_int()) throw std::runtime_error("modulo requires Int");
    if (b.iv == 0) throw std::runtime_error("division by zero");
    return Val::int_(a.iv % b.iv);
}
inline Val op_neg(const Val& a) {
    if (a.is_int()) return Val::int_(-a.iv);
    if (a.is_flt()) return Val::flt(-a.fv);
    throw std::runtime_error("cannot negate " + std::string(a.kind_name()));
}
inline bool eq_v(const Val& a, const Val& b) {
    if (a.is_num() && b.is_num()) return a.num() == b.num();
    if (a.t != b.t) return false;
    switch (a.t) {
        case Val::T::Nil: return true;
        case Val::T::Int: return a.iv == b.iv;
        case Val::T::Flt: return a.fv == b.fv;
        case Val::T::Bool: return a.bv == b.bv;
        case Val::T::Str: return a.sv == b.sv;
        case Val::T::Arr: {
            if (a.arr.size() != b.arr.size()) return false;
            for (size_t k = 0; k < a.arr.size(); k++) if (!eq_v(a.arr[k], b.arr[k])) return false;
            return true;
        }
        case Val::T::Obj: {
            if (a.obj.size() != b.obj.size()) return false;
            for (auto& kv : a.obj) {
                const Val* o = b.find(kv.first);
                if (!o || !eq_v(kv.second, *o)) return false;
            }
            return true;
        }
    }
    return false;
}
inline Val op_eq(const Val& a, const Val& b) { return Val::boolean(eq_v(a, b)); }
inline Val op_ne(const Val& a, const Val& b) { return Val::boolean(!eq_v(a, b)); }
inline Val op_lt(const Val& a, const Val& b) {
    if (a.is_num() && b.is_num()) return Val::boolean(a.num() < b.num());
    if (a.is_str() && b.is_str()) return Val::boolean(a.sv < b.sv);
    throw std::runtime_error("cannot compare " + std::string(a.kind_name()) + " and " + b.kind_name());
}
inline Val op_le(const Val& a, const Val& b) {
    if (a.is_num() && b.is_num()) return Val::boolean(a.num() <= b.num());
    if (a.is_str() && b.is_str()) return Val::boolean(a.sv <= b.sv);
    throw std::runtime_error("cannot compare " + std::string(a.kind_name()) + " and " + b.kind_name());
}
inline Val op_gt(const Val& a, const Val& b) { return op_lt(b, a); }
inline Val op_ge(const Val& a, const Val& b) { return op_le(b, a); }
inline Val op_and(const Val& a, const Val& b) { return Val::boolean(a.truthy() && b.truthy()); }
inline Val op_or(const Val& a, const Val& b) { return Val::boolean(a.truthy() || b.truthy()); }

inline Val to_int(const Val& v) {
    switch (v.t) {
        case Val::T::Int: return v;
        case Val::T::Flt: return Val::int_((int64_t)v.fv);
        case Val::T::Bool: return Val::int_(v.bv ? 1 : 0);
        case Val::T::Str: {
            try { return Val::int_(std::stoll(v.sv)); }
            catch (...) { throw std::runtime_error("cannot convert \"" + v.sv + "\" to Int"); }
        }
        default: throw std::runtime_error("cannot convert " + std::string(v.kind_name()) + " to Int");
    }
}
inline Val to_flt(const Val& v) {
    switch (v.t) {
        case Val::T::Flt: return v;
        case Val::T::Int: return Val::flt((double)v.iv);
        case Val::T::Bool: return Val::flt(v.bv ? 1.0 : 0.0);
        case Val::T::Str: {
            try { return Val::flt(std::stod(v.sv)); }
            catch (...) { throw std::runtime_error("cannot convert \"" + v.sv + "\" to Float"); }
        }
        default: throw std::runtime_error("cannot convert " + std::string(v.kind_name()) + " to Float");
    }
}
inline Val to_text(const Val& v) {
    switch (v.t) {
        case Val::T::Nil: return Val::text("nil");
        case Val::T::Int: return Val::text(std::to_string(v.iv));
        case Val::T::Flt: { std::ostringstream o; o.precision(15); o << v.fv; return Val::text(o.str()); }
        case Val::T::Bool: return Val::text(v.bv ? "true" : "false");
        case Val::T::Str: return v;
        default: return Val::text(v.to_json());
    }
}
inline Val to_bool(const Val& v) { return Val::boolean(v.truthy()); }

inline Val get_member(const Val& o, const std::string& k) {
    if (o.is_obj()) {
        const Val* p = o.find(k);
        return p ? *p : Val::nil();
    }
    return Val::nil();
}
inline int64_t val_len(const Val& v) {
    switch (v.t) {
        case Val::T::Arr: return (int64_t)v.arr.size();
        case Val::T::Obj: return (int64_t)v.obj.size();
        case Val::T::Str: return (int64_t)v.sv.size();
        default: return 0;
    }
}
inline Val v_len(const Val& v) { return Val::int_(val_len(v)); }
inline Val index_at(const Val& v, const Val& i) {
    if (v.is_arr()) {
        if (!i.is_int()) throw std::runtime_error("list index must be Int");
        int64_t n = i.iv;
        int64_t sz = (int64_t)v.arr.size();
        if (n < 0) n += sz;
        if (n < 0 || n >= sz) throw std::runtime_error("list index out of range");
        return v.arr[(size_t)n];
    }
    if (v.is_obj()) {
        if (!i.is_str()) throw std::runtime_error("map key must be Text");
        const Val* p = v.find(i.sv);
        return p ? *p : Val::nil();
    }
    throw std::runtime_error("cannot index " + std::string(v.kind_name()));
}
inline Val set_index(Val& v, const Val& i, const Val& x) {
    if (v.is_arr()) {
        if (!i.is_int()) throw std::runtime_error("list index must be Int");
        int64_t n = i.iv;
        int64_t sz = (int64_t)v.arr.size();
        if (n < 0) n += sz;
        if (n < 0 || n >= sz) throw std::runtime_error("list index out of range");
        v.arr[(size_t)n] = x;
        return x;
    }
    if (v.is_obj()) {
        if (!i.is_str()) throw std::runtime_error("map key must be Text");
        v.get(i.sv) = x;
        return x;
    }
    throw std::runtime_error("cannot index " + std::string(v.kind_name()));
}
inline void print_out(const std::string& s) {
    fwrite(s.c_str(), 1, s.size(), stdout);
    fflush(stdout);
}
inline void log_val(const Val& v) {
    std::string s = v.is_str() ? v.sv : v.to_text_dbg();
    print_out(s + "\n");
}
inline void print_val(const Val& v) { log_val(v); }
inline Val v_print(const Val& v) { log_val(v); return Val::nil(); }

#endif
