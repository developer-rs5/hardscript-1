// qa/runtime/session.cpp -- session manager
//
// Cookies are the only client: every test builds a request with a Cookie
// header (or none), calls the language-surface functions directly, and reads
// the Set-Cookie lines off a drained response. No server runs here; the
// dispatch hook and `hs_respond` drain are exercised as the units they are.

#include "hs_runtime_session.hpp"

#include "support.hpp"

namespace qa {
namespace {

struct TestReq {
    std::string cookie_store;
    hs::Request req;
    TestReq() {}
    // `cookie` is the value; browsers send `hs_session=<value>`.
    explicit TestReq(const std::string& cookie) : cookie_store("hs_session=" + cookie) {
        req.add_header("cookie", cookie_store);
    }
};

std::string set_cookie_of(const hs::Response& res) {
    for (const auto& h : res.headers) {
        if (h.first == "Set-Cookie" && h.second.find("hs_session=") == 0) return h.second;
    }
    return "";
}

std::string cookie_value(const std::string& set_cookie) {
    // Between `hs_session=` and the first `;`.
    size_t eq = set_cookie.find('=');
    if (eq == std::string::npos) return "";
    size_t end = set_cookie.find(';', eq + 1);
    return set_cookie.substr(eq + 1, end == std::string::npos ? end : end - eq - 1);
}

void drain_into(hs::Response& res) {
    // What `hs_respond` does for every route return, spelled out so the test
    // names the contract instead of the helper.
    if (hs::hs_respond_drain_hook) hs::hs_respond_drain_hook(res);
}

struct SecretGuard {
    SecretGuard() { hs::session_set_secret_for_tests("test-secret-0123456789abcdef"); }
    ~SecretGuard() { hs::session_clear_secret_for_tests(); }
};

// ---------------------------------------------------------------------------
// Codec: sign, verify, parse
// ---------------------------------------------------------------------------

void signed_payloads_round_trip() {
    SecretGuard g;
    hs::Val payload = hs::Val::object({{"user", hs::Val::object({{"id", hs::Val::int_(1)}})},
                                       {"flash", hs::Val::object({})},
                                       {"sid", hs::Val::text("abc")},
                                       {"iat", hs::Val::int_(7)}});
    std::string signed_ = hs::session_sign(payload);
    CHECK(signed_.compare(0, 3, "v1.") == 0, "versioned up front");
    hs::Val back;
    CHECK(hs::session_verify(signed_, back), "verifies");
    CHECK_EQ(back.find("sid")->sv, std::string("abc"), "payload intact");
    CHECK_EQ(back.find("iat")->iv, int64_t(7), "numbers stay numbers");
}

void tampered_cookies_fail_closed() {
    SecretGuard g;
    hs::Val payload = hs::Val::object({{"sid", hs::Val::text("abc")}});
    std::string good = hs::session_sign(payload);
    hs::Val back;
    std::string bad = good;
    bad[5] = (char)(bad[5] == 'A' ? 'B' : 'A');
    CHECK(!hs::session_verify(bad, back), "flipped body byte refused");
    std::string badsig = good.substr(0, good.rfind('.') + 1) + "AAAA";
    CHECK(!hs::session_verify(badsig, back), "forged signature refused");
    CHECK(!hs::session_verify("", back), "empty refused");
    CHECK(!hs::session_verify("abc", back), "no dots refused");
    CHECK(!hs::session_verify("a.b", back), "two parts refused");
    CHECK(!hs::session_verify("v2.x.y", back), "unknown version refused");
    CHECK(!hs::session_verify("v1.!!!.???", back), "bad base64 refused");
}

void signatures_follow_the_secret() {
    hs::session_set_secret_for_tests("secret-A");
    std::string a = hs::session_sign(hs::Val::object({{"sid", hs::Val::text("x")}}));
    hs::session_set_secret_for_tests("secret-B");
    hs::Val back;
    CHECK(!hs::session_verify(a, back), "rotation invalidates everything outstanding");
    hs::session_clear_secret_for_tests();
}

void cookie_headers_are_complete() {
    std::string h = hs::session_cookie_header("v1.abc.def", 60);
    CHECK(h.find("hs_session=v1.abc.def") == 0, "name and value first");
    CHECK(h.find("Path=/") != std::string::npos, "path");
    CHECK(h.find("HttpOnly") != std::string::npos, "no scripts");
    CHECK(h.find("SameSite=Lax") != std::string::npos, "lax by default");
    CHECK(h.find("Secure") != std::string::npos, "secure always");
    CHECK(h.find("Max-Age=60") != std::string::npos, "max age");
    std::string dead = hs::session_cookie_header("", 0);
    CHECK(dead.find("Max-Age=0") != std::string::npos, "expired on destroy");
}

void cookie_parsing_tolerates_reality() {
    CHECK_EQ(hs::session_cookie_from_header("hs_session=abc", "hs_session"), std::string("abc"),
             "alone");
    CHECK_EQ(hs::session_cookie_from_header("a=1; hs_session=abc; b=2", "hs_session"), std::string("abc"),
             "among others");
    CHECK_EQ(hs::session_cookie_from_header("hs_session=old; hs_session=new", "hs_session"),
             std::string("new"), "last wins like browsers send");
    CHECK_EQ(hs::session_cookie_from_header("hs_session = spaced ; x=1", "hs_session"),
             std::string("spaced"), "spaces around");
    CHECK_EQ(hs::session_cookie_from_header("hs_session=\"quoted\"", "hs_session"), std::string("quoted"),
             "one layer of quotes off");
    CHECK_EQ(hs::session_cookie_from_header("other=1", "hs_session"), std::string(""), "absent is empty");
    CHECK_EQ(hs::session_cookie_from_header("", "hs_session"), std::string(""), "empty header");
}

// ---------------------------------------------------------------------------
// start / user / destroy, through requests and responses
// ---------------------------------------------------------------------------

hs::Response start_session(const hs::Val& user) {
    TestReq req;
    hs::session_request_start();
    CHECK(hs::session_start(req.req, user).truthy(), "start returns true");
    hs::Response res;
    drain_into(res);
    return res;
}

void start_signs_the_user_in() {
    SecretGuard g;
    hs::Val user = hs::Val::object({{"id", hs::Val::int_(1)}, {"name", hs::Val::text("ann")}});
    hs::Response res = start_session(user);
    std::string sc = set_cookie_of(res);
    CHECK(!sc.empty(), "a cookie leaves");
    hs::Val payload;
    CHECK(hs::session_verify(cookie_value(sc), payload), "signed");
    CHECK_EQ(payload.find("user")->find("name")->sv, std::string("ann"), "carrying the user");
    CHECK(!payload.find("sid")->sv.empty(), "with a session id");
    CHECK(payload.find("iat")->iv > 0, "and an issued-at");
}

void user_reads_back_through_a_second_request() {
    SecretGuard g;
    hs::Val user = hs::Val::object({{"id", hs::Val::int_(2)}});
    std::string sc = set_cookie_of(start_session(user));
    TestReq req2(cookie_value(sc));
    hs::session_request_start();
    hs::Val back = hs::session_user(req2.req);
    CHECK_EQ(back.find("id")->iv, int64_t(2), "the same user");
    hs::Response res;
    drain_into(res);
    CHECK(!set_cookie_of(res).empty(), "reading rolls the cookie");
}

void absence_is_nil_not_error() {
    SecretGuard g;
    TestReq req;
    hs::session_request_start();
    CHECK(hs::session_user(req.req).is_nil(), "no cookie, nil user");
    TestReq forged("v1.e30.e30.sig");
    hs::session_request_start();
    CHECK(hs::session_user(forged.req).is_nil(), "forgery is absence too");
}

void destroy_expires_and_forgets() {
    SecretGuard g;
    std::string sc = set_cookie_of(start_session(hs::Val::int_(1)));
    TestReq req2(cookie_value(sc));
    hs::session_request_start();
    CHECK(hs::session_destroy(req2.req).truthy(), "destroy returns true");
    hs::Response res;
    drain_into(res);
    std::string dead = set_cookie_of(res);
    CHECK(dead.find("Max-Age=0") != std::string::npos, "expired cookie leaves");
    TestReq req3(cookie_value(dead));
    hs::session_request_start();
    CHECK(hs::session_user(req3.req).is_nil(), "nothing left to read");
}

void starting_twice_replaces() {
    SecretGuard g;
    TestReq req;
    hs::session_request_start();
    hs::session_start(req.req, hs::Val::int_(1));
    hs::session_start(req.req, hs::Val::int_(2));
    hs::Response res;
    drain_into(res);
    std::string sc = set_cookie_of(res);
    hs::Val payload;
    CHECK(hs::session_verify(cookie_value(sc), payload), "one cookie");
    CHECK_EQ(payload.find("user")->iv, int64_t(2), "the second session wins");
}

void untouched_sessions_emit_nothing() {
    SecretGuard g;
    std::string sc = set_cookie_of(start_session(hs::Val::int_(1)));
    TestReq req2(cookie_value(sc));
    hs::session_request_start();
    // No session call at all: the response carries no cookie.
    hs::Response res;
    drain_into(res);
    CHECK(set_cookie_of(res).empty(), "quiet when untouched");
}

void scalar_and_large_users_round_trip() {
    SecretGuard g;
    hs::Response res = start_session(hs::Val::int_(42));
    hs::Val payload;
    CHECK(hs::session_verify(cookie_value(set_cookie_of(res)), payload), "signed");
    CHECK_EQ(payload.find("user")->iv, int64_t(42), "a scalar user");
    std::string big(5000, 'u');
    res = start_session(hs::Val::text(big));
    CHECK(hs::session_verify(cookie_value(set_cookie_of(res)), payload), "signed");
    CHECK_EQ(payload.find("user")->sv.size(), size_t(5000), "a large user");
}

// ---------------------------------------------------------------------------
// Flash: set now, read once
// ---------------------------------------------------------------------------

void flash_set_reads_once() {
    SecretGuard g;
    TestReq req;
    hs::session_request_start();
    CHECK(hs::session_flash(req.req, hs::Val::text("notice"), hs::Val::text("hi")).truthy(), "set");
    CHECK_EQ(hs::session_flash(req.req, hs::Val::text("notice"), hs::Val::nil()).sv, std::string("hi"),
             "read");
    // Consumption lands at drain, not at read: re-reading in the same request
    // sees the value again, and only the next request finds it gone (proved
    // below). A handler passing flash data around must not lose it mid-way.
    CHECK_EQ(hs::session_flash(req.req, hs::Val::text("notice"), hs::Val::nil()).sv, std::string("hi"),
             "still there until the response goes out");
}

void flash_survives_exactly_one_request() {
    SecretGuard g;
    TestReq req;
    hs::session_request_start();
    (void)hs::session_flash(req.req, hs::Val::text("notice"), hs::Val::text("hi"));
    hs::Response res;
    drain_into(res);
    // Next request: the flash is there...
    TestReq req2(cookie_value(set_cookie_of(res)));
    hs::session_request_start();
    CHECK_EQ(hs::session_flash(req2.req, hs::Val::text("notice"), hs::Val::nil()).sv, std::string("hi"),
             "next request reads it");
    hs::Response res2;
    drain_into(res2);
    // ...and the request after: gone, while anonymous otherwise.
    TestReq req3(cookie_value(set_cookie_of(res2)));
    hs::session_request_start();
    CHECK(hs::session_flash(req3.req, hs::Val::text("notice"), hs::Val::nil()).is_nil(), "then gone");
    CHECK(hs::session_user(req3.req).is_nil(), "flash alone never logged anyone in");
}

void flash_missing_and_bad_keys() {
    SecretGuard g;
    TestReq req;
    hs::session_request_start();
    CHECK(hs::session_flash(req.req, hs::Val::text("nope"), hs::Val::nil()).is_nil(), "missing nil");
    bool threw = false;
    try {
        hs::session_flash(req.req, hs::Val::int_(1), hs::Val::text("x"));
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("key as text") != std::string::npos;
    }
    CHECK(threw, "non-text key refused with a reason");
    threw = false;
    try {
        hs::session_flash(req.req, hs::Val::text(""), hs::Val::text("x"));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "empty key refused");
}

void flash_reset_unconsumes() {
    SecretGuard g;
    TestReq req;
    hs::session_request_start();
    (void)hs::session_flash(req.req, hs::Val::text("k"), hs::Val::text("one"));
    (void)hs::session_flash(req.req, hs::Val::text("k"), hs::Val::nil());
    (void)hs::session_flash(req.req, hs::Val::text("k"), hs::Val::text("two"));
    hs::Response res;
    drain_into(res);
    TestReq req2(cookie_value(set_cookie_of(res)));
    hs::session_request_start();
    CHECK_EQ(hs::session_flash(req2.req, hs::Val::text("k"), hs::Val::nil()).sv, std::string("two"),
             "reset after read lives again");
}

// ---------------------------------------------------------------------------
// CSRF: bound to the session, verified constant-time
// ---------------------------------------------------------------------------

void csrf_round_trips_on_its_session() {
    SecretGuard g;
    std::string sc = set_cookie_of(start_session(hs::Val::int_(1)));
    TestReq req2(cookie_value(sc));
    hs::session_request_start();
    hs::Val token = hs::session_csrf(req2.req);
    CHECK(token.is_str() && !token.sv.empty(), "a token");
    CHECK(hs::session_csrf_valid(req2.req, token).truthy(), "validates on its session");
    CHECK(!hs::session_csrf_valid(req2.req, hs::Val::text(token.sv + "x")).truthy(), "tampered fails");
    CHECK(!hs::session_csrf_valid(req2.req, hs::Val::int_(1)).truthy(), "non-text fails");
}

void csrf_without_a_session() {
    SecretGuard g;
    TestReq req;
    hs::session_request_start();
    bool threw = false;
    try {
        hs::session_csrf(req.req);
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("needs a session") != std::string::npos;
    }
    CHECK(threw, "generation needs a session");
    CHECK(!hs::session_csrf_valid(req.req, hs::Val::text("whatever")).truthy(),
          "validation without one is false, not an error");
}

void csrf_does_not_cross_sessions() {
    SecretGuard g;
    std::string a = set_cookie_of(start_session(hs::Val::int_(1)));
    std::string b = set_cookie_of(start_session(hs::Val::int_(2)));
    TestReq ra(cookie_value(a));
    hs::session_request_start();
    hs::Val token = hs::session_csrf(ra.req);
    TestReq rb(cookie_value(b));
    hs::session_request_start();
    CHECK(!hs::session_csrf_valid(rb.req, token).truthy(), "a token is bound to its session");
}

// ---------------------------------------------------------------------------
// Plumbing: hooks, drain discipline, secrets
// ---------------------------------------------------------------------------

void hooks_are_installed() {
    CHECK(hs::hs_request_start_hook != nullptr, "dispatch hook set");
    CHECK(hs::hs_respond_drain_hook != nullptr, "drain hook set");
}

void request_start_clears_a_thrown_request() {
    SecretGuard g;
    TestReq req;
    hs::session_request_start();
    hs::session_start(req.req, hs::Val::int_(1));
    // The route throws before responding: simulate the dispatch reset.
    hs::session_request_start();
    TestReq bare;
    hs::session_request_start();
    CHECK(hs::session_user(bare.req).is_nil(), "nothing leaked across");
    hs::Response res;
    drain_into(res);
    CHECK(set_cookie_of(res).empty(), "and nothing queued");
}

void drains_are_single_and_final() {
    SecretGuard g;
    TestReq req;
    hs::session_request_start();
    hs::session_start(req.req, hs::Val::int_(1));
    (void)hs::session_flash(req.req, hs::Val::text("k"), hs::Val::text("v"));
    (void)hs::session_flash(req.req, hs::Val::text("k"), hs::Val::nil());
    hs::Response res;
    drain_into(res);
    int cookies = 0;
    for (const auto& h : res.headers) {
        if (h.first == "Set-Cookie") cookies++;
    }
    CHECK_EQ(cookies, 1, "exactly one cookie per response");
    hs::Response res2;
    drain_into(res2);
    CHECK(set_cookie_of(res2).empty(), "a second drain adds nothing");
}

void secrets_default_sane() {
    hs::session_clear_secret_for_tests();
    CHECK(!hs::session_secret().empty(), "always a secret");
    CHECK(hs::session_secret().size() >= 16, "not a short one");
}

void iat_marks_issue_time() {
    SecretGuard g;
    hs::Response res = start_session(hs::Val::int_(1));
    hs::Val payload;
    CHECK(hs::session_verify(cookie_value(set_cookie_of(res)), payload), "signed");
    int64_t iat = payload.find("iat")->iv;
    int64_t now = hs::session_now_ms();
    CHECK(iat <= now && now - iat < 60 * 1000LL, "issued just now");
}

// ---------------------------------------------------------------------------
// Throughput smoke: session work is crypto plus copies. Prints its rate;
// fails only far below a floor no healthy build misses.
// ---------------------------------------------------------------------------

void session_ops_hit_the_floor() {
    SecretGuard g;
    hs::Val user = hs::Val::object({{"id", hs::Val::int_(1)}});
    TestReq req;
    const int N = 20000;
    auto t0 = std::chrono::steady_clock::now();
    for (int i = 0; i < N; i++) {
        hs::session_request_start();
        hs::session_start(req.req, user);
        hs::Response res;
        drain_into(res);
        (void)set_cookie_of(res);
    }
    auto dt = std::chrono::duration<double>(std::chrono::steady_clock::now() - t0).count();
    double per_sec = N / dt;
    printf("  [info] %.0f session start+drain ops/sec\n", per_sec);
    CHECK(per_sec > 1000.0, "above the 1k/s floor");
}

}  // namespace
}  // namespace qa

int main() {
    qa::signed_payloads_round_trip();
    qa::tampered_cookies_fail_closed();
    qa::signatures_follow_the_secret();
    qa::cookie_headers_are_complete();
    qa::cookie_parsing_tolerates_reality();

    qa::start_signs_the_user_in();
    qa::user_reads_back_through_a_second_request();
    qa::absence_is_nil_not_error();
    qa::destroy_expires_and_forgets();
    qa::starting_twice_replaces();
    qa::untouched_sessions_emit_nothing();
    qa::scalar_and_large_users_round_trip();

    qa::flash_set_reads_once();
    qa::flash_survives_exactly_one_request();
    qa::flash_missing_and_bad_keys();
    qa::flash_reset_unconsumes();

    qa::csrf_round_trips_on_its_session();
    qa::csrf_without_a_session();
    qa::csrf_does_not_cross_sessions();

    qa::hooks_are_installed();
    qa::request_start_clears_a_thrown_request();
    qa::drains_are_single_and_final();
    qa::secrets_default_sane();
    qa::iat_marks_issue_time();

    qa::session_ops_hit_the_floor();

    return qa::report("session");
}
