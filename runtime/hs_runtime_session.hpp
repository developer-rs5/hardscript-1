// hs_runtime_session.hpp -- session manager (M6.4)
//
// Cookie sessions: the session lives in the cookie, signed, not on the
// server. `session.start(user)` signs `{user, flash, sid, iat}` into
// `hs_session`; `session.user()` verifies and returns the user, or nil for
// visitors, expired cookies and forgeries (absence is normal traffic, not an
// error). Every read re-issues the cookie with a fresh Max-Age — rolling
// expiration. `session.destroy()` emits an expired cookie instead.
// `session.flash(k)` reads once, `session.flash(k, v)` stashes for the next
// read. `session.csrf()` binds a token to the session id.
//
// Handlers return values, not responses, so a route cannot set headers
// itself. Session calls stash `Set-Cookie` lines in thread-local pending
// state; `hs_respond` drains them onto the response, and the server clears
// them at dispatch start so an exception path never leaks one request's
// cookie into the next. The HMAC secret comes from `SESSION_SECRET`, or a
// per-process random value when unset (sessions die with restarts, but no
// two deployments share a key).

#ifndef HS_RUNTIME_SESSION_HPP
#define HS_RUNTIME_SESSION_HPP

#include "hs_runtime_crypto.hpp"
#include "hs_runtime_http.hpp"
#include "hs_runtime_io.hpp"
#include "hs_runtime_value.hpp"

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <string>
#include <vector>

