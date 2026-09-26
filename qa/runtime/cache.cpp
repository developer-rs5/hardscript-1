// qa/runtime/cache.cpp -- the native cache engine
//
// A TTL map is a promise about time: a value read before its deadline is the
// value written, and after it is nothing. Time is the only untestable input,
// so these tests override the clock and move it by hand; one test sleeps for
// real to prove the wall-clock path too. Concurrency tests prove the shards
// share safely; the throughput smoke prints its rate and only fails on a
// floor no healthy build can miss.

#include "hs_runtime_cache.hpp"

#include "support.hpp"

#include <thread>

namespace qa {
namespace {

// A fake clock, restored on the way out so no test leaks time into the next.
struct ClockGuard {
    explicit ClockGuard(int64_t ms = 1'000'000) { hs::cache_set_now_for_tests(ms); }
    ~ClockGuard() { hs::cache_clear_now_for_tests(); }
    static void at(int64_t ms) { hs::cache_set_now_for_tests(ms); }
};

struct Counters {
    uint64_t hits, misses, evictions, sets;
    static Counters now() {
        hs::Cache& c = hs::cache_default();
        return {c.hits(), c.misses(), c.evictions(), c.sets()};
    }
};

// ---------------------------------------------------------------------------
// Values in, values out
// ---------------------------------------------------------------------------

void stored_values_come_back_equal() {
    hs::Cache& c = hs::cache_default();
    c.set("v_text", hs::Val::text("hello"));
    c.set("v_int", hs::Val::int_(-42));
    c.set("v_bool", hs::Val::boolean(true));
    c.set("v_float", hs::Val::flt(1.5));
    c.set("v_list", hs::Val::list({hs::Val::int_(1), hs::Val::text("two")}));
    c.set("v_obj", hs::Val::object({{"a", hs::Val::int_(1)}, {"b", hs::Val::list({})}}));
    c.set("v_nil", hs::Val::nil());
    CHECK_EQ(c.get("v_text").sv, std::string("hello"), "text");
    CHECK_EQ(c.get("v_int").iv, int64_t(-42), "int, sign and all");
    CHECK_EQ(c.get("v_bool").bv, true, "bool stays bool, not 1");
    CHECK_EQ(c.get("v_float").fv, 1.5, "float stays float");
    CHECK_EQ(c.get("v_list").arr.size(), size_t(2), "list length");
    CHECK(c.get("v_obj").find("a")->iv == 1, "nested object");
    CHECK(c.get("v_nil").is_nil(), "even nil round-trips as nil, not missing");
}

void a_missing_name_is_nil() {
    hs::Cache& c = hs::cache_default();
    CHECK(c.get("v_absent_xyz").is_nil(), "missing reads nil");
    CHECK(!c.exists("v_absent_xyz"), "and does not exist");
    CHECK_EQ(c.ttl("v_absent_xyz"), int64_t(-2), "ttl says -2, missing");
}

void overwrite_replaces_wholesale() {
    hs::Cache& c = hs::cache_default();
    c.set("v_over", hs::Val::int_(1));
    c.set("v_over", hs::Val::text("two"));
    hs::Val got = c.get("v_over");
    CHECK(got.is_str(), "the second write wins, type and all");
    CHECK_EQ(got.sv, std::string("two"), "the second write wins");
}

void large_and_unicode_values_survive() {
    hs::Cache& c = hs::cache_default();
    std::string big(10000, 'x');
    c.set("v_big", hs::Val::text(big));
    CHECK_EQ(c.get("v_big").sv.size(), size_t(10000), "10KB round-trips");
    c.set("v_uni", hs::Val::text("héllo→世界"));
    CHECK_EQ(c.get("v_uni").sv, std::string("héllo→世界"), "utf-8 is bytes");
    c.set("key with spaces", hs::Val::int_(7));
    CHECK_EQ(c.get("key with spaces").iv, int64_t(7), "keys are opaque strings");
}

// ---------------------------------------------------------------------------
// delete / exists / clear / keys
// ---------------------------------------------------------------------------

void delete_reports_what_it_dropped() {
    hs::Cache& c = hs::cache_default();
    c.set("v_del", hs::Val::int_(1));
    CHECK(c.del("v_del"), "present drops true");
    CHECK(!c.del("v_del"), "twice drops false");
    CHECK(c.get("v_del").is_nil(), "and stays gone");
}

void clear_counts_the_live_ones() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    c.set("v_c1", hs::Val::int_(1));
    c.set("v_c2", hs::Val::int_(2), 1);
    ClockGuard::at(1'000'000 + 2000);  // v_c2 dies here
    // The cache is shared with every other test, so the count is whatever is
    // live right now -- measured, not assumed.
    int64_t live = (int64_t)c.keys().size();
    CHECK_EQ(c.clear(), live, "every live entry counted, the dead one not");
    CHECK(c.get("v_c1").is_nil(), "and everything is gone either way");
}

void keys_lists_the_live_sorted() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    c.set("v_k_b", hs::Val::int_(1));
    c.set("v_k_a", hs::Val::int_(2));
    c.set("v_k_dead", hs::Val::int_(3), 1);
    ClockGuard::at(1'000'000 + 2000);
    auto keys = c.keys();
    // Other tests' keys share the cache; these three tell the story.
    bool has_a = false, has_b = false, has_dead = false;
    for (auto& k : keys) {
        if (k == "v_k_a") has_a = true;
        if (k == "v_k_b") has_b = true;
        if (k == "v_k_dead") has_dead = true;
    }
    CHECK(has_a && has_b, "live keys listed");
    CHECK(!has_dead, "dead keys are not");
    CHECK(std::is_sorted(keys.begin(), keys.end()), "sorted");
}

