#ifndef HS_RUNTIME_VALIDATION_HPP
#define HS_RUNTIME_VALIDATION_HPP
// ===========================================================================
// Validation engine (v0.6 M5.1)
// ===========================================================================
// Native validators with no third-party dependency: Email, URL, UUID, IP,
// Phone, Regex, Min, Max, Length, Enum and Nullable, plus the type check
// implied by the field's declared type.
//
// The compiler lowers a `model` declaration into a `VSchema` registration
// (see compiler/src/codegen.rs). Routes that bind a model to their body
// parameter get automatic validation: a failure short-circuits the handler
// and returns HTTP 400 with a JSON error list.
//
//   POST "/register" :: (body = Register) { ... }
//   // body is validated against the Register schema before `{ ... }` runs
//
#include "hs_runtime_value.hpp"
#include "hs_runtime_http.hpp"

// <regex> is pulled in by hs_runtime_value.hpp: every header in this runtime
// is included after `namespace hs { using namespace std; }` has been opened,
// so a standard header first included here would be parsed inside `hs`.
#include <initializer_list>

namespace hs {

// ---------------------------------------------------------------------------
// Rule specification
// ---------------------------------------------------------------------------

// Canonical lowercase type names. `String`/`Str`/`Text` all normalize to
// "string", so the same schema works regardless of the source spelling.
enum class VType { Any, String, Int, Float, Bool, List, Object, Time };

inline VType vtype_of(const std::string& ty) {
    if (ty == "String" || ty == "Str" || ty == "Text" || ty == "string") return VType::String;
    if (ty == "Int" || ty == "Integer" || ty == "int") return VType::Int;
    if (ty == "Float" || ty == "Float64" || ty == "Number" || ty == "float") return VType::Float;
    if (ty == "Bool" || ty == "Boolean" || ty == "bool") return VType::Bool;
    if (ty == "List" || ty == "Array" || ty == "list") return VType::List;
    if (ty == "Object" || ty == "Map" || ty == "Json" || ty == "object") return VType::Object;
    if (ty == "Time" || ty == "DateTime" || ty == "Timestamp" || ty == "Date" || ty == "time")
        return VType::String; // serialized as an ISO-8601 string
    return VType::Any;
}

struct VParam {
    double lo = 0;
    double hi = 0;
    bool has_hi = false;
    std::string text;
    std::vector<std::string> list;
    size_t rx = 0; // index into the shared compiled-regex cache
};

struct VRuleSpec {
    const char* name = "";
    VParam p;
};

// --- rule constructors used by generated code ---

inline VRuleSpec r_email() { VRuleSpec r; r.name = "email"; return r; }
inline VRuleSpec r_url() { VRuleSpec r; r.name = "url"; return r; }
inline VRuleSpec r_uuid() { VRuleSpec r; r.name = "uuid"; return r; }
inline VRuleSpec r_ip() { VRuleSpec r; r.name = "ip"; return r; }
inline VRuleSpec r_phone() { VRuleSpec r; r.name = "phone"; return r; }
inline VRuleSpec r_required() { VRuleSpec r; r.name = "required"; return r; }
inline VRuleSpec r_nullable() { VRuleSpec r; r.name = "nullable"; return r; }
inline VRuleSpec r_min(double v) { VRuleSpec r; r.name = "min"; r.p.lo = v; return r; }
inline VRuleSpec r_max(double v) { VRuleSpec r; r.name = "max"; r.p.lo = v; return r; }
inline VRuleSpec r_minmax(double lo, double hi) {
    VRuleSpec r; r.name = "range"; r.p.lo = lo; r.p.hi = hi; r.p.has_hi = true; return r;
}
inline VRuleSpec r_length(size_t lo, size_t hi) {
    VRuleSpec r; r.name = "length"; r.p.lo = (double)lo; r.p.hi = (double)hi; r.p.has_hi = true;
    return r;
}
inline VRuleSpec r_enum(std::initializer_list<const char*> vals) {
    VRuleSpec r; r.name = "enum";
    for (const char* v : vals) r.p.list.emplace_back(v);
    return r;
}
/// One enum member, accumulated across `Enum("a", "b", "c")`.
inline VRuleSpec r_enum_one(const char* v) {
    VRuleSpec r; r.name = "enum";
    r.p.list.emplace_back(v);
    r.p.has_hi = true; // marks "partial list, merge with siblings"
    return r;
}
/// Element type of a list: `List(Int)` validates every element.
inline VRuleSpec r_items(const char* ty) {
    VRuleSpec r; r.name = "items";
    r.p.text = ty;
    return r;
}
inline VRuleSpec r_enum_str(const std::string& csv) {
    VRuleSpec r; r.name = "enum";
    std::string cur;
    for (char c : csv) {
        if (c == ',') { r.p.list.push_back(cur); cur.clear(); }
        else cur += c;
    }
    if (!cur.empty()) r.p.list.push_back(cur);
    return r;
}

// Compiled-regex handle for `Regex("...")` / `regex="..."` constraints.
size_t v_regex_id(const std::string& pattern);
inline VRuleSpec r_regex(const std::string& pattern) {
    VRuleSpec r; r.name = "regex"; r.p.text = pattern; r.p.rx = v_regex_id(pattern); return r;
}

/// Collapse per-member `enum` rules into one (see `merge_enum_rules` below).
inline void merge_enum_rules(std::vector<VRuleSpec>& rules);

struct VField {
    std::string name;
    std::string type;   // source spelling, kept for messages
    VType vtype = VType::Any;
    std::vector<VRuleSpec> rules;
    bool nullable = false;
    // Presence is opt-in: `name : Str` validates the value when it is present
    // but does not demand it. Write `Str!` (or the `required` constraint) to
    // reject a missing key. The shipped v0.5 examples declare `model` bodies
    // without presence markers, so required-by-default would break them.
    bool required = false;
};

inline VField vf(std::string name, std::string type,
                 std::initializer_list<VRuleSpec> rules = {}) {
    VField f;
    f.name = std::move(name);
    f.vtype = vtype_of(type);
    // Email / Url / Uuid / Ip / Phone are dedicated types: the format check is
    // implied by the type, so no explicit rule is required in the source.
    const std::string& t = type;
    if (t == "Email") f.rules.push_back(r_email());
    else if (t == "Url" || t == "URL") f.rules.push_back(r_url());
    else if (t == "Uuid" || t == "UUID") f.rules.push_back(r_uuid());
    else if (t == "Ip" || t == "IP") f.rules.push_back(r_ip());
    else if (t == "Phone") f.rules.push_back(r_phone());
    for (const auto& r : rules) f.rules.push_back(r);
    for (const auto& r : f.rules) {
        if (std::strcmp(r.name, "nullable") == 0) f.nullable = true;
        if (std::strcmp(r.name, "required") == 0) f.required = true;
    }
    merge_enum_rules(f.rules);
    return f;
}

struct VSchema {
    std::string name;
    std::vector<VField> fields;
    bool strict = false; // report unknown keys
};

/// `Enum("a", "b", "c")` lowers to one rule per member; collapse them back
/// into a single `enum` rule so the member list is checked as a unit.
inline void merge_enum_rules(std::vector<VRuleSpec>& rules) {
    VRuleSpec merged;
    merged.name = "enum";
    bool any = false;
    std::vector<VRuleSpec> rest;
    rest.reserve(rules.size());
    for (auto& r : rules) {
        if (std::strcmp(r.name, "enum") == 0) {
            for (auto& v : r.p.list) merged.p.list.push_back(v);
            any = true;
        } else {
            rest.push_back(r);
        }
    }
    if (!any) return;
    rest.push_back(std::move(merged));
    rules.swap(rest);
}

// ---------------------------------------------------------------------------
// Schema registry (populated by generated code before main() runs)
// ---------------------------------------------------------------------------

struct SchemaRegistry {
    std::mutex m;
    std::vector<VSchema> items;
    const VSchema* find(const std::string& n) const {
        for (auto& s : items)
            if (s.name == n) return &s;
        return nullptr;
    }
    void add(VSchema s) {
        std::lock_guard<std::mutex> g(m);
        for (auto& e : items)
            if (e.name == s.name) { e = std::move(s); return; }
        items.push_back(std::move(s));
    }
    void clear() { std::lock_guard<std::mutex> g(m); items.clear(); }
};
inline SchemaRegistry& schema_registry() {
    static SchemaRegistry r;
    return r;
}

inline void register_schema(std::string name, std::vector<VField> fields, bool strict = false) {
    VSchema s;
    s.name = std::move(name);
    s.fields = std::move(fields);
    s.strict = strict;
    schema_registry().add(std::move(s));
}

// ---------------------------------------------------------------------------
// Format validators
// ---------------------------------------------------------------------------

inline bool is_alpha(char c) { return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z'); }
inline bool is_digit(char c) { return c >= '0' && c <= '9'; }
inline bool is_alnum(char c) { return is_alpha(c) || is_digit(c); }
inline bool is_hex(char c) {
    return is_digit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F');
}

// RFC-5322-shaped subset: `local@domain.tld`, no whitespace, one `@`, domain
// labels of alphanumerics and interior hyphens, TLD of >= 2 ASCII letters.
inline bool v_is_email(const std::string& s) {
    size_t at = s.find('@');
    if (at == std::string::npos || at == 0 || at + 1 >= s.size()) return false;
    if (s.size() > 254) return false;
    const std::string local = s.substr(0, at);
    const std::string domain = s.substr(at + 1);
    if (local.size() > 64) return false;
    bool ld = false;
    for (char c : local) {
        if (is_alnum(c) || c == '.' || c == '_' || c == '-' || c == '+') ld = true;
        else return false;
    }
    if (!ld) return false;
    if (local.front() == '.' || local.back() == '.' || local.find("..") != std::string::npos)
        return false;
    size_t dot = domain.rfind('.');
    if (dot == std::string::npos || dot == 0 || dot + 1 >= domain.size()) return false;
    std::string tld = domain.substr(dot + 1);
    if (tld.size() < 2) return false;
    for (char c : tld)
        if (!is_alpha(c)) return false;
    size_t start = 0;
    while (start < domain.size()) {
        size_t end = domain.find('.', start);
        if (end == std::string::npos) end = domain.size();
        if (end == start) return false;
        if (domain[start] == '-' || domain[end - 1] == '-') return false;
        for (size_t i = start; i < end; i++)
            if (!is_alnum(domain[i]) && domain[i] != '-') return false;
        start = end + 1;
    }
    return true;
}

// `scheme://host[:port][/path][?query][#fragment]` with an explicit scheme.
inline bool v_is_url(const std::string& s) {
    if (s.size() < 8 || s.size() > 2048) return false;
    for (char c : s)
        if ((unsigned char)c <= 0x20 || c == 0x7f) return false;
    size_t sep = s.find("://");
    if (sep == std::string::npos || sep == 0) return false;
    std::string scheme = s.substr(0, sep);
    for (char c : scheme) {
        if (!is_alnum(c) && c != '+' && c != '-' && c != '.') return false;
    }
    scheme[0] = fold_ascii(scheme[0]);
    if (scheme != "http" && scheme != "https" && scheme != "ws" && scheme != "wss" &&
        scheme != "ftp" && scheme != "ftps")
        return false;
    size_t rest = sep + 3;
    size_t host_end = rest;
    while (host_end < s.size()) {
        char c = s[host_end];
        if (c == '/' || c == '?' || c == '#') break;
        host_end++;
    }
    std::string hostport = s.substr(rest, host_end - rest);
    if (hostport.empty()) return false;
    std::string host = hostport;
    if (hostport.front() == '[') { // IPv6 literal
        size_t close = hostport.find(']');
        if (close == std::string::npos) return false;
        host = hostport.substr(1, close - 1);
        std::string tail = hostport.substr(close + 1);
        if (!tail.empty() && (tail[0] != ':' || tail.size() < 2)) return false;
    } else {
        size_t colon = hostport.rfind(':');
        if (colon != std::string::npos) {
            std::string port = hostport.substr(colon + 1);
            if (port.empty() || port.size() > 5) return false;
            for (char c : port)
                if (!is_digit(c)) return false;
            long p = std::strtol(port.c_str(), nullptr, 10);
            if (p < 1 || p > 65535) return false;
            host = hostport.substr(0, colon);
        }
    }
    if (host.empty()) return false;
    for (char c : host) {
        if (!is_alnum(c) && c != '.' && c != '-' && c != ':' && c != '_' && c != '%')
            return false;
    }
    return true;
}

// 8-4-4-4-12 lowercase-or-uppercase hex with dashes at offsets 8/13/18/23.
inline bool v_is_uuid(const std::string& s) {
    if (s.size() != 36) return false;
    for (size_t i = 0; i < s.size(); i++) {
        if (i == 8 || i == 13 || i == 18 || i == 23) {
            if (s[i] != '-') return false;
            continue;
        }
        if (!is_hex(s[i])) return false;
    }
    return true;
}

// IPv4 dotted quad with octet range checks, or an IPv6 address (`::` elision
// and embedded IPv4 accepted).
inline bool v_is_ipv4(const std::string& s) {
    int parts = 0, i = 0;
    while (i < (int)s.size()) {
        if (!is_digit(s[i])) return false;
        int val = 0, digits = 0;
        while (i < (int)s.size() && is_digit(s[i])) {
            val = val * 10 + (s[i] - '0');
            i++;
            if (++digits > 3) return false;
        }
        if (val > 255) return false;
        parts++;
        if (i == (int)s.size()) break;
        if (s[i] != '.') return false;
        i++;
        if (parts == 4) return false;
    }
    return parts == 4;
}

inline bool v_is_ipv6(const std::string& s) {
    if (s.find(':') == std::string::npos) return false;
    size_t dbl = s.find("::");
    if (dbl != std::string::npos && s.find("::", dbl + 1) != std::string::npos) return false;
    int groups = 0;
    bool saw_dc = false;
    size_t i = 0;
    while (i < s.size()) {
        if (s[i] == ':') {
            if (i + 1 < s.size() && s[i + 1] == ':') { saw_dc = true; i += 2; continue; }
            if (i == 0) return false;
            i++;
            continue;
        }
        size_t st = i;
        int hex = 0;
        while (i < s.size() && is_hex(s[i]) && hex < 5) { hex++; i++; }
        if (i < s.size() && s[i] == '.') { // embedded IPv4 tail
            if (!v_is_ipv4(s.substr(st))) return false;
            groups += 2;
            i = s.size();
            break;
        }
        if (hex == 0 || hex > 4) return false;
        groups++;
    }
    if (saw_dc) return groups <= 8;
    return groups == 8;
}

inline bool v_is_ip(const std::string& s) { return v_is_ipv4(s) || v_is_ipv6(s); }

// E.164 / North-American shaped: optional `+`, 7..15 digits, with spaces,
// dashes, dots and parentheses ignored.
inline bool v_is_phone(const std::string& s) {
    int digits = 0;
    bool plus = false;
    for (size_t i = 0; i < s.size(); i++) {
        char c = s[i];
        if (is_digit(c)) { digits++; continue; }
        if (c == '+') {
            if (i != 0 || plus) return false;
            plus = true;
            continue;
        }
        if (c == ' ' || c == '-' || c == '.' || c == '(' || c == ')' || c == '/') continue;
        return false;
    }
    return digits >= 7 && digits <= 15;
}

// ---------------------------------------------------------------------------
// Compiled regex cache
// ---------------------------------------------------------------------------

struct RegexCache {
    std::mutex m;
    std::vector<std::pair<std::string, std::shared_ptr<std::regex>>> items;
};
inline RegexCache& regex_cache() {
    static RegexCache c;
    return c;
}

inline size_t v_regex_id(const std::string& pattern) {
    RegexCache& c = regex_cache();
    std::lock_guard<std::mutex> g(c.m);
    for (size_t i = 0; i < c.items.size(); i++)
        if (c.items[i].first == pattern) return i;
    auto re = std::make_shared<std::regex>(pattern, std::regex::ECMAScript);
    c.items.emplace_back(pattern, re);
    return c.items.size() - 1;
}

inline bool v_regex_test(size_t id, const std::string& subject) {
    RegexCache& c = regex_cache();
    std::lock_guard<std::mutex> g(c.m);
    if (id >= c.items.size()) return false;
    return std::regex_search(subject, *c.items[id].second);
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

struct VErr {
    std::string field;
    std::string rule;
    std::string message;
};

struct VResult {
    bool ok = true;
    std::vector<VErr> errors;

    void add(std::string field, const char* rule, std::string msg) {
        ok = false;
        errors.push_back({std::move(field), rule, std::move(msg)});
    }
    // `{ ok: false, errors: [{ field, rule, message }] }`
    Val to_val() const {
        std::vector<Val> es;
        es.reserve(errors.size());
        for (auto& e : errors) {
            Val o = Val::object({});
            o.set("field", Val::text(e.field));
            o.set("rule", Val::text(e.rule));
            o.set("message", Val::text(e.message));
            es.push_back(std::move(o));
        }
        Val out = Val::object({});
        out.set("ok", Val::boolean(ok));
        out.set("errors", Val::list(std::move(es)));
        out.set("count", Val::int_((int64_t)errors.size()));
        return out;
    }
};

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

inline VResult validate_fields(const std::vector<VField>& fields, const Val& input,
                               bool strict = false) {
    VResult res;
    if (!input.is_obj()) {
        res.add("", "type", "expected an object, got " + input.kind_name());
        return res;
    }
    for (const auto& f : fields) {
        const Val* v = input.find(f.name);
        bool present = v != nullptr;
        if (!present || v->is_nil()) {
            if (f.nullable) continue;
            if (f.required) res.add(f.name, "required", f.name + " is required");
            continue;
        }
        // type check
        switch (f.vtype) {
            case VType::String:
                if (!v->is_str())
                    res.add(f.name, "type",
                            f.name + " must be a string, got " + v->kind_name());
                break;
            case VType::Int:
                if (!v->is_int())
                    res.add(f.name, "type",
                            f.name + " must be an integer, got " + v->kind_name());
                break;
            case VType::Float:
                if (!v->is_num())
                    res.add(f.name, "type", f.name + " must be a number, got " + v->kind_name());
                break;
            case VType::Bool:
                if (!v->is_bool())
                    res.add(f.name, "type",
                            f.name + " must be a boolean, got " + v->kind_name());
                break;
            case VType::List:
                if (!v->is_arr())
                    res.add(f.name, "type", f.name + " must be a list, got " + v->kind_name());
                break;
            case VType::Object:
                if (!v->is_obj())
                    res.add(f.name, "type",
                            f.name + " must be an object, got " + v->kind_name());
                break;
            default:
                break;
        }
        if (!res.ok && res.errors.size() && res.errors.back().field == f.name &&
            res.errors.back().rule == "type")
            continue; // don't cascade format rules over a wrong type
        for (const auto& r : f.rules) {
            const std::string rn = r.name;
            if (rn == "email") {
                if (v->is_str() && !v_is_email(v->sv))
                    res.add(f.name, "email", f.name + " must be a valid email address");
            } else if (rn == "url") {
                if (v->is_str() && !v_is_url(v->sv))
                    res.add(f.name, "url", f.name + " must be a valid URL");
            } else if (rn == "uuid") {
                if (v->is_str() && !v_is_uuid(v->sv))
                    res.add(f.name, "uuid", f.name + " must be a valid UUID");
            } else if (rn == "ip") {
                if (v->is_str() && !v_is_ip(v->sv))
                    res.add(f.name, "ip", f.name + " must be a valid IP address");
            } else if (rn == "phone") {
                if (v->is_str() && !v_is_phone(v->sv))
                    res.add(f.name, "phone", f.name + " must be a valid phone number");
            } else if (rn == "regex") {
                if (v->is_str() && !v_regex_test(r.p.rx, v->sv))
                    res.add(f.name, "regex",
                            f.name + " must match " + (r.p.text.empty() ? "the pattern" : r.p.text));
            } else if (rn == "min") {
                double n = 0;
                bool ok = false;
                if (v->is_num()) { n = v->num(); ok = true; }
                else if (v->is_str()) { n = (double)v->sv.size(); ok = true; }
                else if (v->is_arr()) { n = (double)v->arr.size(); ok = true; }
                if (ok && n < r.p.lo)
                    res.add(f.name, "min",
                            f.name + " must be at least " + std::to_string((long long)r.p.lo));
            } else if (rn == "max") {
                double n = 0;
                bool ok = false;
                if (v->is_num()) { n = v->num(); ok = true; }
                else if (v->is_str()) { n = (double)v->sv.size(); ok = true; }
                else if (v->is_arr()) { n = (double)v->arr.size(); ok = true; }
                if (ok && n > r.p.lo)
                    res.add(f.name, "max",
                            f.name + " must be at most " + std::to_string((long long)r.p.lo));
            } else if (rn == "range") {
                if (v->is_num() && (v->num() < r.p.lo || v->num() > r.p.hi))
                    res.add(f.name, "range",
                            f.name + " must be between " + std::to_string((long long)r.p.lo) +
                                " and " + std::to_string((long long)r.p.hi));
            } else if (rn == "length") {
                double n = v->is_num() ? (double)(long long)v->as_int()
                                       : (v->is_str() ? (double)v->sv.size()
                                                      : (v->is_arr() ? (double)v->arr.size() : -1.0));
                if (n >= 0 && (n < r.p.lo || n > r.p.hi))
                    res.add(f.name, "length",
                            f.name + " length must be between " +
                                std::to_string((long long)r.p.lo) + " and " +
                                std::to_string((long long)r.p.hi));
            } else if (rn == "enum") {
                bool ok = false;
                for (auto& allowed : r.p.list) {
                    if (v->is_str() && v->sv == allowed) { ok = true; break; }
                    if (v->is_int() && std::to_string(v->iv) == allowed) { ok = true; break; }
                    if (v->is_bool() && ((v->bv && allowed == "true") ||
                                         (!v->bv && allowed == "false"))) {
                        ok = true;
                        break;
                    }
                }
                if (!ok) {
                    std::string allowed_list;
                    for (size_t i = 0; i < r.p.list.size(); i++) {
                        if (i) allowed_list += ", ";
                        allowed_list += r.p.list[i];
                    }
                    res.add(f.name, "enum",
                            f.name + " must be one of: " + allowed_list);
                }
            } else if (rn == "items") {
                if (v->is_arr()) {
                    VType et = vtype_of(r.p.text);
                    for (size_t i = 0; i < v->arr.size(); i++) {
                        const Val& e = v->arr[i];
                        bool ok = true;
                        switch (et) {
                            case VType::String: ok = e.is_str(); break;
                            case VType::Int: ok = e.is_int(); break;
                            case VType::Float: ok = e.is_num(); break;
                            case VType::Bool: ok = e.is_bool(); break;
                            case VType::List: ok = e.is_arr(); break;
                            case VType::Object: ok = e.is_obj(); break;
                            default: ok = true; break;
                        }
                        if (ok && et != VType::Any) {
                            // Format-typed elements reuse the scalar rules.
                            const std::string& t = r.p.text;
                            if (t == "Email" && e.is_str() && !v_is_email(e.sv)) ok = false;
                            else if ((t == "Url" || t == "URL") && e.is_str() && !v_is_url(e.sv)) ok = false;
                            else if ((t == "Uuid" || t == "UUID") && e.is_str() && !v_is_uuid(e.sv)) ok = false;
                            else if ((t == "Ip" || t == "IP") && e.is_str() && !v_is_ip(e.sv)) ok = false;
                            else if (t == "Phone" && e.is_str() && !v_is_phone(e.sv)) ok = false;
                        }
                        if (!ok) {
                            res.add(f.name, "items",
                                    f.name + "[" + std::to_string(i) + "] must be " +
                                        r.p.text);
                            break;
                        }
                    }
                }
            }
        }
    }
    if (strict) {
        for (auto& kv : input.obj) {
            bool known = false;
            for (auto& f : fields)
                if (f.name == kv.first) { known = true; break; }
            if (!known) res.add(kv.first, "unknown", "unknown field " + kv.first);
        }
    }
    return res;
}

inline VResult validate(const std::string& schema, const Val& input) {
    const VSchema* s = schema_registry().find(schema);
    if (!s) {
        VResult r;
        r.add("", "schema", "unknown schema " + schema);
        return r;
    }
    return validate_fields(s->fields, input, s->strict);
}

inline bool valid(const std::string& schema, const Val& input) {
    return validate(schema, input).ok;
}

/// HTTP 400 body for a failed validation, matching the framework contract:
///
///     {"error":"validation_failed","status":400,"errors":[{...}]}
inline Response validation_response(const VResult& r) {
    Response resp;
    resp.status = 400;
    resp.ctype = "application/json; charset=utf-8";
    Val out = Val::object({});
    Val detail = r.to_val();
    out.set("error", Val::text("validation_failed"));
    out.set("status", Val::int_(400));
    const Val* es = detail.find("errors");
    out.set("errors", es ? *es : Val::list({}));
    out.set("count", Val::int_((int64_t)r.errors.size()));
    resp.json_v = std::move(out);
    resp.has_json = true;
    return resp;
}

/// Convenience: validate and return 400 when invalid, `ok` otherwise.
inline bool validation_gate(const std::string& schema, const Val& input, Response& out) {
    VResult r = validate(schema, input);
    if (r.ok) return true;
    out = validation_response(r);
    return false;
}

// ---------------------------------------------------------------------------
// Ad-hoc validation (no schema)
// ---------------------------------------------------------------------------

/// `true` when the value is nil or JSON `null`.
inline bool v_is_null(const Val& v) { return v.is_nil(); }

/// A format predicate on an empty string is always false, so the source-level
/// `validation.email(x)` etc. guard against reporting a match for "".
inline bool v_nonempty(const std::string& s) { return !s.empty(); }

/// Length used by `min` / `max` / `length` rules: text and lists are measured
/// by element count, numbers by value.
inline double v_measure(const Val& v, bool& ok) {
    if (v.is_num()) { ok = true; return v.num(); }
    if (v.is_str()) { ok = true; return (double)v.sv.size(); }
    if (v.is_arr()) { ok = true; return (double)v.arr.size(); }
    ok = false;
    return 0;
}

inline bool v_min(const Val& v, double lo) {
    bool ok = false;
    double n = v_measure(v, ok);
    return ok && n >= lo;
}
inline bool v_max(const Val& v, double hi) {
    bool ok = false;
    double n = v_measure(v, ok);
    return ok && n <= hi;
}
inline bool v_length(const Val& v, double lo, double hi) {
    bool ok = false;
    double n = v_measure(v, ok);
    return ok && n >= lo && n <= hi;
}
inline bool v_between(const Val& v, double lo, double hi) {
    return v.is_num() && v.num() >= lo && v.num() <= hi;
}
inline bool v_one_of(const Val& v, const std::string& csv) {
    std::string cur;
    std::string target = v.is_str() ? v.sv : (v.is_num() ? std::to_string((long long)v.as_int())
                                                        : std::string());
    bool any = false;
    for (size_t i = 0; i <= csv.size(); i++) {
        if (i == csv.size() || csv[i] == ',') {
            if (!cur.empty() && cur == target) { any = true; break; }
            cur.clear();
        } else {
            cur += csv[i];
        }
    }
    return any;
}

/// Unwind the request with a 400 built from an existing validation result.
[[noreturn]] inline void validation_reject(const Val& result) {
    const Val* errs = result.find("errors");
    Response resp;
    resp.status = 400;
    resp.ctype = "application/json; charset=utf-8";
    Val out = Val::object({});
    out.set("error", Val::text("validation_failed"));
    out.set("status", Val::int_(400));
    out.set("errors", errs ? *errs : Val::list({}));
    out.set("count", Val::int_((int64_t)(errs ? errs->size() : 0)));
    resp.json_v = std::move(out);
    resp.has_json = true;
    throw HttpAbort(std::move(resp));
}

} // namespace hs

#endif // HS_RUNTIME_VALIDATION_HPP
