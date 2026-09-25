#pragma once
// hs_runtime_auth.hpp - JWT authentication and the route guard (M5.2).
//
// Responsibilities:
//
//   * issue a token   auth_issue(claims, secret, ttl, issuer)
//   * read a token    auth_bearer(req)   -> the raw token, without checking it
//   * check a token   auth_check(t, s)   -> claims + a reason string
//   * guard a route   auth_guard(req, secret)  -> throws 401, publishes claims
//   * the caller      auth_user()        -> claims of the request in flight
//   * passwords       auth_hash_password / auth_verify_password (PBKDF2-HMAC-SHA256)
//
// Everything here builds on hs_runtime_crypto.hpp (HMAC-SHA256, base64url) and
// hs_runtime_http.hpp (Request, HttpAbort). The guard is deliberately the only
// place that decides "is this request allowed": the compiler emits one call per
// protected route, so an exempt path is a codegen-time decision and not a
// runtime prefix guess.

#include "hs_runtime_crypto.hpp"
#include "hs_runtime_http.hpp"

namespace hs {

// ---------------------------------------------------------------------------
// Claims
// ---------------------------------------------------------------------------

/// How a token is allowed to be presented, in the order they are tried.
enum class CredSource { None, Bearer, Header, Query, Cookie };

struct AuthOutcome {
    bool ok = false;
    std::string reason;      // empty when ok
    Val claims = Val::nil(); // decoded payload, nil when !ok
    /// The token exactly as it arrived, before any verification. Empty when the
    /// request carried none. Kept separate from `claims` so "the raw string we
    /// were handed" and "the payload we decoded" are never confused.
    std::string token;
    CredSource source = CredSource::None;
};

/// Split a compact JWS into its three dot-separated segments. `alg: none`
/// style two-segment tokens collapse to an empty signature segment, which
/// `auth_check` then rejects.
inline bool jwt_split(const std::string& token,
                      std::string& head, std::string& body, std::string& sig) {
    size_t a = token.find('.');
    if (a == std::string::npos) return false;
    size_t b = token.find('.', a + 1);
    if (b == std::string::npos) return false;
    // A fourth segment means this is a JWE, which we do not implement.
    if (token.find('.', b + 1) != std::string::npos) return false;
    head = token.substr(0, a);
    body = token.substr(a + 1, b - a - 1);
    sig = token.substr(b + 1);
    return true;
}

/// Full verification: shape, algorithm, signature, `nbf`, `exp`. Every failure
/// is reported by name so a 401 can say why without leaking which half failed.
inline AuthOutcome auth_check(const std::string& token, const std::string& secret,
                              double skew_secs = 0.0) {
    AuthOutcome out;
    if (token.empty()) { out.reason = "missing token"; return out; }

    std::string h, b, s;
    if (!jwt_split(token, h, b, s)) { out.reason = "malformed token"; return out; }
    if (h.empty() || b.empty()) { out.reason = "malformed token"; return out; }
    if (s.empty()) { out.reason = "unsigned token"; return out; }
    if (secret.empty()) { out.reason = "no signing secret configured"; return out; }

    // Pin alg=HS256 before spending a HMAC, and never let the header claim
    // anything other than the one algorithm we implement.
    Val head;
    try {
        head = parse_json(base64_decode(h));
    } catch (...) { out.reason = "malformed token"; return out; }
    const Val* alg = head.find("alg");
    if (!alg || !alg->is_str()) { out.reason = "missing alg"; return out; }
    if (alg->sv != "HS256") { out.reason = "unsupported alg"; return out; }

    if (!ct_equal(base64_encode_url(hmac_sha256_bin(secret, h + "." + b)), s)) {
        out.reason = "bad signature";
        return out;
    }

    try {
        out.claims = parse_json(base64_decode(b));
    } catch (...) { out.reason = "malformed token"; return out; }
    if (!out.claims.is_obj()) { out.reason = "malformed token"; return out; }

    // `skew_secs` is leeway for clock drift between the signer and this server.
    // It widens the accepted window on both edges: a token stays good for
    // `skew_secs` past its `exp`, and becomes usable `skew_secs` before `nbf`.
    const double now = (double)(unix_ms() / 1000);
    const Val* exp = out.claims.find("exp");
    if (exp) {
        if (!exp->is_num()) { out.reason = "malformed exp"; return out; }
        if ((double)exp->num() + skew_secs < now) { out.reason = "token expired"; return out; }
    }
    const Val* nbf = out.claims.find("nbf");
    if (nbf) {
        if (!nbf->is_num()) { out.reason = "malformed nbf"; return out; }
        if ((double)nbf->num() - skew_secs > now) { out.reason = "token not yet valid"; return out; }
    }
    out.ok = true;
    return out;
}

/// Issue a token. `ttl_secs <= 0` means "no expiry", which is allowed for
/// tests but a server should always pass a real lifetime. `claims` is not
/// mutated: `iat`, `exp`, `iss` and `jti` are added to the copy that is
/// actually signed.
inline std::string auth_issue(const Val& claims, const std::string& secret,
                              double ttl_secs = 3600.0, const std::string& issuer = "") {
    Val c = claims.is_obj() ? claims : Val::object({});
    const double now = (double)(unix_ms() / 1000);
    c.set("iat", Val::int_((int64_t)now));
    if (ttl_secs > 0) c.set("exp", Val::int_((int64_t)(now + ttl_secs)));
    if (!issuer.empty()) c.set("iss", Val::text(issuer));
    if (!c.find("jti")) c.set("jti", Val::text(random_urlsafe(12)));
    return jwt_sign(c, secret);
}

// ---------------------------------------------------------------------------
// Credentials off the wire
// ---------------------------------------------------------------------------

inline std::string trim_ws(const std::string& s) {
    size_t a = 0, b = s.size();
    while (a < b && (s[a] == ' ' || s[a] == '\t')) a++;
    while (b > a && (s[b - 1] == ' ' || s[b - 1] == '\t')) b--;
    return s.substr(a, b - a);
}

inline std::vector<std::string> split_str(const std::string& s, char sep) {
    std::vector<std::string> out;
    std::string cur;
    for (char c : s) {
        if (c == sep) { out.push_back(cur); cur.clear(); }
        else cur += c;
    }
    out.push_back(cur);
    return out;
}

/// The `Authorization` header, `?token=`, and `access_token_cookie` in that
/// order. The scheme name is matched case-insensitively because RFC 7235 says
/// it is, and real clients send `bearer` as often as `Bearer`.
inline AuthOutcome auth_from_request(const Request& req) {
    AuthOutcome out;
    std::string auth = trim_ws(req.header("authorization"));
    if (!auth.empty()) {
        out.source = CredSource::Header;
        size_t sp = auth.find(' ');
        if (sp == std::string::npos) {
            out.reason = "malformed authorization header";
            return out;
        }
        std::string scheme = auth.substr(0, sp);
        std::string rest = trim_ws(auth.substr(sp + 1));
        for (char& c : scheme) c = (char)tolower((unsigned char)c);
        if (scheme == "bearer") {
            if (rest.empty()) { out.reason = "empty bearer token"; return out; }
            out.token = rest; // the caller checks it against the secret
            return out;
        }
        out.reason = "unsupported authorization scheme";
        return out;
    }
    std::string q = req.q("token");
    if (q.empty()) q = req.q("access_token");
    if (!q.empty()) { out.source = CredSource::Query; out.token = q; return out; }
    std::string cookie = trim_ws(req.header("cookie"));
    if (!cookie.empty()) {
        for (const std::string& part : split_str(cookie, ';')) {
            std::string kv = trim_ws(part);
            size_t eq = kv.find('=');
            if (eq == std::string::npos) continue;
            if (trim_ws(kv.substr(0, eq)) == "access_token") {
                out.source = CredSource::Cookie;
                out.token = trim_ws(kv.substr(eq + 1));
                return out;
            }
        }
    }
    out.reason = "no credentials";
    return out;
}

/// The raw token off the request, or "" when there is none. Unlike
/// `auth_verify_request` this does not check the signature, so it is only
/// useful for handing a token to another service.
inline std::string auth_bearer(const Request& req) {
    return auth_from_request(req).token;
}

/// Decode a token's payload WITHOUT checking the signature. Exposed as
/// `auth.claims` for reading `jti` off a token being discarded and for tests.
/// An authorization decision must use `auth_check` instead.
inline Val jwt_claims_unchecked(const std::string& token) {
    std::string h, b, s;
    if (!jwt_split(token, h, b, s)) return Val::nil();
    try {
        return parse_json(base64_decode(b));
    } catch (...) {
        return Val::nil();
    }
}

/// Verify whatever the request presented.
inline AuthOutcome auth_verify_request(const Request& req, const std::string& secret) {
    AuthOutcome got = auth_from_request(req);
    if (got.token.empty()) {
        // Nothing usable was presented. When the Authorization header was there
        // but unusable, say why rather than pretending no credentials arrived;
        // this keys off the presence of the header, not off the wording above.
        if (got.source == CredSource::None) got.reason = "no credentials";
        return got;
    }
    AuthOutcome out = auth_check(got.token, secret);
    out.source = got.source;
    return out;
}

// ---------------------------------------------------------------------------
// The current request's identity
// ---------------------------------------------------------------------------
//
// One `Val` per worker thread, published by the guard and read by handlers.
// Handler code runs on a pooled thread, so this has to be thread-local rather
// than a field on the request: the same handler is re-entered concurrently.

inline Val& auth_claims_slot() { static thread_local Val v = Val::nil(); return v; }
inline std::string& auth_secret_slot() { static thread_local std::string s; return s; }

inline Val auth_user() { return auth_claims_slot(); }
inline std::string auth_current_secret() { return auth_secret_slot(); }

/// The 401 body. `WWW-Authenticate` is included because RFC 7235 requires a
/// challenge on an unauthenticated response.
inline Response auth_unauthorized(const std::string& reason) {
    Val v = Val::object({});
    v.set("error", Val::text("unauthorized"));
    v.set("status", Val::int_(401));
    v.set("message", Val::text(reason));
    Response r;
    r.status = 401;
    r.ctype = "application/json; charset=utf-8";
    r.json_v = std::move(v);
    r.has_json = true;
    r.headers.emplace_back("WWW-Authenticate", "Bearer realm=\"hardscript\"");
    return r;
}

inline Response auth_forbidden(const std::string& reason) {
    Val v = Val::object({});
    v.set("error", Val::text("forbidden"));
    v.set("status", Val::int_(403));
    v.set("message", Val::text(reason));
    Response r;
    r.status = 403;
    r.ctype = "application/json; charset=utf-8";
    r.json_v = std::move(v);
    r.has_json = true;
    return r;
}

/// Guard emitted at the top of every protected route. Throws 401 when the
/// request has no valid token, otherwise publishes the claims and returns.
inline void auth_guard(const Request& req, const std::string& secret) {
    AuthOutcome o = auth_verify_request(req, secret);
    if (!o.ok) {
        // Clear before throwing: the slot is per-thread and the server reuses
        // threads, so a rejected request must not leave the previous request's
        // identity visible to whatever runs next.
        auth_claims_slot() = Val::nil();
        auth_secret_slot().clear();
        throw HttpAbort(auth_unauthorized(o.reason));
    }
    auth_claims_slot() = o.claims;
    auth_secret_slot() = secret;
}

/// `auth.require(...)` in a handler: same check, but usable for a route the
/// compiler did not mark, and it can name a required claim.
inline void auth_require(const Request& req, const std::string& secret,
                         const std::string& claim = "") {
    AuthOutcome o = auth_verify_request(req, secret);
    if (!o.ok) {
        auth_claims_slot() = Val::nil();
        auth_secret_slot().clear();
        throw HttpAbort(auth_unauthorized(o.reason));
    }
    if (!claim.empty() && !o.claims.find(claim)) {
        throw HttpAbort(auth_forbidden("token is missing the `" + claim + "` claim"));
    }
    auth_claims_slot() = o.claims;
    auth_secret_slot() = secret;
}

/// True when the request already carries a valid token. Never throws, so it
/// can be used for optional auth where a route serves both audiences.
inline bool auth_optional(const Request& req, const std::string& secret) {
    AuthOutcome o = auth_verify_request(req, secret);
    if (!o.ok) { auth_claims_slot() = Val::nil(); return false; }
    auth_claims_slot() = o.claims;
    auth_secret_slot() = secret;
    return true;
}

inline void auth_reject(int status, const std::string& message) {
    if (status == 403) throw HttpAbort(auth_forbidden(message));
    throw HttpAbort(auth_unauthorized(message));
}

// ---------------------------------------------------------------------------
// Passwords
// ---------------------------------------------------------------------------
//
// PBKDF2-HMAC-SHA256, 120k iterations, 16-byte salt. Stored as
// `pbkdf2-sha256$<iterations>$<salt-hex>$<key-hex>` so the work factor travels
// with the hash and can be raised without invalidating old rows.

inline std::string pbkdf2_sha256(const std::string& password, const std::string& salt,
                                 int iterations, size_t dk_len) {
    std::string out;
    out.reserve(dk_len);
    const int h_len = 32; // SHA-256 output
    uint32_t blocks = (uint32_t)((dk_len + h_len - 1) / h_len);
    for (uint32_t b = 1; b <= blocks; b++) {
        std::string msg = salt;
        msg.push_back((char)((b >> 24) & 0xff));
        msg.push_back((char)((b >> 16) & 0xff));
        msg.push_back((char)((b >> 8) & 0xff));
        msg.push_back((char)(b & 0xff));
        std::string u = hmac_sha256_bin(password, msg);
        std::string t = u;
        for (int i = 1; i < iterations; i++) {
            u = hmac_sha256_bin(password, u);
            for (size_t j = 0; j < t.size(); j++) t[j] = (char)(t[j] ^ u[j]);
        }
        out += t;
    }
    out.resize(dk_len);
    return out;
}

inline std::string hex_encode(const std::string& raw) {
    static const char* d = "0123456789abcdef";
    std::string out;
    out.reserve(raw.size() * 2);
    for (unsigned char c : raw) {
        out.push_back(d[c >> 4]);
        out.push_back(d[c & 0xf]);
    }
    return out;
}

inline int hex_value(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

inline std::string hex_decode(const std::string& hex) {
    std::string out;
    out.reserve(hex.size() / 2);
    for (size_t i = 0; i + 1 < hex.size(); i += 2) {
        int hi = hex_value(hex[i]), lo = hex_value(hex[i + 1]);
        if (hi < 0 || lo < 0) return std::string();
        out.push_back((char)((hi << 4) | lo));
    }
    return out;
}

inline const int kPbkdf2Iterations = 120000;
inline const size_t kPbkdf2SaltBytes = 16;
inline const size_t kPbkdf2KeyBytes = 32;

/// Stored form: `pbkdf2-sha256$<iterations>$<salt-hex>$<dk-hex>`. The salt is
/// stored hex-encoded for readability but PBKDF2 is fed the *decoded* bytes, so
/// hashing and verifying derive the key from identical input.
inline std::string auth_hash_password(const std::string& password) {
    std::string salt_hex = random_hex(kPbkdf2SaltBytes);
    std::string salt = hex_decode(salt_hex);
    if (salt.size() != kPbkdf2SaltBytes) return std::string();
    std::string dk = pbkdf2_sha256(password, salt, kPbkdf2Iterations, kPbkdf2KeyBytes);
    return std::string("pbkdf2-sha256$") + std::to_string(kPbkdf2Iterations) + "$" +
           salt_hex + "$" + hex_encode(dk);
}

/// Constant-time password check. An unparseable stored hash is a failure, not
/// an exception: a corrupt row must not turn into a 500.
inline bool auth_verify_password(const std::string& password, const std::string& stored) {
    std::vector<std::string> parts;
    std::string cur;
    for (char c : stored) {
        if (c == '$') { parts.push_back(cur); cur.clear(); }
        else cur += c;
    }
    parts.push_back(cur);
    if (parts.size() != 4 || parts[0] != "pbkdf2-sha256") return false;
    int iterations = atoi(parts[1].c_str());
    if (iterations <= 0 || iterations > 10000000) return false;
    std::string salt = hex_decode(parts[2]);
    std::string want = hex_decode(parts[3]);
    // The stored key length drives the PBKDF2 work factor, so it is pinned
    // rather than trusted: a row claiming a huge key would otherwise turn one
    // login into a denial of service.
    if (salt.size() != kPbkdf2SaltBytes || want.size() != kPbkdf2KeyBytes) return false;
    std::string got = pbkdf2_sha256(password, salt, iterations, kPbkdf2KeyBytes);
    return ct_equal(got, want);
}

} // namespace hs