// ---------------------------------------------------------------------------
// TTL: declared, explicit, persist
// ---------------------------------------------------------------------------

void a_declared_name_lives_its_ttl() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    c.declare("v_d1", 100);
    c.set("v_d1", hs::Val::int_(1));
    CHECK_EQ(c.ttl("v_d1"), int64_t(100), "full life at birth");
    ClockGuard::at(1'000'000 + 30'000);
    CHECK_EQ(c.ttl("v_d1"), int64_t(70), "counting down");
    CHECK(c.exists("v_d1"), "still there");
    ClockGuard::at(1'000'000 + 100'001);
    CHECK(!c.exists("v_d1"), "gone a millisecond past");
    CHECK(c.get("v_d1").is_nil(), "reads nil");
}

void an_explicit_ttl_beats_the_declaration() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    c.declare("v_d2", 100);
    c.set("v_d2", hs::Val::int_(1), 10);
    CHECK_EQ(c.ttl("v_d2"), int64_t(10), "explicit wins");
}

void undeclared_names_persist() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    c.set("v_persist", hs::Val::int_(1));
    CHECK_EQ(c.ttl("v_persist"), int64_t(-1), "persist marker");
    ClockGuard::at(1'000'000 + 86400'000LL * 365);
    CHECK(c.exists("v_persist"), "a year later");
}

void overwrite_refreshes_the_deadline() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    c.declare("v_re", 100);
    c.set("v_re", hs::Val::int_(1));
    ClockGuard::at(1'000'000 + 90'000);
    c.set("v_re", hs::Val::int_(2));
    ClockGuard::at(1'000'000 + 110'000);
    CHECK_EQ(c.get("v_re").iv, int64_t(2), "the second write's clock counts");
}

void redeclaring_to_persist_sticks() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    c.declare("v_rp", 100);
    c.set("v_rp", hs::Val::int_(1));
    c.declare("v_rp", -1);
    c.set("v_rp", hs::Val::int_(2));
    CHECK_EQ(c.ttl("v_rp"), int64_t(-1), "persist from here on");
}

// ---------------------------------------------------------------------------
// expire / ttl
// ---------------------------------------------------------------------------

void expire_moves_the_deadline() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    c.declare("v_ex", 1000);
    c.set("v_ex", hs::Val::int_(1));
    CHECK(c.expire("v_ex", 10), "true for a live name");
    CHECK_EQ(c.ttl("v_ex"), int64_t(10), "ten seconds now");
    ClockGuard::at(1'000'000 + 11'000);
    CHECK(!c.exists("v_ex"), "gone after");
}

void expire_missing_is_false() {
    hs::Cache& c = hs::cache_default();
    CHECK(!c.expire("v_ex_absent", 10), "nothing to move");
}

void expire_zero_kills_now() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    c.set("v_ex0", hs::Val::int_(1));
    CHECK(c.expire("v_ex0", 0), "true, it was alive");
    CHECK(!c.exists("v_ex0"), "and now it is not");
}