namespace hs {

constexpr const char* SESSION_COOKIE = "hs_session";
constexpr int64_t SESSION_MAX_AGE = 86400;  // rolling window, one day

/// The HMAC secret: `SESSION_SECRET` when set, else a per-process random
/// value. Tests pin it through `session_set_secret_for_tests` so signatures
/// are deterministic.
inline const std::string& session_secret() {
    static const std::string secret = [] {
        std::string from_env = env_get("SESSION_SECRET");
        if (!from_env.empty()) return from_env;
        return std::string("dev-") + random_hex(32);
    }();
    return secret;
}

inline std::string& session_test_secret() {
    static std::string s;
    return s;
}

inline void session_set_secret_for_tests(const std::string& s) { session_test_secret() = s; }

inline void session_clear_secret_for_tests() { session_test_secret().clear(); }

inline const std::string& session_active_secret() {
    if (!session_test_secret().empty()) return session_test_secret();
    return session_secret();
}

inline int64_t session_now_ms() {
    return (int64_t)std::chrono::duration_cast<std::chrono::milliseconds>(
               std::chrono::system_clock::now().time_since_epoch())
        .count();
}

/// One parsed session: the payload plus whether it has been read or written
/// this request (which decides re-issue).
struct Session {
    Val user;
    Val flash = Val::object({});
    std::string sid;
    int64_t iat_ms = 0;
    bool touched = false;
};

/// Per-request-thread session state. The server clears it at dispatch start;
/// `hs_respond` drains it onto the response.
struct SessionRequest {
    bool have_session = false;
    Session session;
    bool destroyed = false;
    std::vector<std::string> flash_consumed;
    std::vector<std::string> pending_cookies;
};

inline SessionRequest& session_request() {
    thread_local SessionRequest r;
    return r;
}

/// Called by the server at dispatch start: exceptions bypass `hs_respond`,
/// so without this a thrown request would leak its cookies into the next
/// request on the worker thread. Plugged into dispatch through the hook the
/// HTTP layer provides (null when sessions are not linked in).
inline void session_request_start() {
    session_request() = SessionRequest();
}

/// Sign a payload object into a cookie value: `v1.<body>.<sig>`. The version
/// prefix leaves room to rotate formats without breaking old readers loudly
/// (they fail closed: unknown version reads as absent).
inline std::string session_sign(const Val& payload) {
    std::string body = base64_encode_url(to_json(payload));
    std::string inner = std::string("v1.") + body;
    return inner + "." + base64_encode_url(hmac_sha256_bin(session_active_secret(), inner));
}

/// Verify a cookie value into a payload object. Anything malformed, unsigned,
/// or versioned-unknown reads as absent: forged cookies are normal hostile
/// traffic, not errors.
inline bool session_verify(const std::string& cookie, Val& payload) {
    std::vector<std::string> parts;
    std::string cur;
    for (char c : cookie) {
        if (c == '.') {
            parts.push_back(cur);
            cur.clear();
        } else {
            cur += c;
        }
    }
    parts.push_back(cur);
    if (parts.size() != 3 || parts[0] != "v1") return false;
    std::string expect =
        base64_encode_url(hmac_sha256_bin(session_active_secret(), parts[0] + "." + parts[1]));
    if (!ct_equal(expect, parts[2])) return false;
    try {
        payload = parse_json(base64_decode(parts[1]));
    } catch (...) {
        return false;
    }
    return payload.is_obj();
}

inline std::string session_cookie_header(const std::string& value, int64_t max_age) {
    // Secure unconditionally: localhost is a secure context in browsers, and
    // anything else serving plain HTTP has no business holding sessions.
    // HttpOnly keeps scripts out; Lax blocks cross-site sends without
    // breaking top-level navigation.
    std::string out = std::string(SESSION_COOKIE) + "=" + value +
                      "; Path=/; HttpOnly; SameSite=Lax; Secure";
    if (max_age >= 0) {
        out += "; Max-Age=" + std::to_string(max_age);
    }
    return out;
}

/// Read one cookie value out of a `Cookie` header. The last one wins, the
/// way browsers send them.
inline std::string session_cookie_from_header(const std::string& header, const std::string& name) {
    std::string found;
    size_t i = 0;
    while (i < header.size()) {
        while (i < header.size() && (header[i] == ' ' || header[i] == ';')) i++;
        size_t eq = header.find('=', i);
        if (eq == std::string::npos) break;
        std::string k = header.substr(i, eq - i);
        // Trim trailing spaces off the key (`a = 1` is legal).
        while (!k.empty() && k.back() == ' ') k.pop_back();
        size_t end = header.find(';', eq + 1);
        std::string v = header.substr(eq + 1, end == std::string::npos ? end : end - eq - 1);
        // Trim spaces; a quoted value keeps its quotes stripped one level.
        while (!v.empty() && v.front() == ' ') v.erase(v.begin());
        while (!v.empty() && v.back() == ' ') v.pop_back();
        if (v.size() >= 2 && v.front() == '"' && v.back() == '"') v = v.substr(1, v.size() - 2);
        if (k == name) found = v;
        if (end == std::string::npos) break;
        i = end + 1;
    }
    return found;
}

/// Load the request's session once per request, verifying the cookie. After
/// this, reads are free and writes mark for re-issue.
inline Session* session_load(const Request& req) {
    SessionRequest& r = session_request();
    if (r.destroyed) return nullptr;
    if (r.have_session) return &r.session;
    std::string raw = session_cookie_from_header(req.header("cookie"), SESSION_COOKIE);
    if (raw.empty()) return nullptr;
    Val payload;
    if (!session_verify(raw, payload)) return nullptr;
    Session s;
    if (const Val* u = payload.find("user")) s.user = *u;
    if (const Val* f = payload.find("flash")) {
        if (f->is_obj()) s.flash = *f;
    }
    if (const Val* id = payload.find("sid")) {
        if (id->is_str()) s.sid = id->sv;
    }
    if (const Val* iat = payload.find("iat")) {
        if (iat->is_int()) s.iat_ms = iat->iv;
    }
    if (s.sid.empty()) return nullptr;
    r.have_session = true;
    r.session = s;
    return &r.session;
}

inline void session_emit_current() {
    SessionRequest& r = session_request();
    Val payload = Val::object({});
    payload.set("user", r.session.user);
    Val flash = Val::object({});
    if (r.session.flash.is_obj()) {
        for (const auto& kv : r.session.flash.obj) {
            bool consumed = false;
            for (const auto& c : r.flash_consumed) {
                if (c == kv.first) {
                    consumed = true;
                    break;
                }
            }
            if (!consumed) flash.set(kv.first, kv.second);
        }
    }
    payload.set("flash", flash);
    payload.set("sid", Val::text(r.session.sid));
    payload.set("iat", Val::int_(r.session.iat_ms));
    r.pending_cookies.push_back(session_cookie_header(session_sign(payload), SESSION_MAX_AGE));
}

/// Drain pending cookies onto a response: called by `hs_respond` for every
/// route return, so handlers set cookies by calling session functions. A
/// session that moved since an earlier emit this request (flashes consumed,
/// values changed) is re-signed once: exactly one cookie leaves per request.
inline void session_drain_pending(Response& res) {
    SessionRequest& r = session_request();
    if (r.destroyed) {
        res.headers.push_back({"Set-Cookie", session_cookie_header("", 0)});
        r = SessionRequest();
        return;
    }
    if (!r.flash_consumed.empty() || (r.have_session && r.session.touched)) {
        r.pending_cookies.clear();
        session_emit_current();
    }
    for (const auto& c : r.pending_cookies) {
        res.headers.push_back({"Set-Cookie", c});
    }
    r = SessionRequest();
}

// ---------------------------------------------------------------------------
// Language surface. Every function takes the request for cookie input; codegen
// gates them to routes and middleware, where one exists.
// ---------------------------------------------------------------------------

/// `session.start(user)`: sign a fresh session around the user and queue its
/// cookie. Replaces any session already open: starting over means over.
inline Val session_start(const Request& req, const Val& user) {
    (void)req;
    SessionRequest& r = session_request();
    r.destroyed = false;
    r.flash_consumed.clear();
    r.pending_cookies.clear();
    r.have_session = true;
    r.session.user = user;
    r.session.flash = Val::object({});
    r.session.sid = random_hex(16);
    r.session.iat_ms = session_now_ms();
    r.session.touched = false;
    session_emit_current();
    return Val::boolean(true);
}

/// `session.user()`: the signed-in user, or nil for visitors, expired
/// cookies and forgeries. Reading marks for rolling re-issue.
inline Val session_user(const Request& req) {
    Session* s = session_load(req);
    if (!s) return Val::nil();
    s->touched = true;
    return s->user;
}

/// `session.destroy()`: forget the session and queue an expired cookie.
inline Val session_destroy(const Request& req) {
    (void)req;
    SessionRequest& r = session_request();
    r.destroyed = true;
    r.have_session = false;
    return Val::boolean(true);
}

/// `session.flash(key)` reads a flash value once; `session.flash(key, value)`
/// stashes one for the next read. Setting without a session starts an
/// anonymous one: flashes belong to visitors too.
inline Val session_flash(const Request& req, const Val& key, const Val& value) {
    if (!key.is_str() || key.sv.empty())
        throw std::runtime_error("session: flash needs a key as text");
    Session* s = session_load(req);
    if (value.is_nil()) {
        // Read once: the value comes out and is scheduled for deletion on
        // the way out, so the next request finds it gone.
        if (!s || !s->flash.is_obj()) return Val::nil();
        const Val* v = s->flash.find(key.sv);
        if (!v) return Val::nil();
        Val out = *v;
        s->touched = true;
        session_request().flash_consumed.push_back(key.sv);
        return out;
    }
    if (!s) {
        SessionRequest& r = session_request();
        r.have_session = true;
        r.session.user = Val::nil();
        r.session.flash = Val::object({});
        r.session.sid = random_hex(16);
        r.session.iat_ms = session_now_ms();
        s = &r.session;
    }
    if (!s->flash.is_obj()) s->flash = Val::object({});
    s->flash.set(key.sv, value);
    s->touched = true;
    // A consumed-then-reset key is alive again: un-schedule its deletion.
    auto& consumed = session_request().flash_consumed;
    consumed.erase(std::remove(consumed.begin(), consumed.end(), key.sv), consumed.end());
    return Val::boolean(true);
}

/// `session.csrf()`: a token bound to the session id. Present it back with
/// `session.csrf_valid(token)`.
inline Val session_csrf(const Request& req) {
    Session* s = session_load(req);
    if (!s) throw std::runtime_error("session: csrf needs a session; start one first");
    s->touched = true;
    return Val::text(base64_encode_url(hmac_sha256_bin(session_active_secret(), "csrf:" + s->sid)));
}

/// `session.csrf_valid(token)`: true when the token is this session's own.
/// No session, no truth: validation without a session is always false rather
/// than an error, because logged-out forms validate too.
inline Val session_csrf_valid(const Request& req, const Val& token) {
    Session* s = session_load(req);
    if (!s || !token.is_str()) return Val::boolean(false);
    std::string expect =
        base64_encode_url(hmac_sha256_bin(session_active_secret(), "csrf:" + s->sid));
    return Val::boolean(ct_equal(expect, token.sv));
}

namespace {
// Runs once per process: plug session reset into request dispatch, and the
// cookie drain into every route return. Both hooks stay null without this
// header, so programs that never open a session pay one branch per request.
struct SessionHookInstall {
    SessionHookInstall() {
        hs_request_start_hook = session_request_start;
        hs_respond_drain_hook = session_drain_pending;
    }
};
inline SessionHookInstall session_hook_install;
}

}  // namespace hs

#endif  // HS_RUNTIME_SESSION_HPP
