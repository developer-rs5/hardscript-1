#pragma once
// hs_runtime.hpp — HardScript runtime (single header, C++17).
// Values, JSON, operators, fs/env/time/runtime, crypto (sha256/sha1/md5/
// hmac/base64), JWT, HTTP/1.1 server with routing & middleware, WebSocket
// server, PostgreSQL wire client, and the in-process test runner.
// The HardScript codegen emits a C++ file that includes this header and calls:
//   int hs_main(int /*argc*/, char** /*argv*/, App&);
// where App exposes: listen(port), exec_test(Supplier::run()) etc. (see below).

#include <algorithm>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <fstream>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <set>
#include <sstream>
#include <string>
#include <thread>
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
inline void json_escape(const std::string& in, std::string& out) {
    for (unsigned char c : in) {
        switch (c) {
            case '"': out += "\\\""; break;
            case '\\': out += "\\\\"; break;
            case '\b': out += "\\b"; break;
            case '\f': out += "\\f"; break;
            case '\n': out += "\\n"; break;
            case '\r': out += "\\r"; break;
            case '\t': out += "\\t"; break;
            default:
                if (c < 0x20) { char b[8]; std::snprintf(b, 8, "\\u%04x", c); out += b; }
                else out += (char)c;
        }
    }
}

inline void val_to_json(const Val& v, std::string& out) {
    switch (v.t) {
        case Val::T::Nil: out += "null"; break;
        case Val::T::Int: out += std::to_string(v.iv); break;
        case Val::T::Flt: {
            std::ostringstream o;
            o.precision(15);
            o << v.fv;
            std::string s = o.str();
            if (s.find('.') == std::string::npos) s += ".0";
            out += s;
            break;
        }
        case Val::T::Bool: out += v.bv ? "true" : "false"; break;
        case Val::T::Str: out += '"'; json_escape(v.sv, out); out += '"'; break;
        case Val::T::Arr: {
            out += '[';
            for (size_t k = 0; k < v.arr.size(); k++) { if (k) out += ','; val_to_json(v.arr[k], out); }
            out += ']';
            break;
        }
        case Val::T::Obj: {
            out += '{';
            for (size_t k = 0; k < v.obj.size(); k++) {
                if (k) out += ',';
                out += '"'; json_escape(v.obj[k].first, out); out += "\":";
                val_to_json(v.obj[k].second, out);
            }
            out += '}';
            break;
        }
    }
}