void real_time_expiry_end_to_end() {
    hs::Cache& c = hs::cache_default();
    c.set("v_real", hs::Val::int_(1), 1);
    CHECK(c.exists("v_real"), "alive immediately");
    std::this_thread::sleep_for(std::chrono::milliseconds(1200));
    CHECK(!c.exists("v_real"), "dead after its second, on the wall clock too");
}

// ---------------------------------------------------------------------------
// The wheel
// ---------------------------------------------------------------------------

void the_sweeper_drops_what_is_due() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    c.set("v_w1", hs::Val::int_(1), 5);
    c.set("v_w2", hs::Val::int_(2), 5000);
    c.set("v_w3", hs::Val::int_(3));
    CHECK_EQ(c.expire_due(1'000'000 + 6'000), int64_t(1), "only the due one");
    CHECK(c.get("v_w1").is_nil(), "gone");
    CHECK(c.exists("v_w2"), "far-future alive");
    CHECK(c.exists("v_w3"), "persist alive");
}

void stale_marks_are_ignored() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    // A short TTL plants a mark; rewriting with a long TTL must survive the
    // old mark arriving.
    c.set("v_stale", hs::Val::int_(1), 5);
    c.set("v_stale", hs::Val::int_(2), 5000);
    CHECK_EQ(c.expire_due(1'000'000 + 6'000), int64_t(0), "the old mark finds a new version");
    CHECK_EQ(c.get("v_stale").iv, int64_t(2), "untouched");
}

void same_slot_later_second_survives() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    // 3605 and 5 land in the same wheel slot, an hour apart.
    c.set("v_slot_a", hs::Val::int_(1), 5);
    c.set("v_slot_b", hs::Val::int_(2), 3605);
    CHECK_EQ(c.expire_due(1'000'000 + 6'000), int64_t(1), "only the due one goes");
    CHECK(c.exists("v_slot_b"), "the later one stays");
}

void an_empty_sweep_sweeps_nothing() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    CHECK_EQ(c.expire_due(1'000'000), int64_t(0), "nothing due, nothing dropped");
}

void expire_call_refreshes_against_old_marks() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    c.set("v_em", hs::Val::int_(1), 5);
    c.expire("v_em", 5000);
    CHECK_EQ(c.expire_due(1'000'000 + 6'000), int64_t(0), "expire() bumped the version past the old mark");
    CHECK(c.exists("v_em"), "alive");
}

void the_sweeper_thread_starts_and_survives() {
    hs::cache_start_sweeper();
    hs::cache_start_sweeper();
    hs::Cache& c = hs::cache_default();
    c.set("v_sw", hs::Val::int_(1));
    CHECK_EQ(c.get("v_sw").iv, int64_t(1), "usable with the sweeper running");
}

// ---------------------------------------------------------------------------
// increment / decrement
// ---------------------------------------------------------------------------

void counters_start_at_zero() {
    hs::Cache& c = hs::cache_default();
    CHECK_EQ(c.incr("v_n1"), int64_t(1), "missing counts from 0 by 1");
    CHECK_EQ(c.incr("v_n1"), int64_t(2), "and on");
    CHECK_EQ(c.decr("v_n2"), int64_t(-1), "decrement from 0");
}

void steps_add_up() {
    hs::Cache& c = hs::cache_default();
    CHECK_EQ(c.incr("v_st", 10), int64_t(10), "a step of ten");
    CHECK_EQ(c.incr("v_st", -3), int64_t(7), "negative steps too");
    CHECK_EQ(c.decr("v_st", 2), int64_t(5), "decrement takes a step as well");
}

void only_integers_increment() {
    hs::Cache& c = hs::cache_default();
    c.set("v_f", hs::Val::flt(1.5));
    c.set("v_s", hs::Val::text("x"));
    c.set("v_b", hs::Val::boolean(true));
    for (const char* k : {"v_f", "v_s", "v_b"}) {
        bool threw = false;
        try {
            c.incr(k);
        } catch (const std::exception& e) {
            threw = std::string(e.what()).find("only integers") != std::string::npos;
        }
        CHECK(threw, std::string("rejected with a reason: ") + k);
    }
}

void an_expired_counter_starts_over() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    c.set("v_xc", hs::Val::int_(100), 5);
    ClockGuard::at(1'000'000 + 6'000);
    CHECK_EQ(c.incr("v_xc"), int64_t(1), "the dead value does not leak into the new count");
}

// ---------------------------------------------------------------------------
// The language surface (free functions)
// ---------------------------------------------------------------------------

void surface_names_must_be_text() {
    for (auto bad : {hs::Val::int_(1), hs::Val::nil(), hs::Val::list({})}) {
        bool threw = false;
        try {
            hs::cache_get(bad);
        } catch (const std::exception& e) {
            threw = std::string(e.what()).find("needs a name") != std::string::npos;
        }
        CHECK(threw, "a non-name is refused with a reason");
    }
    bool threw = false;
    try {
        hs::cache_get(hs::Val::text(""));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "an empty name too");
}

void surface_set_validates_its_ttl() {
    bool threw = false;
    try {
        hs::cache_set(hs::Val::text("v_badttl"), hs::Val::int_(1), hs::Val::int_(-5));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "a negative TTL is refused");
    threw = false;
    try {
        hs::cache_set(hs::Val::text("v_badttl"), hs::Val::int_(1), hs::Val::text("soon"));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "a non-integer TTL too");
}

void surface_declare_validates() {
    CHECK(hs::cache_declare(hs::Val::text("v_sd"), hs::Val::int_(60)).truthy(), "declaring returns true");
    bool threw = false;
    try {
        hs::cache_declare(hs::Val::text("v_sd"), hs::Val::int_(-5));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "below -1 is refused");
}

void surface_round_trip() {
    ClockGuard clock;
    CHECK(hs::cache_declare(hs::Val::text("v_rt"), hs::Val::int_(100)).truthy(), "declare");
    CHECK(hs::cache_set(hs::Val::text("v_rt"), hs::Val::int_(7)).truthy(), "set");
    CHECK_EQ(hs::cache_get(hs::Val::text("v_rt")).iv, int64_t(7), "get");
    CHECK(hs::cache_exists(hs::Val::text("v_rt")).truthy(), "exists");
    CHECK_EQ(hs::cache_ttl(hs::Val::text("v_rt")).iv, int64_t(100), "ttl");
    CHECK_EQ(hs::cache_increment(hs::Val::text("v_rti"), hs::Val::int_(2)).iv, int64_t(2), "increment");
    CHECK_EQ(hs::cache_decrement(hs::Val::text("v_rti"), hs::Val::int_(1)).iv, int64_t(1), "decrement");
    CHECK(hs::cache_expire(hs::Val::text("v_rt"), hs::Val::int_(50)).truthy(), "expire");
    CHECK_EQ(hs::cache_ttl(hs::Val::text("v_rt")).iv, int64_t(50), "moved");
    CHECK(hs::cache_delete(hs::Val::text("v_rt")).truthy(), "delete");
    CHECK(!hs::cache_exists(hs::Val::text("v_rt")).truthy(), "gone");
    int64_t before = (int64_t)hs::cache_keys().arr.size();
    CHECK(hs::cache_set(hs::Val::text("v_rt2"), hs::Val::int_(1)).truthy(), "set again");
    CHECK_EQ((int64_t)hs::cache_keys().arr.size(), before + 1, "keys lists it");
    CHECK(hs::cache_clear().iv >= 1, "clear counts");
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

void hits_and_misses_are_counted() {
    ClockGuard clock;
    Counters before = Counters::now();
    hs::Cache& c = hs::cache_default();
    c.set("v_hm", hs::Val::int_(1));
    (void)c.get("v_hm");
    (void)c.get("v_hm_absent");
    Counters after = Counters::now();
    CHECK_EQ(after.hits - before.hits, uint64_t(1), "one hit");
    CHECK_EQ(after.misses - before.misses, uint64_t(1), "one miss");
    CHECK(after.sets > before.sets, "sets counted");
}

void evictions_are_counted() {
    ClockGuard clock;
    Counters before = Counters::now();
    hs::Cache& c = hs::cache_default();
    c.set("v_ev", hs::Val::int_(1), 5);
    ClockGuard::at(1'000'000 + 6'000);
    (void)c.get("v_ev");
    Counters after = Counters::now();
    CHECK_EQ(after.evictions - before.evictions, uint64_t(1), "lazy expiration counts");
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

void concurrent_sets_and_gets_agree() {
    hs::Cache& c = hs::cache_default();
    std::atomic<int> bad{0};
    auto worker = [&](int t) {
        for (int i = 0; i < 2000; i++) {
            std::string k = "v_mt_" + std::to_string(t) + "_" + std::to_string(i);
            c.set(k, hs::Val::int_(i));
            hs::Val got = c.get(k);
            if (!got.is_int() || got.iv != i) bad.fetch_add(1);
        }
    };
    std::vector<std::thread> threads;
    for (int t = 0; t < 8; t++) threads.emplace_back(worker, t);
    for (auto& th : threads) th.join();
    CHECK_EQ(bad.load(), 0, "16k sets and gets, every read its own write");
}

void concurrent_increments_add_up_exactly() {
    hs::Cache& c = hs::cache_default();
    c.del("v_mtc");
    auto worker = [&] {
        for (int i = 0; i < 1000; i++) c.incr("v_mtc");
    };
    std::vector<std::thread> threads;
    for (int t = 0; t < 8; t++) threads.emplace_back(worker);
    for (auto& th : threads) th.join();
    CHECK_EQ(c.get("v_mtc").iv, int64_t(8000), "eight thousand increments, none lost");
}

void concurrent_ttl_traffic_does_not_crash() {
    ClockGuard clock;
    hs::Cache& c = hs::cache_default();
    auto worker = [&](int t) {
        for (int i = 0; i < 500; i++) {
            std::string k = "v_mtt_" + std::to_string(t);
            c.set(k, hs::Val::int_(i), 100);
            (void)c.ttl(k);
            (void)c.expire(k, 100);
            (void)c.exists(k);
        }
    };
    std::vector<std::thread> threads;
    for (int t = 0; t < 8; t++) threads.emplace_back(worker, t);
    for (auto& th : threads) th.join();
    CHECK(true, "no crash, no deadlock");
}

// ---------------------------------------------------------------------------
// Throughput smoke: prints its rate, fails only below a floor no healthy
// build misses. The milestone's 100k ops/s target is measured properly in
// M6.9; this keeps a pathological slowdown from slipping through.
// ---------------------------------------------------------------------------

void mixed_ops_hit_the_floor() {
    hs::Cache& c = hs::cache_default();
    const int N = 200000;
    auto t0 = std::chrono::steady_clock::now();
    for (int i = 0; i < N; i++) {
        std::string k = "v_bm_" + std::to_string(i % 1000);
        c.set(k, hs::Val::int_(i));
        (void)c.get(k);
        c.incr("v_bm_counter");
    }
    auto dt = std::chrono::duration<double>(std::chrono::steady_clock::now() - t0).count();
    double per_sec = (3.0 * N) / dt;
    printf("  [info] %.0f mixed cache ops/sec\n", per_sec);
    CHECK(per_sec > 20000.0, "above the 20k/s floor");
}

}  // namespace
}  // namespace qa

int main() {
    qa::stored_values_come_back_equal();
    qa::a_missing_name_is_nil();
    qa::overwrite_replaces_wholesale();
    qa::large_and_unicode_values_survive();

    qa::delete_reports_what_it_dropped();
    qa::clear_counts_the_live_ones();
    qa::keys_lists_the_live_sorted();

    qa::a_declared_name_lives_its_ttl();
    qa::an_explicit_ttl_beats_the_declaration();
    qa::undeclared_names_persist();
    qa::overwrite_refreshes_the_deadline();
    qa::redeclaring_to_persist_sticks();

    qa::expire_moves_the_deadline();
    qa::expire_missing_is_false();
    qa::expire_zero_kills_now();
    qa::real_time_expiry_end_to_end();

    qa::the_sweeper_drops_what_is_due();
    qa::stale_marks_are_ignored();
    qa::same_slot_later_second_survives();
    qa::an_empty_sweep_sweeps_nothing();
    qa::expire_call_refreshes_against_old_marks();
    qa::the_sweeper_thread_starts_and_survives();

    qa::counters_start_at_zero();
    qa::steps_add_up();
    qa::only_integers_increment();
    qa::an_expired_counter_starts_over();

    qa::surface_names_must_be_text();
    qa::surface_set_validates_its_ttl();
    qa::surface_declare_validates();
    qa::surface_round_trip();

    qa::hits_and_misses_are_counted();
    qa::evictions_are_counted();

    qa::concurrent_sets_and_gets_agree();
    qa::concurrent_increments_add_up_exactly();
    qa::concurrent_ttl_traffic_does_not_crash();

    qa::mixed_ops_hit_the_floor();

    return qa::report("cache");
}
