// m5.2: the auth engine. Covers the JWT claim/verify round trip, every
// fail-closed path an attacker would try (expired, not-yet-valid, unsigned,
// wrong algorithm, wrong secret, tampered payload), credential discovery across
// header/cookie/query, the constant-time compare, and PBKDF2 passwords.
#include <hs_runtime.hpp>
#include <cstdio>
#include <string>

static int fails = 0;
#define CHECK(cond, msg)                                \
    do {                                                \
        if (!(cond)) {                                  \
            std::fprintf(stderr, "FAIL %s\n", msg);     \
            fails++;                                    \
        }                                               \
    } while (0)

static const char* SECRET = "reg-secret";

static std::string claim(const std::string& json) {
    return hs::auth_issue(hs::parse_json(json), SECRET, 3600.0, "hardscript");
}

// `Request` stores string_views into caller-owned storage (it normally points
// into the connection buffer), so the backing text has to outlive the request.
// `Sink` is that storage; keep it alive for the whole block that uses `r`.
struct Sink {
    std::vector<std::string> keep;
    hs::Request make(const std::string& authz) {
        keep.push_back(authz);
        hs::Request r;
        r.method = "GET";
        r.path = "/me";
        if (!authz.empty()) r.add_header("authorization", keep.back());
        return r;
    }
};
static hs::Request req_with(const char* authz) {
    static Sink sink;
    return sink.make(authz ? authz : "");
}