inline std::string to_json(const Val& v) {
    std::string out;
    val_to_json(v, out);
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

// JSON parser
inline Val parse_json(const std::string& s) {
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
            std::string num = s.substr(start, p - start);
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

// ===========================================================================
// fs / env / time / runtime modules
// ===========================================================================
inline std::string env_get(const std::string& k) {
    const char* v = std::getenv(k.c_str());
    return v ? std::string(v) : "";
}
inline int64_t unix_ms() {
    return (int64_t)std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::system_clock::now().time_since_epoch()).count();
}
inline std::string time_iso() {
    time_t t = time(nullptr);
    struct tm g;
    gmtime_r(&t, &g);
    char b[40];
    std::strftime(b, sizeof b, "%Y-%m-%dT%H:%M:%SZ", &g);
    return b;
}
inline void sleep_sec(double s) {
    std::this_thread::sleep_for(std::chrono::duration<double>(s));
}

inline std::string fs_read(const std::string& path) {
    std::ifstream f(path, std::ios::binary);
    if (!f) throw std::runtime_error("cannot read file: " + path);
    std::ostringstream ss;
    ss << f.rdbuf();
    return ss.str();
}
inline void fs_write(const std::string& path, const std::string& data) {
    std::ofstream f(path, std::ios::binary | std::ios::trunc);
    if (!f) throw std::runtime_error("cannot write file: " + path);
    f.write(data.data(), (std::streamsize)data.size());
}
inline void fs_append(const std::string& path, const std::string& data) {
    std::ofstream f(path, std::ios::binary | std::ios::app);
    if (!f) throw std::runtime_error("cannot append to file: " + path);
    f.write(data.data(), (std::streamsize)data.size());
}
inline bool fs_exists(const std::string& path) {
    struct stat st;
    return stat(path.c_str(), &st) == 0;
}
inline bool fs_is_dir(const std::string& path) {
    struct stat st;
    return stat(path.c_str(), &st) == 0 && S_ISDIR(st.st_mode);
}
inline std::vector<std::string> fs_list(const std::string& path) {
    std::vector<std::string> out;
    DIR* d = opendir(path.c_str());
    if (!d) throw std::runtime_error("cannot list directory: " + path);
    struct dirent* e;
    while ((e = readdir(d))) {
        std::string n = e->d_name;
        if (n != "." && n != "..") out.push_back(n);
    }
    closedir(d);
    std::sort(out.begin(), out.end());
    return out;
}
inline void fs_remove(const std::string& path) {
    if (::remove(path.c_str()) != 0) throw std::runtime_error("cannot remove: " + path);
}
inline int64_t fs_size(const std::string& path) {
    struct stat st;
    if (stat(path.c_str(), &st) != 0) throw std::runtime_error("cannot stat: " + path);
    return (int64_t)st.st_size;
}

// ===========================================================================
// Crypto
// ===========================================================================
inline uint32_t rol32(uint32_t x, int n) { return (x << n) | (x >> (32 - n)); }

inline void sha256_blocks(uint32_t* h, const uint8_t* p) {
    static const uint32_t K[64] = {
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    };
    uint32_t w[64];
    for (int i = 0; i < 16; i++)
        w[i] = ((uint32_t)p[i * 4] << 24) | ((uint32_t)p[i * 4 + 1] << 16) | ((uint32_t)p[i * 4 + 2] << 8) | p[i * 4 + 3];
    for (int i = 16; i < 64; i++) {
        uint32_t s0 = rol32(w[i - 15], 7) ^ rol32(w[i - 15], 18) ^ (w[i - 15] >> 3);
        uint32_t s1 = rol32(w[i - 2], 17) ^ rol32(w[i - 2], 19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16] + s0 + w[i - 7] + s1;
    }
    uint32_t a = h[0], b = h[1], c = h[2], d = h[3];
    uint32_t e = h[4], f = h[5], g = h[6], hh = h[7];
    for (int i = 0; i < 64; i++) {
        uint32_t S1 = rol32(e, 6) ^ rol32(e, 11) ^ rol32(e, 25);
        uint32_t ch = (e & f) ^ (~e & g);
        uint32_t t1 = hh + S1 + ch + K[i] + w[i];
        uint32_t S0 = rol32(a, 2) ^ rol32(a, 13) ^ rol32(a, 22);
        uint32_t maj = (a & b) ^ (a & c) ^ (b & c);
        uint32_t t2 = S0 + maj;
        hh = g; g = f; f = e; e = d + t1;
        d = c; c = b; b = a; a = t1 + t2;
    }
    h[0] += a; h[1] += b; h[2] += c; h[3] += d;
    h[4] += e; h[5] += f; h[6] += g; h[7] += hh;
}
inline std::string sha256_hex(const std::string& data) {
    uint32_t h[8] = { 0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19 };
    std::vector<uint8_t> m(data.begin(), data.end());
    m.push_back(0x80);
    while (m.size() % 64 != 56) m.push_back(0);
    uint64_t bitlen = (uint64_t)data.size() * 8;
    for (int i = 7; i >= 0; i--) m.push_back((uint8_t)(bitlen >> (i * 8)));
    for (size_t o = 0; o < m.size(); o += 64) sha256_blocks(h, m.data() + o);
    const char* hex = "0123456789abcdef";
    std::string out;
    for (int i = 0; i < 8; i++)
        for (int b = 28; b >= 0; b -= 4) out += hex[(h[i] >> b) & 15];
    return out;
}
inline std::string sha256_bin(const std::string& d) {
    const std::string hex = sha256_hex(d);
    auto hv = [](char c) { if (c >= '0' && c <= '9') return c - '0'; if (c >= 'a' && c <= 'f') return c - 'a' + 10; return 0; };
    std::string out;
    for (size_t i = 0; i < hex.size(); i += 2) out += (char)((hv(hex[i]) << 4) | hv(hex[i + 1]));
    return out;
}
inline std::string hmac_sha256_bin(const std::string& key, const std::string& data) {
    std::string k = key;
    if (k.size() > 64) k = sha256_bin(k);
    while (k.size() < 64) k.push_back('\0');
    std::string ipad(64, 0x36), opad(64, 0x5c);
    for (int i = 0; i < 64; i++) { ipad[i] ^= k[i]; opad[i] ^= k[i]; }
    return sha256_bin(opad + sha256_bin(ipad + data));
}
inline std::string base64_encode(const std::string& d) {
    static const char* tbl = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    std::string out;
    size_t i = 0;
    while (i + 3 <= d.size()) {
        uint32_t v = ((uint8_t)d[i] << 16) | ((uint8_t)d[i + 1] << 8) | (uint8_t)d[i + 2];
        out += tbl[(v >> 18) & 63]; out += tbl[(v >> 12) & 63]; out += tbl[(v >> 6) & 63]; out += tbl[v & 63];
        i += 3;
    }
    size_t r = d.size() - i;
    if (r == 1) {
        uint32_t v = (uint8_t)d[i] << 16;
        out += tbl[(v >> 18) & 63]; out += tbl[(v >> 12) & 63]; out += "==";
    } else if (r == 2) {
        uint32_t v = ((uint8_t)d[i] << 16) | ((uint8_t)d[i + 1] << 8);
        out += tbl[(v >> 18) & 63]; out += tbl[(v >> 12) & 63]; out += tbl[(v >> 6) & 63]; out += "=";
    }
    return out;
}
inline std::string base64_encode_url(const std::string& d) {
    std::string out = base64_encode(d);
    std::replace(out.begin(), out.end(), '+', '-');
    std::replace(out.begin(), out.end(), '/', '_');
    while (!out.empty() && out.back() == '=') out.pop_back();
    return out;
}
inline std::string base64_decode(const std::string& s) {
    auto hv = [](char c) -> int {
        if (c >= 'A' && c <= 'Z') return c - 'A';
        if (c >= 'a' && c <= 'z') return c - 'a' + 26;
        if (c >= '0' && c <= '9') return c - '0' + 52;
        if (c == '+' || c == '-') return 62;
        if (c == '/' || c == '_') return 63;
        return -1;
    };
    std::string out;
    int v = 0, bits = 0;
    for (char c : s) {
        int d = hv(c);
        if (d < 0) continue;
        v = (v << 6) | d;
        bits += 6;
        if (bits >= 8) { bits -= 8; out += (char)((v >> bits) & 0xff); }
    }
    return out;
}
inline std::string random_hex(int nbytes) {
    static std::atomic<uint64_t> c{0};
    uint64_t seed = std::chrono::high_resolution_clock::now().time_since_epoch().count() ^ (c.fetch_add(1) * 0x9e3779b97f4a7c15ULL);
    std::mt19937_64 g(seed);
    const char* hex = "0123456789abcdef";
    std::string out;
    for (int i = 0; i < nbytes; i++) {
        uint64_t x = g();
        out += hex[(x >> 8) & 15];
        out += hex[x & 15];
    }
    return out;
}
inline std::string random_urlsafe(int nbytes) {
    static const char* tbl = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    static std::atomic<uint64_t> c{0};
    uint64_t seed = std::chrono::high_resolution_clock::now().time_since_epoch().count() ^ (c.fetch_add(1) * 0x9e3779b97f4a7c15ULL);
    std::mt19937_64 g(seed);
    std::string out;
    for (int i = 0; i < nbytes; i++) out += tbl[g() % 64];
    return out;
}
inline std::string sha1_bin(const std::string& data) {
    uint32_t h[5] = { 0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0 };
    std::vector<uint8_t> m(data.begin(), data.end());
    m.push_back(0x80);
    while (m.size() % 64 != 56) m.push_back(0);
    uint64_t bitlen = (uint64_t)data.size() * 8;
    for (int i = 7; i >= 0; i--) m.push_back((uint8_t)(bitlen >> (i * 8)));
    for (size_t o = 0; o < m.size(); o += 64) {
        const uint8_t* p = m.data() + o;
        uint32_t w[80];
        for (int i = 0; i < 16; i++)
            w[i] = ((uint32_t)p[i * 4] << 24) | ((uint32_t)p[i * 4 + 1] << 16) | ((uint32_t)p[i * 4 + 2] << 8) | p[i * 4 + 3];
        for (int i = 16; i < 80; i++) { uint32_t x = w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]; w[i] = rol32(x, 1); }
        uint32_t a = h[0], b = h[1], c = h[2], d = h[3], e = h[4];
        for (int i = 0; i < 80; i++) {
            uint32_t f, k;
            if (i < 20) { f = (b & c) | (~b & d); k = 0x5A827999; }
            else if (i < 40) { f = b ^ c ^ d; k = 0x6ED9EBA1; }
            else if (i < 60) { f = (b & c) | (b & d) | (c & d); k = 0x8F1BBCDC; }
            else { f = b ^ c ^ d; k = 0xCA62C1D6; }
            uint32_t tmp = rol32(a, 5) + f + e + k + w[i];
            e = d; d = c; c = rol32(b, 30); b = a; a = tmp;
        }
        h[0] += a; h[1] += b; h[2] += c; h[3] += d; h[4] += e;
    }
    std::string out;
    for (int i = 0; i < 5; i++)
        for (int b = 24; b >= 0; b -= 8) out += (char)((h[i] >> b) & 0xff);
    return out;
}
inline std::string md5_hex(const std::string& data) {
    uint32_t a0 = 0x67452301, b0 = 0xefcdab89, c0 = 0x98badcfe, d0 = 0x10325476;
    static const uint32_t K[64] = {
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    };
    static const int S[64] = { 7,12,17,22,7,12,17,22,7,12,17,22,7,12,17,22, 5,9,14,20,5,9,14,20,5,9,14,20,5,9,14,20, 4,11,16,23,4,11,16,23,4,11,16,23,4,11,16,23, 6,10,15,21,6,10,15,21,6,10,15,21,6,10,15,21 };
    auto rol = [](uint32_t x, int n) { return (x << n) | (x >> (32 - n)); };
    auto le = [](const uint8_t* p) { return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24); };
    std::vector<uint8_t> m(data.begin(), data.end());
    uint64_t bitlen = (uint64_t)data.size() * 8;
    m.push_back(0x80);
    while (m.size() % 64 != 56) m.push_back(0);
    for (int i = 0; i < 8; i++) m.push_back((uint8_t)(bitlen >> (i * 8)));
    for (size_t o = 0; o < m.size(); o += 64) {
        const uint8_t* p = m.data() + o;
        uint32_t M[16];
        for (int i = 0; i < 16; i++) M[i] = le(p + i * 4);
        uint32_t A = a0, B = b0, C = c0, D = d0;
        for (int i = 0; i < 64; i++) {
            uint32_t F, g;
            if (i < 16) { F = (B & C) | (~B & D); g = i; }
            else if (i < 32) { F = (D & B) | (~D & C); g = (5 * i + 1) % 16; }
            else if (i < 48) { F = B ^ C ^ D; g = (3 * i + 5) % 16; }
            else { F = C ^ (B | ~D); g = (7 * i) % 16; }
            uint32_t tmp = D;
            D = C; C = B;
            B = B + rol(A + F + K[i] + M[g], S[i]);
            A = tmp;
        }
        a0 += A; b0 += B; c0 += C; d0 += D;
    }
    const char* hex = "0123456789abcdef";
    std::string out;
    auto emit = [&](uint32_t v) {
        for (int b = 0; b < 4; b++) { uint8_t x = (uint8_t)(v >> (b * 8)); out += hex[x >> 4]; out += hex[x & 15]; }
    };
    emit(a0); emit(b0); emit(c0); emit(d0);
    return out;
}

