// qa/runtime/sched.cpp -- schedule engine
//
// Time is the only untestable input, so the next-fire math is free functions
// driven with fixed clocks, and threads only prove firing, catch-up and
// shutdown. The fake clock freezes `sched_now_ms`; the math never sees a
// thread.

#include "hs_runtime_schedule.hpp"
#include "hs_runtime_sqlite.hpp"

#include "support.hpp"

#include <thread>

namespace qa {
namespace {

struct ClockGuard {
    explicit ClockGuard(int64_t ms = 1767225600000LL) { hs::sched_set_now_for_tests(ms); }
    ~ClockGuard() { hs::sched_clear_now_for_tests(); }
    static void at(int64_t ms) { hs::sched_set_now_for_tests(ms); }
};

// 2026-01-01 00:00:00 UTC, a Thursday.
constexpr int64_t T0 = 1767225600000LL;
constexpr int64_t DAY = 86400 * 1000LL;

// ---------------------------------------------------------------------------
// Parsing zones, weekdays, times
// ---------------------------------------------------------------------------

void zones_parse_or_refuse() {
    CHECK_EQ(hs::sched_parse_zone("UTC"), 0, "utc");
    CHECK_EQ(hs::sched_parse_zone("Z"), 0, "z");
    CHECK_EQ(hs::sched_parse_zone("utc"), 0, "case-insensitive");
    CHECK_EQ(hs::sched_parse_zone("+05:30"), 330, "india");
    CHECK_EQ(hs::sched_parse_zone("-08:00"), -480, "pacific");
    CHECK_EQ(hs::sched_parse_zone("+0000"), 0, "no colon");
    CHECK_EQ(hs::sched_parse_zone("local"), hs::SCHED_LOCAL_ZONE, "system zone");
    for (const char* bad : {"", "Mars", "+25:00", "+05:61", "++05:00", "+5", "EST"}) {
        bool threw = false;
        try {
            hs::sched_parse_zone(bad);
        } catch (const std::exception& e) {
            threw = std::string(e.what()).find("unknown timezone") != std::string::npos;
        }
        CHECK(threw, std::string("refused with a reason: ") + bad);
    }
}

void weekdays_parse_monday_first() {
    CHECK_EQ(hs::sched_parse_weekday("monday"), 0, "monday is 0");
    CHECK_EQ(hs::sched_parse_weekday("sunday"), 6, "sunday is 6");
    CHECK_EQ(hs::sched_parse_weekday("Fri"), 4, "abbreviations");
    CHECK_EQ(hs::sched_parse_weekday("WEDNESDAY"), 2, "case-insensitive");
    CHECK_EQ(hs::sched_parse_weekday("day"), -1, "every day");
    bool threw = false;
    try {
        hs::sched_parse_weekday("someday");
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "someday is not a day");
}

void times_parse_or_refuse() {
    int h, m, s;
    hs::sched_parse_time("03:00", h, m, s);
    CHECK(h == 3 && m == 0 && s == 0, "minutes default seconds");
    hs::sched_parse_time("17:30:15", h, m, s);
    CHECK(h == 17 && m == 30 && s == 15, "full form");
    hs::sched_parse_time("3:00", h, m, s);
    CHECK(h == 3, "unpadded hour");
    for (const char* bad : {"", "3", "25:00", "03:61", "03:00:61", "noon", "3-00"}) {
        bool threw = false;
        try {
            hs::sched_parse_time(bad, h, m, s);
        } catch (const std::exception&) {
            threw = true;
        }
        CHECK(threw, std::string("refused: ") + bad);
    }
}

// ---------------------------------------------------------------------------
// Civil math round-trips
// ---------------------------------------------------------------------------

void epoch_and_civil_agree() {
    CHECK_EQ(hs::sched_epoch(2026, 1, 1, 0, 0, 0, 0), T0, "the anchor");
    int y, mo, d, w, hh, mm, ss;
    hs::sched_civil(T0, 0, y, mo, d, w, hh, mm, ss);
    CHECK(y == 2026 && mo == 1 && d == 1, "date back");
    CHECK_EQ(w, 3, "Thursday is 3 Monday-first");
    CHECK(hh == 0 && mm == 0 && ss == 0, "midnight back");
    // A fixed zone shifts the wall clock but not the instant.
    hs::sched_civil(T0, 330, y, mo, d, w, hh, mm, ss);
    CHECK(hh == 5 && mm == 30, "+05:30 reads 05:30");
    CHECK_EQ(hs::sched_epoch(2026, 1, 1, 5, 30, 0, 330), T0, "and back to the instant");
    // Negative side of the line: 16:00 at -08:00 is midnight UTC.
    CHECK_EQ(hs::sched_epoch(2025, 12, 31, 16, 0, 0, -480), T0, "across the date line");
}

void local_zone_round_trips() {
    int64_t now = T0 + 12 * 3600 * 1000LL;
    int y, mo, d, w, hh, mm, ss;
    hs::sched_civil(now, hs::SCHED_LOCAL_ZONE, y, mo, d, w, hh, mm, ss);
    int64_t back = hs::sched_epoch(y, mo, d, hh, mm, ss, hs::SCHED_LOCAL_ZONE);
    CHECK(back >= 0, "mktime answers");
    // To the minute: seconds can round-trip across a DST guess, minutes may not move.
    CHECK(back / 60000LL == now / 60000LL, "round-trips to the minute");
}

// ---------------------------------------------------------------------------
// Next and previous fires
// ---------------------------------------------------------------------------

void intervals_measure_from_now() {
    CHECK_EQ(hs::sched_next_interval_ms(T0, 3600), T0 + 3600 * 1000LL, "one period out");
    CHECK_EQ(hs::sched_next_interval_ms(T0, 1), T0 + 1000LL, "a second");
}

void daily_fires_at_the_wall_time() {
    CHECK_EQ(hs::sched_next_daily_ms(T0, 3, 0, 0, 0), T0 + 3 * 3600 * 1000LL, "later today");
    CHECK_EQ(hs::sched_next_daily_ms(T0 + 5 * 3600 * 1000LL, 3, 0, 0, 0), T0 + 86400 * 1000LL + 3 * 3600 * 1000LL,
             "tomorrow once passed");
    // Exactly on the second is not strictly after it.
    CHECK_EQ(hs::sched_next_daily_ms(T0 + 3 * 3600 * 1000LL, 3, 0, 0, 0),
             T0 + 86400 * 1000LL + 3 * 3600 * 1000LL, "strictly after");
}

void daily_respects_the_zone() {
    // 03:00 at +05:30 on 2026-01-01 is 2025-12-31 21:30 UTC, already past at
    // midnight UTC, so the next is a day later at 21:30 UTC.
    int64_t got = hs::sched_next_daily_ms(T0, 3, 0, 0, 330);
    CHECK_EQ(got, T0 + 21 * 3600 * 1000LL + 30 * 60 * 1000LL, "next wall 03:00+05:30 in UTC");
    int y, mo, d, w, hh, mm, ss;
    hs::sched_civil(got, 330, y, mo, d, w, hh, mm, ss);
    CHECK(hh == 3 && mm == 0, "reads 03:00 in its own zone");
}

void weekly_finds_its_day() {
    // Thursday 2026-01-01: next Monday 09:00 is 2026-01-05 09:00 UTC.
    CHECK_EQ(hs::sched_next_weekly_ms(T0, 0, 9, 0, 0, 0), 1767571200LL * 1000 + 9 * 3600 * 1000LL,
             "monday after thursday");
    // Same day, time passed: next week, not in an hour.
    CHECK_EQ(hs::sched_next_weekly_ms(T0 + 10 * 3600 * 1000LL, 3, 9, 0, 0, 0),
             T0 + 7 * 86400 * 1000LL + 9 * 3600 * 1000LL, "thursday 09:00 next week");
    // Same day, time ahead: today.
    CHECK_EQ(hs::sched_next_weekly_ms(T0, 3, 9, 0, 0, 0), T0 + 9 * 3600 * 1000LL, "thursday 09:00 today");
}

void previous_fires_look_back() {
    CHECK_EQ(hs::sched_prev_daily_ms(T0 + 5 * 3600 * 1000LL, 3, 0, 0, 0), T0 + 3 * 3600 * 1000LL,
             "today's 03:00, already passed");
    CHECK_EQ(hs::sched_prev_daily_ms(T0 + 2 * 3600 * 1000LL, 3, 0, 0, 0), T0 - 86400 * 1000LL + 3 * 3600 * 1000LL,
             "yesterday's, today's still ahead");
    CHECK_EQ(hs::sched_prev_weekly_ms(T0 + 10 * 3600 * 1000LL, 3, 9, 0, 0, 0), T0 + 9 * 3600 * 1000LL,
             "this morning's weekly");
    CHECK_EQ(hs::sched_prev_weekly_ms(T0, 0, 9, 0, 0, 0), T0 - 3 * 86400 * 1000LL + 9 * 3600 * 1000LL,
             "last monday from thursday midnight");
}

// ---------------------------------------------------------------------------
// Registry: validation, duplicates, state
// ---------------------------------------------------------------------------

void duplicate_same_spec_is_quiet_but_conflict_throws() {
    hs::SchedRuntime r;
    hs::Schedule a;
    a.name = "dup";
    a.kind = hs::SchedKind::Interval;
    a.every_secs = 60;
    r.register_schedule(a, [] { return hs::Val::nil(); });
    // Same spec again: idempotent, for startup paths and tests that run twice.
    r.register_schedule(a, [] { return hs::Val::nil(); });
    CHECK(r.has_schedule("dup"), "still there");
    hs::Schedule b = a;
    b.every_secs = 120;
    bool threw = false;
    try {
        r.register_schedule(b, [] { return hs::Val::nil(); });
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("already registered") != std::string::npos;
    }
    CHECK(threw, "same name, different spec is a conflict");
}

void registration_validates() {
    hs::SchedRuntime r;
    hs::Schedule a;
    a.kind = hs::SchedKind::Interval;
    a.every_secs = 0;
    bool threw = false;
    try {
        r.register_schedule(a, [] { return hs::Val::nil(); });
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "nameless schedule refused");
    a.name = "zero";
    threw = false;
    try {
        r.register_schedule(a, [] { return hs::Val::nil(); });
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "zero interval refused");
    a.every_secs = 60;
    threw = false;
    try {
        r.register_schedule(a, nullptr);
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "null handler refused");
}

void surface_validates_too() {
    bool threw = false;
    try {
        hs::schedule_register_interval(hs::Val::text(""), hs::Val::int_(60),
                                       [] { return hs::Val::nil(); });
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "empty name");
    threw = false;
    try {
        hs::schedule_register_daily(hs::Val::text("d"), hs::Val::int_(25), hs::Val::int_(0),
                                    hs::Val::int_(0), hs::Val::nil(),
                                    [] { return hs::Val::nil(); });
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "hour 25");
    threw = false;
    try {
        hs::schedule_register_daily(hs::Val::text("d"), hs::Val::int_(3), hs::Val::int_(0),
                                    hs::Val::int_(0), hs::Val::text("Mars"),
                                    [] { return hs::Val::nil(); });
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("unknown timezone") != std::string::npos;
    }
    CHECK(threw, "unknown zone names itself");
    threw = false;
    try {
        hs::schedule_register_weekly(hs::Val::text("w"), hs::Val::int_(7), hs::Val::int_(9),
                                     hs::Val::int_(0), hs::Val::int_(0), hs::Val::nil(),
                                     [] { return hs::Val::nil(); });
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "weekday 7");
}

// ---------------------------------------------------------------------------
// Threads: firing, startup, catch-up
// ---------------------------------------------------------------------------

template <typename F>
bool wait_for(int64_t ms, F cond) {
    auto t0 = std::chrono::steady_clock::now();
    while (std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now() - t0)
               .count() < ms) {
        if (cond()) return true;
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
    return cond();
}

void intervals_fire_repeatedly() {
    ClockGuard clock(T0);
    hs::SchedRuntime r;
    std::atomic<int> fires{0};
    hs::Schedule s;
    s.name = "tick";
    s.kind = hs::SchedKind::Interval;
    s.every_secs = 3600;
    r.register_schedule(s, [&]() -> hs::Val {
        fires.fetch_add(1);
        return hs::Val::nil();
    });
    // Nothing fires at registration: the first fire is a full period out.
    std::this_thread::sleep_for(std::chrono::milliseconds(50));
    CHECK_EQ(fires.load(), 0, "no immediate fire");
    ClockGuard::at(T0 + 3600 * 1000LL + 1);
    CHECK(wait_for(2000, [&] { return fires.load() >= 1; }), "fires after its period");
    ClockGuard::at(T0 + 2 * 3600 * 1000LL + 1);
    CHECK(wait_for(2000, [&] { return fires.load() >= 2; }), "and again");
}

void startup_fires_once() {
    ClockGuard clock(T0);
    hs::SchedRuntime r;
    std::atomic<int> fires{0};
    hs::Schedule s;
    s.name = "boot";
    s.kind = hs::SchedKind::Startup;
    r.register_schedule(s, [&]() -> hs::Val {
        fires.fetch_add(1);
        return hs::Val::nil();
    });
    CHECK(wait_for(2000, [&] { return fires.load() == 1; }), "runs at startup");
    std::this_thread::sleep_for(std::chrono::milliseconds(100));
    CHECK_EQ(fires.load(), 1, "exactly once");
    CHECK(!r.has_schedule("boot"), "startup schedules leave after firing");
}

void cron_fires_at_its_wall_time() {
    ClockGuard clock(T0);
    hs::SchedRuntime r;
    std::atomic<int> fires{0};
    hs::Schedule s;
    s.name = "backup";
    s.kind = hs::SchedKind::Daily;
    s.hour = 3;
    s.zone_min = 0;
    r.register_schedule(s, [&]() -> hs::Val {
        fires.fetch_add(1);
        return hs::Val::nil();
    });
    ClockGuard::at(T0 + 3 * 3600 * 1000LL + 1);
    CHECK(wait_for(2000, [&] { return fires.load() == 1; }), "fires at 03:00");
    CHECK_EQ(r.last_run("backup") / 1000LL, (T0 + 3 * 3600 * 1000LL) / 1000LL, "last run recorded");
}

void missed_daily_fires_immediately_once() {
    // Fresh install: no last run recorded, so nothing counts as missed even
    // though earlier wall times passed. Catch-up with real persisted state is
    // covered below.
    ClockGuard clock(T0 + 5 * 3600 * 1000LL);  // Thursday 05:00
    hs::SchedRuntime r;
    hs::Schedule s;
    s.name = "daily";
    s.kind = hs::SchedKind::Daily;
    s.hour = 3;
    s.zone_min = 0;
    std::atomic<int> fires{0};
    r.register_schedule(s, [&]() -> hs::Val {
        fires.fetch_add(1);
        return hs::Val::nil();
    });
    std::this_thread::sleep_for(std::chrono::milliseconds(50));
    CHECK_EQ(fires.load(), 0, "fresh installs do not catch up");
}

void missed_daily_with_state_fires_now() {
    // The real catch-up path: state says the last run predates the most
    // recent scheduled time.
    ClockGuard clock(T0 + 5 * 3600 * 1000LL);  // Thursday 05:00, today's 03:00 passed
    hs::SchedRuntime r;
    hs::SchedMemory* mem = new hs::SchedMemory();
    mem->mark_run("daily2", T0 - 86400 * 1000LL + 3 * 3600 * 1000LL);  // Wednesday 03:00
    r.use_state(mem);
    std::atomic<int> fires{0};
    hs::Schedule s;
    s.name = "daily2";
    s.kind = hs::SchedKind::Daily;
    s.hour = 3;
    s.zone_min = 0;
    r.register_schedule(s, [&]() -> hs::Val {
        fires.fetch_add(1);
        return hs::Val::nil();
    });
    CHECK(wait_for(2000, [&] { return fires.load() == 1; }), "catches up immediately");
    CHECK_EQ(r.missed(), uint64_t(1), "counted as missed");
    // And the rhythm resumes tomorrow, not in a backlog.
    std::this_thread::sleep_for(std::chrono::milliseconds(50));
    CHECK_EQ(fires.load(), 1, "exactly once, no backlog");
}

void throwing_handlers_do_not_kill_time() {
    ClockGuard clock(T0);
    hs::SchedRuntime r;
    std::atomic<int> fires{0};
    hs::Schedule s;
    s.name = "flaky";
    s.kind = hs::SchedKind::Interval;
    s.every_secs = 3600;
    r.register_schedule(s, [&]() -> hs::Val {
        fires.fetch_add(1);
        throw std::runtime_error("boom");
        return hs::Val::nil();
    });
    ClockGuard::at(T0 + 3600 * 1000LL + 1);
    CHECK(wait_for(2000, [&] { return fires.load() >= 1; }), "ran");
    ClockGuard::at(T0 + 2 * 3600 * 1000LL + 1);
    CHECK(wait_for(2000, [&] { return fires.load() >= 2; }), "keeps rhythm after throwing");
}

// ---------------------------------------------------------------------------
// SQLite state
// ---------------------------------------------------------------------------

void sqlite_state_persists_across_reopen() {
    ClockGuard clock(T0);
    std::unique_ptr<hs::SqliteDb> db(new hs::SqliteDb(":memory:"));
    {
        hs::SchedSqlState st(db.get());
        CHECK_EQ(st.last_run("job1"), int64_t(-1), "fresh is -1");
        st.mark_run("job1", T0);
        CHECK_EQ(st.last_run("job1"), T0, "reads back");
    }
    {
        hs::SchedSqlState reopened(db.get());
        CHECK_EQ(reopened.last_run("job1"), T0, "survives reopen");
        CHECK_EQ(reopened.last_run("job2"), int64_t(-1), "unknown still -1");
    }
}

void sqlite_state_drives_catch_up() {
    // End to end: yesterday's run persisted, process "restarts" (new runtime
    // on the same database), today's fire was missed, catch-up runs now.
    ClockGuard clock(T0 + 5 * 3600 * 1000LL);
    std::unique_ptr<hs::SqliteDb> db(new hs::SqliteDb(":memory:"));
    {
        hs::SchedSqlState st(db.get());
        st.mark_run("nightly", T0 - 86400 * 1000LL + 3 * 3600 * 1000LL);
    }
    hs::SchedRuntime r;
    r.use_state(new hs::SchedSqlState(db.get()));
    std::atomic<int> fires{0};
    hs::Schedule s;
    s.name = "nightly";
    s.kind = hs::SchedKind::Daily;
    s.hour = 3;
    s.zone_min = 0;
    r.register_schedule(s, [&]() -> hs::Val {
        fires.fetch_add(1);
        return hs::Val::nil();
    });
    CHECK(wait_for(2000, [&] { return fires.load() == 1; }), "missed nightly runs on start");
    CHECK_EQ(r.missed(), uint64_t(1), "counted");
}

// ---------------------------------------------------------------------------
// Language surface validation
// ---------------------------------------------------------------------------

void surface_register_validates() {
    bool threw = false;
    try {
        hs::schedule_register_interval(hs::Val::text("x"), hs::Val::int_(0),
                                       [] { return hs::Val::nil(); });
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "zero interval refused");
    threw = false;
    try {
        hs::schedule_register_interval(hs::Val::text("x"), hs::Val::text("hour"),
                                       [] { return hs::Val::nil(); });
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "non-integer interval refused");
}

// ---------------------------------------------------------------------------
// Dispatch overhead smoke: pure next-fire math, 100k evaluations. The 5us/job
// target is measured properly in M6.9; this keeps a pathological slowdown
// from slipping through.
// ---------------------------------------------------------------------------

void next_fire_math_is_cheap() {
    auto t0 = std::chrono::steady_clock::now();
    const int N = 100000;
    volatile int64_t sink = 0;
    for (int i = 0; i < N; i++) {
        sink += hs::sched_next_daily_ms(T0 + i * 1000LL, 3, 0, 0, 0);
        sink += hs::sched_next_weekly_ms(T0 + i * 1000LL, i % 7, 9, 30, 0, 330);
    }
    auto dt = std::chrono::duration<double>(std::chrono::steady_clock::now() - t0).count();
    double per_op = dt / (2.0 * N) * 1e6;
    printf("  [info] %.2f us/next-fire computation\n", per_op);
    CHECK(sink != 0, "kept");
    CHECK(per_op < 5.0, "under the 5us/job dispatch budget with room");
}

}  // namespace
}  // namespace qa

int main() {
    qa::zones_parse_or_refuse();
    qa::weekdays_parse_monday_first();
    qa::times_parse_or_refuse();

    qa::epoch_and_civil_agree();
    qa::local_zone_round_trips();

    qa::intervals_measure_from_now();
    qa::daily_fires_at_the_wall_time();
    qa::daily_respects_the_zone();
    qa::weekly_finds_its_day();
    qa::previous_fires_look_back();

    qa::duplicate_same_spec_is_quiet_but_conflict_throws();
    qa::registration_validates();
    qa::surface_validates_too();

    qa::intervals_fire_repeatedly();
    qa::startup_fires_once();
    qa::cron_fires_at_its_wall_time();
    qa::missed_daily_fires_immediately_once();
    qa::missed_daily_with_state_fires_now();
    qa::throwing_handlers_do_not_kill_time();

    qa::sqlite_state_persists_across_reopen();
    qa::sqlite_state_drives_catch_up();

    qa::surface_register_validates();

    qa::next_fire_math_is_cheap();

    return qa::report("sched");
}
