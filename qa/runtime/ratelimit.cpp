// qa/runtime/ratelimit.cpp -- rate limiter
//
// A limiter is a promise about the future: the first N requests in a window
// pass, the rest wait. Time is faked so windows are deterministic; threads
// prove the shards share safely; the abort path proves a denied request
// becomes a 429 with a Retry-After instead of handler code.

#include "hs_runtime_ratelimit.hpp"

#include "support.hpp"

#include <thread>

namespace qa {
namespace {

struct ClockGuard {
    explicit ClockGuard(int64_t ms = 1'000'000) { hs::limit_set_now_for_tests(ms); }
    ~ClockGuard() { hs::limit_clear_now_for_tests(); }
    static void at(int64_t ms) { hs::limit_set_now_for_tests(ms); }
};

// A limiter nobody else touches: registry tests share names globally, but
// these objects are private to each test.
hs::RateLimiter token_bucket(int64_t limit = 5, int64_t window_ms = 60 * 1000LL) {
    return hs::RateLimiter(hs::LIMIT_TOKEN_BUCKET, limit, window_ms);
}

hs::RateLimiter sliding_window(int64_t limit = 5, int64_t window_ms = 60 * 1000LL) {
    return hs::RateLimiter(hs::LIMIT_SLIDING_WINDOW, limit, window_ms);
}

// ---------------------------------------------------------------------------
// Token bucket
// ---------------------------------------------------------------------------

void first_requests_pass_to_capacity() {
    ClockGuard clock;
    hs::RateLimiter lim = token_bucket(3, 60 * 1000LL);
    for (int i = 0; i < 3; i++) {
        hs::LimitVerdict v = lim.check("k", 1'000'000);
        CHECK(v.allowed, "budget spends");
        CHECK_EQ(v.retry_after_ms, int64_t(0), "allowed carries no wait");
    }
    hs::LimitVerdict v = lim.check("k", 1'000'000);
    CHECK(!v.allowed, "empty bucket denies");
    CHECK(v.retry_after_ms > 0, "naming the wait");
}

void tokens_refill_with_time() {
    ClockGuard clock;
    hs::RateLimiter lim = token_bucket(2, 10 * 1000LL);  // 2 per 10s
    CHECK(lim.check("k", 1'000'000).allowed, "one");
    CHECK(lim.check("k", 1'000'000).allowed, "two");
    CHECK(!lim.check("k", 1'000'000).allowed, "empty");
    // Five seconds refills one of two.
    CHECK(lim.check("k", 1'005'000).allowed, "refilled one");
    CHECK(!lim.check("k", 1'005'000).allowed, "but only one");
    // Ten idle seconds refill to capacity, never past it.
    CHECK(lim.check("k", 1'020'000).allowed, "full again");
    CHECK(lim.check("k", 1'020'000).allowed, "twice");
    CHECK(!lim.check("k", 1'020'000).allowed, "capped at capacity");
}

void retry_after_counts_down() {
    ClockGuard clock;
    hs::RateLimiter lim = token_bucket(1, 10 * 1000LL);
    CHECK(lim.check("k", 1'000'000).allowed, "spent");
    hs::LimitVerdict v = lim.check("k", 1'000'000);
    CHECK(!v.allowed, "denied");
    CHECK(v.retry_after_ms >= 9000 && v.retry_after_ms <= 10000, "about ten seconds");
    hs::LimitVerdict w = lim.check("k", 1'005'000);
    CHECK(!w.allowed, "still denied halfway");
    CHECK(w.retry_after_ms < v.retry_after_ms, "but less to wait");
}

void keys_do_not_share_buckets() {
    ClockGuard clock;
    hs::RateLimiter lim = token_bucket(1, 60 * 1000LL);
    CHECK(lim.check("a", 1'000'000).allowed, "a spends");
    CHECK(lim.check("b", 1'000'000).allowed, "b is separate");
    CHECK(!lim.check("a", 1'000'000).allowed, "a stays empty");
}

// ---------------------------------------------------------------------------
// Sliding window
// ---------------------------------------------------------------------------

void window_allows_then_denies() {
    ClockGuard clock;
    hs::RateLimiter lim = sliding_window(3, 60 * 1000LL);
    for (int i = 0; i < 3; i++) {
        CHECK(lim.check("k", 1'000'000 + i).allowed, "within budget");
    }
    hs::LimitVerdict v = lim.check("k", 1'000'000 + 3);
    CHECK(!v.allowed, "the fourth waits");
    CHECK(v.retry_after_ms > 0, "with a wait");
}

void old_hits_age_out() {
    ClockGuard clock;
    hs::RateLimiter lim = sliding_window(2, 10 * 1000LL);
    CHECK(lim.check("k", 1'000'000).allowed, "one");
    CHECK(lim.check("k", 1'001'000).allowed, "two");
    CHECK(!lim.check("k", 1'002'000).allowed, "full");
    // The first hit ages out at +10s.
    CHECK(lim.check("k", 1'010'001).allowed, "room again");
    CHECK(!lim.check("k", 1'010'001).allowed, "one room only");
}

void retry_after_is_oldest_plus_window() {
    ClockGuard clock;
    hs::RateLimiter lim = sliding_window(1, 10 * 1000LL);
    CHECK(lim.check("k", 1'000'000).allowed, "spent");
    hs::LimitVerdict v = lim.check("k", 1'004'000);
    CHECK(!v.allowed, "denied");
    CHECK_EQ(v.retry_after_ms, int64_t(6000), "oldest (t=0) + window - now");
}

// ---------------------------------------------------------------------------
// Validation and keys
// ---------------------------------------------------------------------------

void bad_specs_throw_before_state() {
    ClockGuard clock;
    bool threw = false;
    try {
        hs::limit_check("bad", hs::LIMIT_TOKEN_BUCKET, 0, 60000, "k");
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("budget") != std::string::npos;
    }
    CHECK(threw, "zero budget refused with a reason");
    threw = false;
    try {
        hs::limit_check("bad", hs::LIMIT_TOKEN_BUCKET, 5, 0, "k");
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "zero window refused");
    threw = false;
    try {
        hs::limit_check("bad", 99, 5, 60000, "k");
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "unknown algorithm refused");
}

void key_fallbacks() {
    CHECK_EQ(hs::limit_key_or_ip(hs::Val::text("u1"), "1.2.3.4"), std::string("u1"), "explicit key wins");
    CHECK_EQ(hs::limit_key_or_ip(hs::Val::nil(), "1.2.3.4"), std::string("1.2.3.4"), "nil falls back to IP");
    CHECK_EQ(hs::limit_key_or_ip(hs::Val::text(""), "1.2.3.4"), std::string("1.2.3.4"), "empty too");
    CHECK_EQ(hs::limit_key_or_ip(hs::Val::nil(), ""), std::string("local"), "nothing shares one bucket");
}

void idle_buckets_do_not_accumulate() {
    ClockGuard clock;
    hs::RateLimiter lim = token_bucket(5, 60 * 1000LL);
    // First wave spends a token everywhere. Time frozen means nothing refills
    // to idle, so the second wave (after two windows pass) finds shards over
    // the cap and sheds the first wave, idle a full window by then.
    for (int i = 0; i < 150000; i++) {
        lim.check("idle-a-" + std::to_string(i), 1'000'000);
    }
    ClockGuard::at(1'000'000 + 180 * 1000LL);
    for (int i = 0; i < 150000; i++) {
        lim.check("idle-b-" + std::to_string(i), 1'000'000 + 180 * 1000LL);
    }
    size_t cap = (size_t)64 * 4096;
    CHECK(lim.bucket_count() <= cap, "bounded by the shard caps");
    CHECK(lim.bucket_count() < 300000, "swept, not just capped");
    // And nothing needed was dropped: a first-wave key answers from a fresh
    // full bucket either way.
    CHECK(lim.check("idle-a-0", 1'000'000 + 180 * 1000LL).allowed, "early keys still answer");
}

// ---------------------------------------------------------------------------
// Registry and abort
// ---------------------------------------------------------------------------

void registry_reconfigures_by_spec() {
    ClockGuard clock;
    hs::RateLimiter& a = hs::limit_registry().get("rt_reg1", hs::LIMIT_TOKEN_BUCKET, 2, 60000);
    CHECK(a.check("k", 1'000'000).allowed, "two budget");
    CHECK(a.check("k", 1'000'000).allowed, "two budget");
    CHECK(!a.check("k", 1'000'000).allowed, "empty");
    // Same id, new budget: the registry follows the code, not first write.
    hs::RateLimiter& b = hs::limit_registry().get("rt_reg1", hs::LIMIT_TOKEN_BUCKET, 5, 60000);
    CHECK(b.check("other", 1'000'000).allowed, "fresh bucket under the new budget");
}

void allowed_denied_counters_feed_metrics() {
    ClockGuard clock;
    uint64_t a0 = hs::limit_registry().allowed();
    uint64_t d0 = hs::limit_registry().denied();
    hs::LimitVerdict v = hs::limit_check("rt_cnt", hs::LIMIT_TOKEN_BUCKET, 1, 60000, "k");
    CHECK(v.allowed, "passes");
    hs::limit_check("rt_cnt", hs::LIMIT_TOKEN_BUCKET, 1, 60000, "k");
    CHECK_EQ(hs::limit_registry().allowed() - a0, uint64_t(1), "one allowed");
    CHECK_EQ(hs::limit_registry().denied() - d0, uint64_t(1), "one denied");
}

void deny_becomes_429_with_retry_after() {
    ClockGuard clock;
    hs::limit_check("rt_429", hs::LIMIT_TOKEN_BUCKET, 1, 60000, "k");
    bool threw = false;
    try {
        hs::limit_check_or_abort("rt_429", hs::LIMIT_TOKEN_BUCKET, 1, 60000, "k");
    } catch (const hs::HttpAbort& a) {
        threw = true;
        CHECK_EQ(a.r.status, 429, "too many requests");
        bool has_retry = false;
        for (const auto& h : a.r.headers) {
            if (h.first == "Retry-After") {
                has_retry = true;
                CHECK(std::stoll(h.second) >= 1, "at least a second");
            }
        }
        CHECK(has_retry, "retry-after present");
    } catch (const std::exception&) {
        threw = false;
    }
    CHECK(threw, "denial aborts the request");
    // Allowed passes silently.
    hs::limit_check_or_abort("rt_429_ok", hs::LIMIT_TOKEN_BUCKET, 10, 60000, "k");
    CHECK(true, "no throw when allowed");
}

// ---------------------------------------------------------------------------
// Concurrency: one shared limiter, exact budget across threads
// ---------------------------------------------------------------------------

void concurrent_checks_split_the_budget_exactly() {
    ClockGuard clock;
    std::atomic<int> allowed{0};
    auto worker = [&] {
        for (int i = 0; i < 2000; i++) {
            hs::LimitVerdict v = hs::limit_check("rt_conc", hs::LIMIT_TOKEN_BUCKET, 100, 60000, "shared");
            if (v.allowed) allowed.fetch_add(1);
        }
    };
    std::vector<std::thread> threads;
    for (int t = 0; t < 8; t++) threads.emplace_back(worker);
    for (auto& th : threads) th.join();
    CHECK_EQ(allowed.load(), 100, "a hundred tokens split eight ways, none lost, none made");
}

void concurrent_windows_agree() {
    ClockGuard clock;
    std::atomic<int> allowed{0};
    auto worker = [&] {
        for (int i = 0; i < 1000; i++) {
            hs::LimitVerdict v = hs::limit_check("rt_concw", hs::LIMIT_SLIDING_WINDOW, 50, 60000, "shared");
            if (v.allowed) allowed.fetch_add(1);
        }
    };
    std::vector<std::thread> threads;
    for (int t = 0; t < 4; t++) threads.emplace_back(worker);
    for (auto& th : threads) th.join();
    CHECK_EQ(allowed.load(), 50, "fifty window slots, exactly");
}

// ---------------------------------------------------------------------------
// Throughput smoke: 100k req/s is the milestone floor. Prints its rate;
// fails only far below it. M6.9 measures properly.
// ---------------------------------------------------------------------------

void checks_hit_the_floor() {
    hs::RateLimiter lim = token_bucket(1000000000LL, 60 * 1000LL);
    const int N = 1000000;
    auto t0 = std::chrono::steady_clock::now();
    int n = 0;
    for (int i = 0; i < N; i++) {
        if (lim.check("bench", 1'000'000 + i / 1000).allowed) n++;
    }
    auto dt = std::chrono::duration<double>(std::chrono::steady_clock::now() - t0).count();
    double per_sec = N / dt;
    printf("  [info] %.0f limit checks/sec\n", per_sec);
    CHECK_EQ(n, N, "all allowed against a huge budget");
    CHECK(per_sec > 100000.0, "above the 100k/s floor");
}

}  // namespace
}  // namespace qa

int main() {
    qa::first_requests_pass_to_capacity();
    qa::tokens_refill_with_time();
    qa::retry_after_counts_down();
    qa::keys_do_not_share_buckets();

    qa::window_allows_then_denies();
    qa::old_hits_age_out();
    qa::retry_after_is_oldest_plus_window();

    qa::bad_specs_throw_before_state();
    qa::key_fallbacks();
    qa::idle_buckets_do_not_accumulate();

    qa::registry_reconfigures_by_spec();
    qa::allowed_denied_counters_feed_metrics();
    qa::deny_becomes_429_with_retry_after();

    qa::concurrent_checks_split_the_budget_exactly();
    qa::concurrent_windows_agree();

    qa::checks_hit_the_floor();

    return qa::report("ratelimit");
}