// JWT
inline std::string jwt_sign(const Val& payload, const std::string& secret) {
    std::string h = base64_encode_url("{\"alg\":\"HS256\",\"typ\":\"JWT\"}");
    std::string b = base64_encode_url(to_json(payload));
    std::string body = h + "." + b;
    return body + "." + base64_encode_url(hmac_sha256_bin(secret, body));
}
inline bool jwt_verify(const std::string& token, const std::string& secret) {
    std::vector<std::string> parts;
    std::string cur;
    for (char c : token) { if (c == '.') { parts.push_back(cur); cur.clear(); } else cur += c; }
    parts.push_back(cur);
    if (parts.size() != 3) return false;
    std::string expect = base64_encode_url(hmac_sha256_bin(secret, parts[0] + "." + parts[1]));
    if (expect != parts[2]) return false;
    try {
        Val body = parse_json(base64_decode(parts[1]));
        const Val* exp = body.find("exp");
        if (exp && exp->is_num() && (double)exp->num() < (double)(unix_ms() / 1000)) return false;
    } catch (...) { return false; }
    return true;
}

// ===========================================================================
// HTTP
// ===========================================================================
struct Request {
    std::string method;
    std::string path;    // decoded path, no query
    std::string query;
    std::string body;
    std::map<std::string, std::string> headers;  // lowercased keys
    std::map<std::string, std::string> params;

    std::string header(const std::string& name) const {
        std::string k = name;
        std::transform(k.begin(), k.end(), k.begin(), ::tolower);
        auto it = headers.find(k);
        return it == headers.end() ? "" : it->second;
    }
    std::string q(const std::string& key) const {
        std::string qq = query;
        std::string out;
        size_t i = 0;
        while (i <= qq.size()) {
            size_t amp = qq.find('&', i);
            std::string pair = qq.substr(i, amp == std::string::npos ? qq.size() - i : amp - i);
            size_t eqpos = pair.find('=');
            std::string k = pair.substr(0, eqpos);
            if (k == key) {
                std::string v = eqpos == std::string::npos ? "" : pair.substr(eqpos + 1);
                for (size_t j = 0; j < v.size(); j++) {
                    if (v[j] == '+' ) out += ' ';
                    else if (v[j] == '%' && j + 2 < v.size()) {
                        auto hv = [](char c) { if (c >= '0' && c <= '9') return c - '0'; if (c >= 'a' && c <= 'f') return c - 'a' + 10; if (c >= 'A' && c <= 'F') return c - 'A' + 10; return 0; };
                        out += (char)((hv(v[j + 1]) << 4) | hv(v[j + 2]));
                        j += 2;
                    } else out += v[j];
                }
                return out;
            }
            if (amp == std::string::npos) break;
            i = amp + 1;
        }
        return "";
    }
    Val json() const {
        if (body.empty()) return Val::object({});
        return parse_json(body);
    }
    std::string form(const std::string& name) const {
        Request tmp;
        tmp.query = body;
        return tmp.q(name);
    }
};

