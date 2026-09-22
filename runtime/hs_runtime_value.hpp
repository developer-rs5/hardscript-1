#ifndef HS_RUNTIME_VALUE_HPP
#define HS_RUNTIME_VALUE_HPP
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

#endif