int main() {
    // -- round trip --------------------------------------------------------
    std::string tok = claim("{\"sub\":\"u1\",\"role\":\"admin\"}");
    hs::AuthOutcome ok = hs::auth_check(tok, SECRET);
    CHECK(ok.ok, "fresh token verifies");
    CHECK(ok.claims.get("sub").sv == "u1", "claim survives the round trip");
    CHECK(ok.claims.get("role").sv == "admin", "second claim survives");
    CHECK(!ok.claims.get("jti").sv.empty(), "issue stamps a jti");
    CHECK(ok.claims.get("iss").sv == "hardscript", "issue stamps iss");
    CHECK(ok.claims.get("exp").num() > hs::unix_ms() / 1000, "exp is in the future");
    CHECK(ok.reason.empty(), "a good token has no reason string");

    // A second mint of the same claims differs: jti makes tokens unique, so a
    // revoked or replayed token can be told apart.
    CHECK(claim("{\"sub\":\"u1\"}") != claim("{\"sub\":\"u1\"}"), "jti is random");

    // -- fail closed -------------------------------------------------------
    CHECK(!hs::auth_check(tok, "wrong-secret").ok, "wrong secret is rejected");
    CHECK(!hs::auth_check(tok + "x", SECRET).ok, "tampered signature is rejected");
    CHECK(!hs::auth_check(tok.substr(0, tok.size() - 4) + "AAAA", SECRET).ok,
          "truncated/forged signature is rejected");
    CHECK(!hs::auth_check("", SECRET).ok, "empty token is rejected");
    CHECK(!hs::auth_check("not.a.jwt", SECRET).ok, "garbage token is rejected");
    CHECK(!hs::auth_check("only-one-part", SECRET).ok, "one-part token is rejected");

    // alg=none: the classic JWT bypass. The header is parsed, so the token is
    // otherwise well formed.
    std::string none = hs::base64_encode_url("{\"alg\":\"none\",\"typ\":\"JWT\"}") + "." +
                       hs::base64_encode_url("{\"sub\":\"admin\",\"role\":\"root\"}") + ".";
    CHECK(!hs::auth_check(none, SECRET).ok, "alg=none is rejected");
    CHECK(hs::auth_check(none, SECRET).reason == "unsigned token", "alg=none reason");

    // An attacker re-signs with a different algorithm the library supports.
    std::string hs512 = hs::base64_encode_url("{\"alg\":\"HS512\",\"typ\":\"JWT\"}") + "." +
                        hs::base64_encode_url("{\"sub\":\"admin\"}") + "." +
                        hs::base64_encode_url("garbage");
    CHECK(!hs::auth_check(hs512, SECRET).ok, "HS512 header is rejected");
    CHECK(hs::auth_check(hs512, SECRET).reason == "unsupported alg",
          "HS512 reason");

    // Swap the payload, keep the old signature.
    hs::AuthOutcome swapped = hs::auth_check(
        tok.substr(0, tok.find('.') + 1) +
            hs::base64_encode_url("{\"sub\":\"root\",\"role\":\"root\"}") + "." +
            tok.substr(tok.rfind('.') + 1),
        SECRET);
    CHECK(!swapped.ok, "swapped payload is rejected");
    CHECK(swapped.reason == "bad signature", "swapped payload reason");

    // exp/nbf. These claims are chosen by whoever holds the signing key, so the
    // verifier -- not the minter -- has to police them. They are signed with
    // jwt_sign because auth_issue stamps its own exp.
    auto signed_claims = [&](const std::string& json) {
        return hs::jwt_sign(hs::parse_json(json), SECRET);
    };
    CHECK(!hs::auth_check(signed_claims("{\"exp\":1000}"), SECRET).ok, "expired token is rejected");
    CHECK(hs::auth_check(signed_claims("{\"exp\":1000}"), SECRET).reason == "token expired",
          "expired reason");
    CHECK(!hs::auth_check(signed_claims("{\"exp\":\"soon\"}"), SECRET).ok, "non-numeric exp is rejected");
    CHECK(!hs::auth_check(signed_claims("{\"nbf\":4102444800}"), SECRET).ok, "not-yet-valid token is rejected");
    CHECK(hs::auth_check(signed_claims("{\"nbf\":4102444800}"), SECRET).reason == "token not yet valid",
          "nbf reason");
    CHECK(hs::auth_check(signed_claims("{\"exp\":4102444800}"), SECRET).ok, "far-future exp is fine");

    // Clock skew is leeway in both directions: a token that expired 5s ago is
    // still accepted with 30s of skew, and one that becomes valid in 5s is
    // usable early, but neither is accepted without the skew.
    long long now_s = (long long)(hs::unix_ms() / 1000);
    std::string just_expired = signed_claims("{\"exp\":" + std::to_string(now_s - 5) + "}");
    CHECK(!hs::auth_check(just_expired, SECRET).ok, "5s past exp is rejected without skew");
    CHECK(hs::auth_check(just_expired, SECRET, 30.0).ok, "5s past exp is fine within skew");
    std::string not_yet = signed_claims("{\"nbf\":" + std::to_string(now_s + 5) + "}");
    CHECK(!hs::auth_check(not_yet, SECRET).ok, "5s before nbf is rejected without skew");
    CHECK(hs::auth_check(not_yet, SECRET, 30.0).ok, "5s before nbf is fine within skew");
    CHECK(!hs::auth_check(not_yet, SECRET, 1.0).ok, "skew smaller than the gap still rejects");

    // -- constant-time compare --------------------------------------------
    CHECK(hs::ct_equal("abc", "abc"), "ct_equal same");
    CHECK(!hs::ct_equal("abc", "abd"), "ct_equal differs at the end");
    CHECK(!hs::ct_equal("abc", "ab"), "ct_equal length differs");
    CHECK(hs::ct_equal("", ""), "ct_equal empty");

    // -- credential discovery ---------------------------------------------
    CHECK(hs::auth_bearer(req_with("Bearer abc")) == "abc", "bearer header");
    CHECK(hs::auth_bearer(req_with("bearer abc")) == "abc", "scheme is case-insensitive");
    CHECK(hs::auth_bearer(req_with("BEARER   abc  ")) == "abc", "extra whitespace is trimmed");
    CHECK(hs::auth_bearer(req_with("Bearer")) == "", "scheme with no token");
    CHECK(hs::auth_bearer(req_with("Basic abc")) == "", "wrong scheme");
    CHECK(hs::auth_bearer(req_with(nullptr)) == "", "no header at all");

    // Cookie then query fallback.
    hs::Request cookie;
    cookie.add_header("cookie", "a=1; access_token=from-cookie");
    CHECK(hs::auth_bearer(cookie) == "from-cookie", "cookie fallback");
    hs::Request query;
    query.query = "x=1&token=from-query";
    CHECK(hs::auth_bearer(query) == "from-query", "query fallback");

    // -- guard -------------------------------------------------------------
    {
        Sink sink;
        hs::Request r = sink.make("Bearer " + tok);
        hs::auth_guard(r, SECRET);
        CHECK(hs::auth_user().get("sub").sv == "u1", "guard publishes the claims");
    }
    {
        // The guard must not leak the previous request's identity into a
        // request that carries no credentials at all.
        hs::Request r = req_with(nullptr);
        try {
            hs::auth_guard(r, SECRET);
            CHECK(false, "guard throws without credentials");
        } catch (const hs::HttpAbort& a) {
            CHECK(a.r.status == 401, "guard returns 401");
            CHECK(a.r.has_json, "401 body is json");
            CHECK(hs::to_json(a.r.json_v).find("\"no credentials\"") != std::string::npos,
                  "401 says why");
        }
    }
    {
        hs::Request r = req_with("Bearer nope");
        try {
            hs::auth_guard(r, SECRET);
            CHECK(false, "guard throws on a bad token");
        } catch (const hs::HttpAbort& a) {
            CHECK(a.r.status == 401, "bad token returns 401");
            // Header names are case-insensitive on the wire, so the check has
            // to be too.
            bool challenge = false;
            for (const auto& h : a.r.headers)
                if (hs::fold_eq(h.first, "www-authenticate")) challenge = true;
            CHECK(challenge, "401 carries a challenge");
        }
    }

    // A rejected request must clear the slot, or the next request on this
    // thread would see the previous caller's identity.
    CHECK(hs::auth_user().is_nil(), "claims do not leak across a rejected request");

    // auth.require: a second gate for role checks.
    {
        Sink sink;
        hs::Request r = sink.make("Bearer " + tok);
        hs::auth_guard(r, SECRET);
        hs::auth_require(r, SECRET, "role");
    }
    {
        Sink sink;
        hs::Request r = sink.make("Bearer " + claim("{\"sub\":\"u1\"}"));
        try {
            hs::auth_require(r, SECRET, "role");
            CHECK(false, "require throws without the claim");
        } catch (const hs::HttpAbort& a) {
            CHECK(a.r.status == 403, "a missing claim is 403, not 401");
        }
    }

    // -- unchecked decode --------------------------------------------------
    CHECK(hs::jwt_claims_unchecked(tok).get("sub").sv == "u1", "unchecked decode reads claims");
    CHECK(hs::jwt_claims_unchecked("garbage").is_nil(), "unchecked decode fails soft");

    // -- passwords ---------------------------------------------------------
    std::string h = hs::auth_hash_password("correct horse");
    CHECK(!h.empty(), "password hashes");
    CHECK(hs::auth_verify_password("correct horse", h), "password verifies");
    CHECK(!hs::auth_verify_password("wrong horse", h), "wrong password fails");
    CHECK(h != hs::auth_hash_password("correct horse"), "salted per call");
    // A hash must not leak the password it was made from.
    CHECK(h.find("correct horse") == std::string::npos, "hash hides the password");

    if (fails) {
        std::fprintf(stderr, "%d check(s) failed\n", fails);
        return 1;
    }
    std::printf("ok\n");
    return 0;
}