struct Response {
    int status = 200;
    std::string ctype = "text/plain; charset=utf-8";
    std::string body;
    std::vector<std::pair<std::string, std::string>> headers;
    Response() = default;
    static Response json(const Val& v) {
        Response r;
        r.ctype = "application/json; charset=utf-8";
        r.body = to_json(v);
        return r;
    }
    static Response text(const std::string& s) {
        Response r;
        r.body = s;
        return r;
    }
    static Response html(const std::string& s) {
        Response r;
        r.ctype = "text/html; charset=utf-8";
        r.body = s;
        return r;
    }
    static Response empty(int status = 204) {
        Response r;
        r.status = status;
        return r;
    }
    static Response error(int status, const std::string& msg) {
        Response r;
        r.status = status;
        r.ctype = "application/json; charset=utf-8";
        r.body = "{\"error\":\"" + msg + "\"}";
        return r;
    }
};

using Handler = std::function<Response(const Request&)>;
using Middleware = std::function<Response(const Request&, const std::function<Response()>& next)>;

struct Route {
    std::string method;
    std::string path; // "/users/:id"
    Handler handler;
};

struct Server {
    std::vector<Middleware> middlewares;
    std::vector<Route> routes;
    int port = 3000;

    void before(Middleware m) { middlewares.push_back(std::move(m)); }
    void handle(const std::string& method, const std::string& path, Handler h) {
        routes.push_back({ method, path, std::move(h) });
    }

    static std::vector<std::string> split(const std::string& p) {
        std::vector<std::string> out;
        std::string cur;
        for (char c : p) {
            if (c == '/') { if (!cur.empty()) { out.push_back(cur); cur.clear(); } }
            else cur += c;
        }
        if (!cur.empty()) out.push_back(cur);
        return out;
    }

    // returns index of matching route or -1
    int match(const std::string& method, const std::string& path, Request& req) const {
        std::vector<std::string> got = split(path);
        for (size_t i = 0; i < routes.size(); i++) {
            const Route& r = routes[i];
            if (r.method != method) continue;
            std::vector<std::string> want = split(r.path);
            if (want.size() != got.size()) continue;
            bool ok = true;
            std::map<std::string, std::string> params;
            for (size_t k = 0; k < want.size(); k++) {
                if (!want[k].empty() && want[k][0] == ':') params[want[k].substr(1)] = got[k];
                else if (want[k] != got[k]) { ok = false; break; }
            }
            if (ok) { req.params = std::move(params); return (int)i; }
        }
        return -1;
    }

    Response dispatch(Request& req) {
        std::function<Response(size_t)> run = [&](size_t mi) -> Response {
            if (mi < middlewares.size()) {
                Middleware& mw = middlewares[mi];
                std::function<Response()> next = [&]() { return run(mi + 1); };
                return mw(req, next);
            }
            int ri = match(req.method, req.path, req);
            if (ri < 0) return Response::error(404, "not found");
            try {
                return routes[(size_t)ri].handler(req);
            } catch (const std::exception& e) {
                return Response::error(500, std::string("internal error: ") + e.what());
            }
        };
        try {
            return run(0);
        } catch (const std::exception& e) {
            return Response::error(500, std::string("middleware error: ") + e.what());
        }
    }

    // in-process request (used by tests)
    Val call(const std::string& method, const std::string& raw_url, const Val& body) {
        Request req;
        size_t qpos = raw_url.find('?');
        req.path = qpos == std::string::npos ? raw_url : raw_url.substr(0, qpos);
        req.query = qpos == std::string::npos ? "" : raw_url.substr(qpos + 1);
        req.method = method;
        req.headers["content-type"] = "application/json";
        req.body = body.is_str() ? body.sv : to_json(body);
        Response r = dispatch(req);
        Val out = Val::object({});
        out.set("status", Val::int_(r.status));
        Val h = Val::object({});
        h.set("content-type", Val::text(r.ctype));
        out.set("headers", h);
        if (!r.body.empty()) {
            try { out.set("body", parse_json(r.body)); }
            catch (...) { out.set("body", Val::text(r.body)); }
        }
        return out;
    }

    void listen();
    int test_main();
};

// ---- HTTP wire ----
static std::string http_reason(int status) {
    switch (status) {
        case 200: return "OK";
        case 201: return "Created";
        case 204: return "No Content";
        case 400: return "Bad Request";
        case 401: return "Unauthorized";
        case 403: return "Forbidden";
        case 404: return "Not Found";
        case 405: return "Method Not Allowed";
        case 418: return "I'm a teapot";
        case 500: return "Internal Server Error";
        case 501: return "Not Implemented";
        default: return "Status";
    }
}

static void send_all(int fd, const std::string& data) {
    size_t off = 0;
    while (off < data.size()) {
        ssize_t n = send(fd, data.data() + off, data.size() - off, MSG_NOSIGNAL);
        if (n <= 0) break;
        off += (size_t)n;
    }
}

static Response http_respond(int fd, const Response& r) {
    std::string out = "HTTP/1.1 " + std::to_string(r.status) + " " + http_reason(r.status) + "\r\n";
    out += "Content-Type: " + r.ctype + "\r\n";
    out += "Content-Length: " + std::to_string(r.body.size()) + "\r\n";
    out += "Connection: close\r\n";
    out += "Server: hardscript/" + std::string(HS_VERSION_STRING) + "\r\n";
    for (auto& h : r.headers) out += h.first + ": " + h.second + "\r\n";
    out += "\r\n";
    out += r.body;
    send_all(fd, out);
    return r;
}

// WebSocket frame send
static void ws_send(int fd, const std::string& payload, bool binary) {
    std::string f;
    f += (char)(0x80 | (binary ? 0x2 : 0x1));
    size_t len = payload.size();
    if (len < 126) {
        f += (char)len;
    } else if (len < 65536) {
        f += (char)126; f += (char)(len >> 8); f += (char)(len & 0xff);
    } else {
        f += (char)127;
        for (int i = 7; i >= 0; i--) f += (char)((uint64_t)len >> (i * 8));
    }
    f += payload;
    send_all(fd, f);
}

struct WsHandler {
    std::string path;
    std::function<void(const Request&)> on_open;
    std::function<void(const std::string&, bool binary)> on_msg;
    std::function<void()> on_close;
    std::function<void(const std::string&, const std::string&)> on_broadcast_room;
};

// global ws registry (single server)
static std::vector<WsHandler>& ws_registry() {
    static std::vector<WsHandler> r;
    return r;
}
static std::mutex& ws_bcast_mutex() {
    static std::mutex m;
    return m;
}
static std::map<std::string, int>& ws_clients() {
    static std::map<std::string, int> m;
    return m;
}
static std::map<std::string, std::set<std::string>>& ws_rooms() {
    static std::map<std::string, std::set<std::string>> m;
    return m;
}
static std::map<std::string, std::string>& ws_client_room() {
    static std::map<std::string, std::string> m;
    return m;
}

inline void ws_join(const std::string& client_id, const std::string& room) {
    std::lock_guard<std::mutex> lk(ws_bcast_mutex());
    ws_rooms()[room].insert(client_id);
    ws_client_room()[client_id] = room;
}
inline void ws_leave(const std::string& client_id) {
    std::lock_guard<std::mutex> lk(ws_bcast_mutex());
    auto it = ws_client_room().find(client_id);
    if (it != ws_client_room().end()) {
        ws_rooms().erase(it->second);
        ws_client_room().erase(it);
    }
}

inline void ws_broadcast(const std::string& payload) {
    std::lock_guard<std::mutex> lk(ws_bcast_mutex());
    for (auto& kv : ws_clients()) ws_send(kv.second, payload, false);
}
inline void ws_broadcast_room(const std::string& room, const std::string& payload) {
    std::lock_guard<std::mutex> lk(ws_bcast_mutex());
    auto it = ws_rooms().find(room);
    if (it == ws_rooms().end()) return;
    for (auto& id : it->second) {
        auto c = ws_clients().find(id);
        if (c != ws_clients().end()) ws_send(c->second, payload, false);
    }
}

// parse a single ws frame from buffer at offset; returns consumed bytes or 0
static size_t ws_frame(const std::string& buf, size_t off, std::string& payload_out, bool handle_close, int fd) {
    if (off + 2 > buf.size()) return 0;
    uint8_t b0 = (uint8_t)buf[off];
    uint8_t b1 = (uint8_t)buf[off + 1];
    int opcode = b0 & 0x0f;
    bool fin = (b0 & 0x80) != 0;
    size_t len = b1 & 0x7f;
    bool masked = (b1 & 0x80) != 0;
    size_t pos = off + 2;
    if (len == 126) {
        if (pos + 2 > buf.size()) return 0;
        len = ((uint8_t)buf[pos] << 8) | (uint8_t)buf[pos + 1];
        pos += 2;
    } else if (len == 127) {
        if (pos + 8 > buf.size()) return 0;
        len = 0;
        for (int i = 0; i < 8; i++) len = (len << 8) | (uint8_t)buf[pos + i];
        pos += 8;
    }
    uint8_t mask[4] = { 0, 0, 0, 0 };
    if (masked) {
        if (pos + 4 > buf.size()) return 0;
        memcpy(mask, buf.data() + pos, 4);
        pos += 4;
    }
    if (pos + len > buf.size()) return 0;
    std::string payload = buf.substr(pos, len);
    if (masked) {
        for (size_t i = 0; i < payload.size(); i++) payload[i] ^= mask[i % 4];
    }
    size_t consumed = (pos + len) - off;
    (void)fin;
    if (opcode == 0x8) { // close
        if (handle_close) {
            std::string close_frame;
            close_frame += (char)0x88;
            close_frame += (char)0;
            send_all(fd, close_frame);
        }
        payload_out.clear();
        return consumed | (1ULL << 63); // signal close via high bit
    } else if (opcode == 0x9) { // ping -> pong
        std::string pong;
        pong += (char)0x8A;
        pong += (char)payload.size();
        pong += payload;
        send_all(fd, pong);
        payload_out.clear();
    } else if (opcode == 0xA) { // pong
        payload_out.clear();
    } else { // 1 text, 2 binary
        payload_out = payload;
    }
    return consumed;
}

// thread-local identity for the connection currently being served
inline std::string& ws_cur() { static thread_local std::string s; return s; }
inline int& ws_cur_fd() { static thread_local int s = -1; return s; }
inline std::string ws_self() { return ws_cur(); }
inline void ws_reply(const std::string& payload) {
    int fd = ws_cur_fd();
    if (fd >= 0) ws_send(fd, payload, false);
}

static void handle_connection(int fd, Server& srv) {
    std::string buf;
    char chunk[4096];
    struct pollfd pfd;
    memset(&pfd, 0, sizeof pfd);
    pfd.fd = fd;
    pfd.events = POLLIN;
    // read headers
    while (buf.find("\r\n\r\n") == std::string::npos) {
        int pr = poll(&pfd, 1, 5000);
        if (pr <= 0) break;
        ssize_t n = recv(fd, chunk, sizeof chunk, 0);
        if (n <= 0) break;
        buf.append(chunk, (size_t)n);
        if (buf.size() > 16 * 1024 * 1024) break;
    }
    size_t hend = buf.find("\r\n\r\n");
    if (hend == std::string::npos) { close(fd); return; }
    std::string head = buf.substr(0, hend);
    std::string body = buf.substr(hend + 4);
    // request line
    size_t eol = head.find("\r\n");
    std::string rl = eol == std::string::npos ? head : head.substr(0, eol);
    size_t sp1 = rl.find(' ');
    size_t sp2 = rl.rfind(' ');
    if (sp1 == std::string::npos || sp2 == sp1) { close(fd); return; }
    Request req;
    req.method = rl.substr(0, sp1);
    std::string target = rl.substr(sp1 + 1, sp2 - sp1 - 1);
    size_t qpos = target.find('?');
    req.path = qpos == std::string::npos ? target : target.substr(0, qpos);
    req.query = qpos == std::string::npos ? "" : target.substr(qpos + 1);
    // headers
    size_t p = eol == std::string::npos ? head.size() : eol + 2;
    size_t cl = 0;
    while (p < head.size()) {
        size_t e = head.find("\r\n", p);
        if (e == std::string::npos) e = head.size();
        std::string line = head.substr(p, e - p);
        size_t c = line.find(':');
        if (c != std::string::npos) {
            std::string k = line.substr(0, c);
            std::string v = line.substr(c + 1);
            while (!v.empty() && v[0] == ' ') v.erase(v.begin());
            while (!v.empty() && (v.back() == '\r' || v.back() == '\n')) v.pop_back();
            std::transform(k.begin(), k.end(), k.begin(), ::tolower);
            req.headers[k] = v;
            if (k == "content-length") cl = (size_t)strtoull(v.c_str(), nullptr, 10);
        }
        p = e + 2;
    }
    while (body.size() < cl) {
        ssize_t n = recv(fd, chunk, sizeof chunk, 0);
        if (n <= 0) break;
        body.append(chunk, (size_t)n);
    }
    if (req.headers["upgrade"] == "websocket") {
        // websocket upgrade
        for (auto& ws : ws_registry()) {
            if (ws.path != req.path) continue;
            std::string key = req.headers["sec-websocket-key"];
            std::string accept = base64_encode(sha1_bin(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"));
            std::string resp = "HTTP/1.1 101 Switching Protocols\r\n";
            resp += "Upgrade: websocket\r\n";
            resp += "Connection: Upgrade\r\n";
            resp += "Sec-WebSocket-Accept: " + accept + "\r\n\r\n";
            send_all(fd, resp);
            if (ws.on_open) ws.on_open(req);
            std::string client_id = random_hex(8);
            ws_clients()[client_id] = fd;
            ws_cur() = client_id;
            ws_cur_fd() = fd;
            // frame loop
            std::string acc;
            char wbuf[8192];
            bool open = true;
            while (open) {
                struct pollfd wp;
                memset(&wp, 0, sizeof wp);
                wp.fd = fd;
                wp.events = POLLIN;
                int pr = poll(&wp, 1, 10000);
                if (pr <= 0) break;
                ssize_t n = recv(fd, wbuf, sizeof wbuf, 0);
                if (n <= 0) break;
                acc.append(wbuf, (size_t)n);
                size_t base = 0;
                std::string payload;
                while (base < acc.size()) {
                    size_t r = ws_frame(acc, base, payload, true, fd);
                    if (r == 0) break;
                    if (r & (1ULL << 63)) { open = false; break; }
                    base += r;
                    if (ws.on_msg && !(base == 0 && payload.empty())) {
                        if (!payload.empty() || true) {
                            if (ws.on_msg) ws.on_msg(payload, false);
                        }
                    }
                    payload.clear();
                }
                if (base > 0) acc.erase(0, base);
            }
            ws_leave(client_id);
            ws_clients().erase(client_id);
            ws_cur().clear();
            ws_cur_fd() = -1;
            if (ws.on_close) ws.on_close();
            close(fd);
            return;
        }
        // no ws route — fall through to 404
    }
    req.body = body;
    Response r;
    try {
        r = srv.dispatch(req);
    } catch (const std::exception& e) {
        r = Response::error(500, std::string("error: ") + e.what());
    }
    http_respond(fd, r);
    close(fd);
}

inline void Server::listen() {
    int sfd = socket(AF_INET, SOCK_STREAM, 0);
    if (sfd < 0) throw std::runtime_error("cannot create socket");
    int one = 1;
    setsockopt(sfd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons((uint16_t)port);
    a.sin_addr.s_addr = htonl(INADDR_ANY);
    if (bind(sfd, (struct sockaddr*)&a, sizeof a) != 0) {
        close(sfd);
        throw std::runtime_error("cannot bind to port " + std::to_string(port));
    }
    if (::listen(sfd, 512) != 0) {
        close(sfd);
        throw std::runtime_error("cannot listen on port " + std::to_string(port));
    }
    printf("HardScript server listening on http://127.0.0.1:%d\n", port);
    fflush(stdout);
    for (;;) {
        struct sockaddr_in ca;
        socklen_t clen = sizeof ca;
        int cfd = accept(sfd, (struct sockaddr*)&ca, &clen);
        if (cfd < 0) {
            if (errno == EINTR) continue;
            break;
        }
        std::thread(handle_connection, cfd, std::ref(*this)).detach();
    }
    close(sfd);
}

// ===========================================================================
// Test runner
// ===========================================================================
inline std::vector<std::pair<std::string, std::function<void()>>>& test_registry() {
    static std::vector<std::pair<std::string, std::function<void()>>> r;
    return r;
}
struct TestState {
    int passed = 0;
    int failed = 0;
};
inline TestState& test_state() {
    static TestState s;
    return s;
}
inline void expect(bool cond, const std::string& what, const std::string& where) {
    if (cond) {
        test_state().passed++;
        printf("  ok   %s\n", what.c_str());
    } else {
        test_state().failed++;
        printf("  FAIL %s   [%s]\n", what.c_str(), where.c_str());
        fflush(stdout);
    }
}

inline void register_test(const std::string& name, std::function<void()> fn) {
    test_registry().push_back({ name, std::move(fn) });
}

inline int Server::test_main() {
    test_state() = TestState();
    printf("\nHardScript test run\n");
    printf("==================\n\n");
    for (size_t i = 0; i < test_registry().size(); i++) {
        printf("[%zu/%zu] %s\n", i + 1, test_registry().size(), test_registry()[i].first.c_str());
        try {
            test_registry()[i].second();
        } catch (const std::exception& e) {
            expect(false, std::string("test raised: ") + e.what(), "runtime");
        }
        printf("\n");
    }
    printf("==================\n");
    printf("%d passed, %d failed\n", test_state().passed, test_state().failed);
    return test_state().failed == 0 ? 0 : 1;
}

// ===========================================================================
// PostgreSQL (native wire protocol)
// ===========================================================================
struct Pg {
    int fd = -1;
};

static uint32_t pg_read_int32(int fd) {
    uint32_t v = 0;
    ssize_t got = recv(fd, &v, 4, MSG_WAITALL);
    if (got != 4) throw std::runtime_error("postgres: connection lost");
    return (v >> 24) | ((v >> 8) & 0x0000ff00) | ((v << 8) & 0x00ff0000) | (v << 24);
}
static void pg_write_all(int fd, const std::string& data) {
    send_all(fd, data);
}

inline int pg_connect(std::string conninfo) {
    std::map<std::string, std::string> cfg;
    // parse key=value pairs (postgres-like) or URL
    size_t i = 0;
    std::string host = "127.0.0.1", user = getenv("USER") ? getenv("USER") : "postgres", dbname = user;
    int port = 5432;
    std::string password;
    auto set = [&](const std::string& k, const std::string& v) {
        if (k == "host") host = v;
        else if (k == "port") port = atoi(v.c_str());
        else if (k == "user") user = v;
        else if (k == "password") password = v;
        else if (k == "dbname") dbname = v;
    };
    // crude url parse
    if (conninfo.find("://") != std::string::npos) {
        size_t at = conninfo.rfind('@');
        size_t slash = conninfo.find('/', conninfo.find("://") + 3);
        std::string hostport = at == std::string::npos ? conninfo.substr(conninfo.find("://") + 3, slash == std::string::npos ? std::string::npos : slash - (conninfo.find("://") + 3)) : conninfo.substr(at + 1, slash == std::string::npos ? std::string::npos : slash - (at + 1));
        size_t colon = std::string::npos;
        for (size_t k = 0; k < hostport.size(); k++) if (hostport[k] == ':') { colon = k; break; }
        if (colon != std::string::npos) { host = hostport.substr(0, colon); port = atoi(hostport.substr(colon + 1).c_str()); }
        else host = hostport;
        if (at != std::string::npos) {
            std::string up = conninfo.substr(conninfo.find("://") + 3, at - (conninfo.find("://") + 3));
            size_t uc = up.find(':');
            user = uc == std::string::npos ? up : up.substr(0, uc);
            if (uc != std::string::npos) password = up.substr(uc + 1);
        }
        if (slash != std::string::npos) dbname = conninfo.substr(slash + 1);
    } else {
        std::string rest = conninfo;
        while (!rest.empty()) {
            size_t sp = rest.find(' ');
            std::string tok = rest.substr(0, sp == std::string::npos ? rest.size() : sp);
            size_t eq = tok.find('=');
            if (eq != std::string::npos) set(tok.substr(0, eq), tok.substr(eq + 1));
            if (sp == std::string::npos) break;
            rest = rest.substr(sp + 1);
        }
    }
    (void)cfg;
    Pg c;
    c.fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, host.c_str(), &a.sin_addr) != 1) {
        close(c.fd);
        throw std::runtime_error("postgres: invalid host " + host);
    }
    if (connect(c.fd, (struct sockaddr*)&a, sizeof a) != 0) {
        close(c.fd);
        throw std::runtime_error("postgres: cannot connect to " + host + ":" + std::to_string(port));
    }
    // startup message
    std::string start;
    start += '\0'; start += '\0'; start += (char)0; start += (char)3; // protocol
    auto addkv = [&](const std::string& k, const std::string& v) {
        start += k; start += '\0';
        start += v; start += '\0';
    };
    addkv("user", user);
    addkv("database", dbname);
    if (!host.empty()) addkv("host", host);
    if (!password.empty()) addkv("password", password);
    start += '\0';
    uint32_t len = (uint32_t)start.size() + 4;
    std::string msg;
    msg += (char)(len >> 24); msg += (char)(len >> 16); msg += (char)(len >> 8); msg += (char)len;
    msg += start;
    pg_write_all(c.fd, msg);
    // auth loop
    for (int iter = 0; iter < 4; iter++) {
        char typ;
        if (recv(c.fd, &typ, 1, MSG_WAITALL) != 1) throw std::runtime_error("postgres: no auth response");
        uint32_t l = pg_read_int32(c.fd);
        std::string payload((size_t)(l - 4), '\0');
        size_t got = 0;
        while (got < payload.size()) {
            ssize_t n = recv(c.fd, &payload[got], payload.size() - got, MSG_WAITALL);
            if (n <= 0) break;
            got += (size_t)n;
        }
        if (typ == 'R') {
            uint32_t code = ((uint8_t)payload[0] << 24) | ((uint8_t)payload[1] << 16) | ((uint8_t)payload[2] << 8) | (uint8_t)payload[3];
            if (code == 0) break; // ok
            if (code == 3) { // cleartext password
                std::string pass = password + '\0';
                std::string pmsg;
                pmsg += 'p';
                pmsg += (char)0; pmsg += (char)0; pmsg += (char)0; pmsg += (char)(pass.size() + 4);
                pmsg += pass;
                pg_write_all(c.fd, pmsg);
                continue;
            }
            if (code == 5) { // md5
                std::string salt = payload.substr(4, 4);
                std::string a = md5_hex(password + user);
                std::string b = md5_hex(a + salt);
                std::string pass = std::string("md5") + b + '\0';
                std::string pmsg;
                pmsg += 'p';
                pmsg += (char)0; pmsg += (char)0; pmsg += (char)0; pmsg += (char)(pass.size() + 4);
                pmsg += pass;
                pg_write_all(c.fd, pmsg);
                continue;
            }
            if (code == 10) {
                throw std::runtime_error("postgres: server requires SASL SCRAM auth, which HardScript does not support yet; configure the server to use md5 or trust. Password: not supported");
            }
            throw std::runtime_error("postgres: unsupported auth method " + std::to_string(code));
        } else if (typ == 'E') {
            // error: trailing status
            std::string msgbody;
            for (size_t k = 2; k + 1 < payload.size(); k += 2) {
                if (payload[k] == 'M' || payload[k] == 'S' || payload[k] == 'C' || payload[k] == 'F' ) {
                    msgbody += payload.substr(k + 1, payload.find('\0'));
                    msgbody += " ";
                }
            }
            throw std::runtime_error("postgres: " + msgbody);
        } else if (typ == 'Z') {
            break;
        }
    }
    printf("HardScript postgres connected\n");
    fflush(stdout);
    return c.fd;
}

// run a query, return rows as list of objects, or affected count
inline Val pg_query(int pfd, const std::string& sql) {
    std::string body = sql + '\0';
    std::string msg;
    msg += 'Q';
    msg += (char)0; msg += (char)0; msg += (char)0; msg += (char)(body.size() + 4);
    msg += body;
    pg_write_all(pfd, msg);
    std::vector<std::string> cols;
    bool cols_done = false;
    std::vector<Val> rows;
    int64_t affected = 0;
    bool have_error_tag = false;
    std::string err;
    for (;;) {
        char typ;
        if (recv(pfd, &typ, 1, MSG_WAITALL) != 1) throw std::runtime_error("postgres: connection lost during query");
        uint32_t l = pg_read_int32(pfd);
        std::string payload((size_t)(l - 4), '\0');
        size_t got = 0;
        while (got < payload.size()) {
            ssize_t n = recv(pfd, &payload[got], payload.size() - got, MSG_WAITALL);
            if (n <= 0) break;
            got += (size_t)n;
        }
        if (typ == 'T') { // row description
            uint16_t ncols = ((uint16_t)payload[0] << 8) | (uint8_t)payload[1];
            cols.clear();
            size_t off = 2;
            for (int k = 0; k < ncols; k++) {
                size_t n = 0;
                while (off + n + 1 <= payload.size() && payload[off + n] != '\0') n++;
                cols.push_back(payload.substr(off, n));
                off += n + 1;
                off += 18; // typid, typlen, typmod, format
            }
            (void)cols_done;
        } else if (typ == 'D') { // data row
            uint16_t nf = ((uint16_t)payload[0] << 8) | (uint8_t)payload[1];
            size_t off = 2;
            std::vector<std::pair<std::string, Val>> row;
            for (int k = 0; k < nf; k++) {
                int32_t flen = (int32_t)(((uint32_t)payload[off] << 24) | ((uint32_t)payload[off + 1] << 16) | ((uint32_t)payload[off + 2] << 8) | (uint8_t)payload[off + 3]);
                off += 4;
                std::string cname = k < (int)cols.size() ? cols[(size_t)k] : ("c" + std::to_string(k));
                if (flen < 0) {
                    row.emplace_back(cname, Val::nil());
                } else {
                    std::string v = payload.substr(off, (size_t)flen);
                    off += (size_t)flen;
                    Val boxed;
                    // try numeric detection: if it parses as int -> int
                    std::string d = v;
                    if (!d.empty()) {
                        bool allnum = true;
                        bool dot = false;
                        for (char cc : d) { if (!(cc >= '0' && cc <= '9')) { if (cc == '.' && !dot) { dot = true; } else { allnum = false; break; } } }
                        if (allnum && !d.empty()) {
                            if (dot) boxed = Val::flt(strtod(d.c_str(), nullptr));
                            else {
                                if (d.size() > 0 && d[0] == '0' && d.size() > 1) boxed = Val::text(d);
                                else boxed = Val::int_(strtoll(d.c_str(), nullptr, 10));
                            }
                        } else {
                            boxed = Val::text(d);
                        }
                    } else {
                        boxed = Val::text("");
                    }
                    row.emplace_back(cname, boxed);
                }
            }
            rows.push_back(Val::object(row));
        } else if (typ == 'C') { // command complete
            std::string tag;
            size_t n = 0;
            while (n < payload.size() && payload[n] != '\0') n++;
            tag = payload.substr(0, n);
            size_t sp = tag.rfind(' ');
            if (sp != std::string::npos) affected = strtoll(tag.substr(sp + 1).c_str(), nullptr, 10);
        } else if (typ == 'E') {
            std::string mb;
            for (size_t k = 1; k + 1 < payload.size(); k += 2) {
                if (payload[k] == 'M') {
                    size_t n = 0;
                    while (k + 1 + n < payload.size() && payload[k + 1 + n] != '\0') n++;
                    mb = payload.substr(k + 1, n);
                    break;
                }
            }
            throw std::runtime_error("postgres error: " + mb);
        } else if (typ == 'Z') {
            break;
        }
    }
    if (!cols.empty() || !rows.empty()) return Val::list(rows);
    return Val::int_(affected);
}

// ===========================================================================
// CLI helpers used by generated code
// ===========================================================================
inline std::vector<std::string>& g_args() {
    static std::vector<std::string> v;
    return v;
}
inline void set_args(int argc, char** argv) {
    g_args().clear();
    for (int i = 1; i < argc; i++) g_args().push_back(argv[i]);
}
inline int argc() { return (int)g_args().size(); }
inline std::string argv(int i) {
    if (i < 0 || i >= (int)g_args().size()) return "";
    return g_args()[(size_t)i];
}
inline std::string pid() { return std::to_string((long long)getpid()); }
inline std::string hostname() {
    char b[256];
    return gethostname(b, sizeof b) == 0 ? std::string(b) : "";
}
inline std::string platform() {
#if defined(__linux__)
    return "linux";
#elif defined(__APPLE__)
    return "macos";
#elif defined(_WIN32)
    return "windows";
#else
    return "unix";
#endif
}
inline int64_t cpus() {
    long n = sysconf(_SC_NPROCESSORS_ONLN);
    return n > 0 ? n : 1;
}

// ===========================================================================
// Codegen helpers (used by generated C++)
// ===========================================================================
inline std::string random_uuid() {
    std::string h = random_hex(16);
    h[12] = '4';
    h[16] = ('8' + (h[16] % 2)); // variant 10xx
    return h.substr(0, 8) + "-" + h.substr(8, 4) + "-" + h.substr(12, 4) + "-" +
           h.substr(16, 4) + "-" + h.substr(20, 12);
}

// Turn a HardScript route/fn value into an HTTP response: strings become
// text responses, maps/lists become JSON, nil becomes 204.
inline Response hs_respond(const Val& v) {
    if (v.is_nil()) return Response::empty(204);
    if (v.is_str()) return Response::text(v.sv);
    return Response::json(v);
}

inline Val args_list() {
    std::vector<Val> v;
    for (int i = 0; i < argc(); i++) v.push_back(Val::text(argv(i)));
    return Val::list(std::move(v));
}

inline bool want_test() {
    for (int i = 0; i < argc(); i++) {
        std::string a = argv(i);
        if (a == "--test" || a == "test") return true;
    }
    return false;
}

inline Server*& g_server() {
    static Server* s = nullptr;
    return s;
}
inline void hs_set_server(Server* s) { g_server() = s; }
inline Val test_call(const std::string& verb, const std::string& path, const Val& body) {
    Server* s = g_server();
    if (!s) throw std::runtime_error("http call used outside the running app");
    return s->call(verb, path, body);
}

// Run tasks concurrently and return the first result to complete.
inline Val race_val(std::vector<std::function<Val()>> tasks) {
    const size_t n = tasks.size();
    std::vector<Val> results(n);
    Val first;
    std::mutex m;
    bool done = false;
    if (n == 1) return tasks[0]();
    std::vector<std::thread> ts;
    ts.reserve(n);
    for (size_t i = 0; i < n; i++) {
        ts.emplace_back([&, i]() {
            Val r;
            try { r = tasks[i](); }
            catch (...) { r = Val::nil(); }
            std::lock_guard<std::mutex> lk(m);
            results[i] = r;
            if (!done) { first = r; done = true; }
        });
    }
    for (auto& t : ts) t.join();
    return first;
}

} // namespace hs