// qa/runtime/queue.cpp -- background job queue
//
// A queue is a promise about every job: it runs once, it runs on time, and
// when it cannot run it is either retried or buried where an operator finds
// it. Time is faked so delay and backoff are deterministic; threads prove
// concurrency; SQLite proves persistence; the mock proves the PostgreSQL
// hook speaks the wire.

#include "hs_runtime_queue.hpp"
#include "hs_runtime_sqlite.hpp"

#include "support.hpp"

#include <thread>

namespace qa {
namespace {

struct ClockGuard {
    explicit ClockGuard(int64_t ms = 1'000'000) { hs::queue_set_now_for_tests(ms); }
    ~ClockGuard() { hs::queue_clear_now_for_tests(); }
    static void at(int64_t ms) { hs::queue_set_now_for_tests(ms); }
};

hs::Job mkjob(const std::string& type, int priority = 0, int64_t run_after = 0) {
    hs::Job j;
    j.type = type;
    j.payload = hs::Val::object({{"n", hs::Val::int_(1)}});
    j.max_attempts = 5;
    j.priority = priority;
    j.run_after_ms = run_after;
    return j;
}

// ---------------------------------------------------------------------------
// Memory backend: order, priority, delay
// ---------------------------------------------------------------------------

void waiting_jobs_come_out_oldest_ready_first() {
    ClockGuard clock;
    hs::MemoryBackend b;
    b.push(mkjob("a"));
    ClockGuard::at(1'000'001);
    b.push(mkjob("a"));
    hs::Job out;
    CHECK(b.poll({"a"}, 1'000'001, out), "something ready");
    CHECK_EQ(out.id, int64_t(1), "the oldest first");
    CHECK(b.poll({"a"}, 1'000'001, out), "and then");
    CHECK_EQ(out.id, int64_t(2), "the next");
    CHECK(!b.poll({"a"}, 1'000'001, out), "then nothing");
}

void priority_beats_age() {
    ClockGuard clock;
    hs::MemoryBackend b;
    b.push(mkjob("a", 0));
    b.push(mkjob("a", 10));
    b.push(mkjob("a", 5));
    hs::Job out;
    CHECK(b.poll({"a"}, 1'000'000, out), "ready");
    CHECK_EQ(out.priority, 10, "most important first");
    CHECK(b.poll({"a"}, 1'000'000, out), "ready");
    CHECK_EQ(out.priority, 5, "then next");
    CHECK(b.poll({"a"}, 1'000'000, out), "ready");
    CHECK_EQ(out.priority, 0, "then last");
}

void the_future_waits() {
    ClockGuard clock;
    hs::MemoryBackend b;
    b.push(mkjob("a", 0, 1'005'000));
    hs::Job out;
    CHECK(!b.poll({"a"}, 1'000'000, out), "not yet");
    CHECK(b.poll({"a"}, 1'005'000, out), "now");
    CHECK_EQ(out.id, int64_t(1), "the same job");
}

void types_do_not_leak_into_each_other() {
    ClockGuard clock;
    hs::MemoryBackend b;
    b.push(mkjob("a"));
    hs::Job out;
    CHECK(!b.poll({"b"}, 1'000'000, out), "no b jobs");
    CHECK(b.poll({}, 1'000'000, out), "empty filter means any");
    CHECK_EQ(out.type, std::string("a"), "the a job");
}

void depth_counts_the_waiting() {
    ClockGuard clock;
    hs::MemoryBackend b;
    CHECK_EQ(b.depth(""), int64_t(0), "empty");
    b.push(mkjob("a"));
    b.push(mkjob("a"));
    b.push(mkjob("b"));
    CHECK_EQ(b.depth(""), int64_t(3), "all");
    CHECK_EQ(b.depth("a"), int64_t(2), "by type");
    CHECK_EQ(b.depth("zzz"), int64_t(0), "unknown type is zero, not an error");
    hs::Job out;
    (void)b.poll({"a"}, 1'000'000, out);
    CHECK_EQ(b.depth(""), int64_t(2), "claimed is not waiting");
}

void complete_forgets_and_fail_remembers() {
    ClockGuard clock;
    hs::MemoryBackend b;
    b.push(mkjob("a"));
    b.push(mkjob("a"));
    hs::Job one, two;
    CHECK(b.poll({"a"}, 1'000'000, one), "claim one");
    CHECK(b.poll({"a"}, 1'000'000, two), "claim two");
    b.track(one);
    b.track(two);
    b.complete(one.id);
    CHECK_EQ(b.depth(""), int64_t(0), "completed is gone");
    b.fail(two.id, 1, "boom", 0);
    CHECK_EQ(b.dlq("").size(), size_t(1), "buried");
    CHECK_EQ(b.dlq("")[0].last_error, std::string("boom"), "with its reason");
    CHECK_EQ(b.dlq("")[0].attempts, 1, "and its count");
    CHECK_EQ(b.dlq("a").size(), size_t(1), "listed by type");
    CHECK_EQ(b.dlq("b").size(), size_t(0), "other types empty");
}

void fail_with_a_time_requeues() {
    ClockGuard clock;
    hs::MemoryBackend b;
    b.push(mkjob("a"));
    hs::Job out;
    CHECK(b.poll({"a"}, 1'000'000, out), "claim");
    b.track(out);
    b.fail(out.id, 1, "flaky", 1'010'000);
    CHECK(!b.poll({"a"}, 1'000'000, out), "not yet");
    CHECK(b.poll({"a"}, 1'010'000, out), "after the backoff");
    CHECK_EQ(out.attempts, 1, "attempts traveled");
    CHECK_EQ(out.last_error, std::string("flaky"), "and so did the error");
}

void failing_unknown_ids_is_quiet() {
    ClockGuard clock;
    hs::MemoryBackend b;
    b.fail(999, 1, "x", 0);
    b.complete(999);
    CHECK_EQ(b.dlq("").size(), size_t(0), "nothing buried for a stranger");
}

void clear_empties_everything() {
    ClockGuard clock;
    hs::MemoryBackend b;
    b.push(mkjob("a"));
    hs::Job out;
    (void)b.poll({"a"}, 1'000'000, out);
    b.track(out);
    b.fail(out.id, 1, "x", 0);
    b.clear();
    CHECK_EQ(b.depth(""), int64_t(0), "waiting gone");
    CHECK_EQ(b.dlq("").size(), size_t(0), "buried gone");
}

void backoff_doubles_to_a_minute() {
    CHECK_EQ(hs::queue_backoff_ms(1), int64_t(1000), "1s after the first failure");
    CHECK_EQ(hs::queue_backoff_ms(2), int64_t(2000), "2s");
    CHECK_EQ(hs::queue_backoff_ms(3), int64_t(4000), "4s");
    CHECK_EQ(hs::queue_backoff_ms(10), int64_t(60000), "capped at a minute");
    CHECK_EQ(hs::queue_backoff_ms(0), int64_t(1000), "never below a second");
}

// ---------------------------------------------------------------------------
// SQL backend on real SQLite
// ---------------------------------------------------------------------------

hs::SqliteDb* test_db() {
    // One database per test file section would be cleaner, but the backend
    // needs an owner: fresh database per test, closed (freed) at the end.
    return new hs::SqliteDb(":memory:");
}

void sql_push_poll_complete_round_trip() {
    ClockGuard clock;
    std::unique_ptr<hs::SqliteDb> db(test_db());
    hs::SqlJobBackend b(db.get());
    hs::Job j = mkjob("mail");
    j.payload = hs::Val::object({{"to", hs::Val::text("a@b.c")}});
    int64_t id = b.push(j);
    CHECK(id > 0, "an id comes back");
    CHECK_EQ(b.depth(""), int64_t(1), "waiting");
    hs::Job out;
    CHECK(b.poll({"mail"}, 1'000'000, out), "claimed");
    CHECK_EQ(out.id, id, "the same job");
    CHECK_EQ(out.payload.find("to")->sv, std::string("a@b.c"), "payload survived JSON");
    b.complete(id);
    CHECK_EQ(b.depth(""), int64_t(0), "completed");
}

void sql_orders_by_time_priority_id() {
    ClockGuard clock;
    std::unique_ptr<hs::SqliteDb> db(test_db());
    hs::SqlJobBackend b(db.get());
    hs::Job a = mkjob("t");
    hs::Job c = mkjob("t", 10);
    hs::Job d = mkjob("t", 0, 1'005'000);
    b.push(a);
    b.push(c);
    b.push(d);
    hs::Job out;
    CHECK(b.poll({"t"}, 1'000'000, out), "ready");
    CHECK_EQ(out.priority, 10, "priority first");
    // The second poll takes the plain one (id order); the delayed waits.
    hs::Job second;
    CHECK(b.poll({"t"}, 1'000'000, second), "something else ready");
    CHECK_EQ(second.priority, 0, "the unprioritized one");
    CHECK(!b.poll({"t"}, 1'000'000, out), "delayed not yet");
    CHECK(b.poll({"t"}, 1'005'000, out), "delayed now");
}

void sql_fail_requeues_and_buries() {
    ClockGuard clock;
    std::unique_ptr<hs::SqliteDb> db(test_db());
    hs::SqlJobBackend b(db.get());
    int64_t id = b.push(mkjob("t"));
    hs::Job out;
    CHECK(b.poll({"t"}, 1'000'000, out), "claimed");
    b.fail(id, 1, "flaky", 1'010'000);
    CHECK(!b.poll({"t"}, 1'000'000, out), "waiting out the backoff");
    CHECK(b.poll({"t"}, 1'010'000, out), "retried");
    CHECK_EQ(out.attempts, 1, "attempts persisted");
    b.fail(id, 2, "dead", 0);
    auto buried = b.dlq("");
    CHECK_EQ(buried.size(), size_t(1), "buried");
    CHECK_EQ(buried[0].last_error, std::string("dead"), "with the last error");
    CHECK_EQ(b.dlq("other").size(), size_t(0), "filtered by type");
}

void sql_claims_recover_on_open() {
    ClockGuard clock;
    std::unique_ptr<hs::SqliteDb> db(test_db());
    {
        hs::SqlJobBackend b(db.get());
        b.push(mkjob("t"));
        hs::Job out;
        CHECK(b.poll({"t"}, 1'000'000, out), "claimed");
        // No complete, no fail: the crash between them.
    }
    {
        // Reopening requeues the claimed: missed-job recovery.
        hs::SqlJobBackend reopened(db.get());
        CHECK_EQ(reopened.depth(""), int64_t(1), "the crashed claim waits again");
        hs::Job out;
        CHECK(reopened.poll({"t"}, 1'000'000, out), "claimable");
        reopened.complete(out.id);
    }
}

void sql_clear_and_depth() {
    ClockGuard clock;
    std::unique_ptr<hs::SqliteDb> db(test_db());
    hs::SqlJobBackend b(db.get());
    b.push(mkjob("a"));
    b.push(mkjob("b"));
    CHECK_EQ(b.depth("a"), int64_t(1), "by type");
    b.clear();
    CHECK_EQ(b.depth(""), int64_t(0), "cleared");
}

void sql_bad_payload_decodes_empty() {
    ClockGuard clock;
    std::unique_ptr<hs::SqliteDb> db(test_db());
    hs::SqlJobBackend b(db.get());
    db->run("INSERT INTO \"hs_jobs\" (\"type\", \"payload\", \"attempts\", \"max_attempts\", \"priority\","
            " \"run_after\", \"status\", \"last_error\", \"created\") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            {hs::Val::text("t"), hs::Val::text("{oops"), hs::Val::int_(0), hs::Val::int_(5),
             hs::Val::int_(0), hs::Val::int_(0), hs::Val::text("queued"), hs::Val::text(""),
             hs::Val::int_(0)});
    hs::Job out;
    CHECK(b.poll({"t"}, 1'000'000, out), "still claimable");
    CHECK(out.payload.is_obj(), "decoded to an empty object, not a crash");
    b.complete(out.id);
}

// ---------------------------------------------------------------------------
// Registry and language surface
// ---------------------------------------------------------------------------

void declare_registers_arity_and_budget() {
    hs::queue_runtime().use_backend(new hs::MemoryBackend());
    CHECK(hs::queue_declare_job(hs::Val::text("q_d1"), hs::Val::list({hs::Val::text("user")}),
                                hs::Val::nil())
              .truthy(),
          "declaring returns true");
    // Re-declaring updates instead of failing: startup and tests both run it.
    CHECK(hs::queue_declare_job(hs::Val::text("q_d1"), hs::Val::list({hs::Val::text("user")}),
                                hs::Val::int_(3))
              .truthy(),
          "twice is fine");
    bool threw = false;
    try {
        hs::queue_declare_job(hs::Val::text(""), hs::Val::list({}), hs::Val::nil());
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "an empty name is refused");
    threw = false;
    try {
        hs::queue_declare_job(hs::Val::text("q_d1"), hs::Val::text("nope"), hs::Val::nil());
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "params must be a list");
    threw = false;
    try {
        hs::queue_declare_job(hs::Val::text("q_d1"), hs::Val::list({}), hs::Val::int_(0));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "zero attempts is refused");
}

void enqueue_validates_everything() {
    ClockGuard clock;
    hs::queue_runtime().use_backend(new hs::MemoryBackend());
    hs::queue_declare_job(hs::Val::text("q_e1"), hs::Val::list({hs::Val::text("user")}), hs::Val::nil());
    bool threw = false;
    try {
        hs::queue_enqueue(hs::Val::text("q_missing"), hs::Val::list({}), hs::Val::object({}));
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("unknown job") != std::string::npos;
    }
    CHECK(threw, "unknown jobs name themselves in the error");
    threw = false;
    try {
        hs::queue_enqueue(hs::Val::text("q_e1"), hs::Val::list({}), hs::Val::object({}));
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("1 argument") != std::string::npos;
    }
    CHECK(threw, "arity is checked with the count");
    threw = false;
    try {
        hs::queue_enqueue(hs::Val::text("q_e1"), hs::Val::list({hs::Val::int_(1)}),
                          hs::Val::object({{"delay", hs::Val::int_(-1)}}));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "negative delay refused");
    threw = false;
    try {
        hs::queue_enqueue(hs::Val::text("q_e1"), hs::Val::list({hs::Val::int_(1)}),
                          hs::Val::object({{"bogus", hs::Val::int_(1)}}));
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("unknown option") != std::string::npos;
    }
    CHECK(threw, "unknown options name themselves too");
    threw = false;
    try {
        hs::queue_enqueue(hs::Val::text("q_e1"), hs::Val::int_(1), hs::Val::object({}));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "args must be a list");
    int64_t id = hs::queue_enqueue(hs::Val::text("q_e1"), hs::Val::list({hs::Val::int_(9)}),
                                   hs::Val::object({{"priority", hs::Val::int_(3)}}))
                       .iv;
    CHECK(id > 0, "a good enqueue returns an id");
    CHECK_EQ(hs::queue_depth(hs::Val::text("q_e1")).iv, int64_t(1), "and depth sees it");
}

void enqueue_options_land_on_the_job() {
    ClockGuard clock;
    hs::queue_runtime().use_backend(new hs::MemoryBackend());
    hs::queue_declare_job(hs::Val::text("q_o1"), hs::Val::list({hs::Val::text("x")}), hs::Val::nil());
    (void)hs::queue_enqueue(hs::Val::text("q_o1"), hs::Val::list({hs::Val::int_(1)}),
                            hs::Val::object({{"delay", hs::Val::int_(60)},
                                             {"priority", hs::Val::int_(7)},
                                             {"max_attempts", hs::Val::int_(2)}}));
    hs::JobBackend* b = hs::queue_runtime().backend();
    hs::Job out;
    CHECK(!b->poll({"q_o1"}, 1'000'000, out), "delayed a minute out");
    CHECK(b->poll({"q_o1"}, 1'060'001, out), "ready after");
    CHECK_EQ(out.priority, 7, "priority carried");
    CHECK_EQ(out.max_attempts, 2, "attempt budget overridden");
    CHECK_EQ(out.payload.find("x")->iv, int64_t(1), "payload named by declaration");
}

void worker_registration_needs_a_handler() {
    bool threw = false;
    try {
        hs::queue_register_worker(hs::Val::text("q_w0"), hs::Val::int_(1), nullptr);
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "null handler refused");
    threw = false;
    try {
        hs::queue_register_worker(hs::Val::text("q_w0"), hs::Val::int_(0),
                                  [](const hs::Val&) { return hs::Val::nil(); });
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "zero concurrency refused");
}

// Wait up to `ms` for `cond`, polling every 10ms. Async assertions need a
// deadline, not a sleep: fast when healthy, bounded when broken.
template <typename F>
bool wait_for(int64_t ms, F cond) {
    auto t0 = std::chrono::steady_clock::now();
    while (std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now() - t0)
               .count() < ms) {
        if (cond()) return true;
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    return cond();
}

void workers_run_jobs_to_completion() {
    ClockGuard clock;
    hs::queue_runtime().use_backend(new hs::MemoryBackend());
    hs::queue_declare_job(hs::Val::text("q_run"), hs::Val::list({hs::Val::text("n")}), hs::Val::nil());
    std::atomic<int> ran{0};
    hs::queue_register_worker(
        hs::Val::text("q_run"), hs::Val::int_(2),
        [&](const hs::Val& payload) -> hs::Val {
            CHECK(payload.find("n") != nullptr, "payload carries declared params");
            ran.fetch_add(1);
            return hs::Val::nil();
        });
    for (int i = 0; i < 5; i++) {
        (void)hs::queue_enqueue(hs::Val::text("q_run"), hs::Val::list({hs::Val::int_(i)}),
                                hs::Val::object({}));
    }
    CHECK(wait_for(5000, [&] { return ran.load() == 5; }), "all five ran");
    CHECK_EQ(hs::queue_depth(hs::Val::text("q_run")).iv, int64_t(0), "nothing waiting");
}

void failing_jobs_retry_then_bury() {
    ClockGuard clock;
    hs::queue_runtime().use_backend(new hs::MemoryBackend());
    hs::queue_declare_job(hs::Val::text("q_fail"), hs::Val::list({hs::Val::text("n")}),
                          hs::Val::int_(2));
    std::atomic<int> tries{0};
    hs::queue_register_worker(
        hs::Val::text("q_fail"), hs::Val::int_(1),
        [&](const hs::Val&) -> hs::Val {
            tries.fetch_add(1);
            throw std::runtime_error("always");
            return hs::Val::nil();
        });
    (void)hs::queue_enqueue(hs::Val::text("q_fail"), hs::Val::list({hs::Val::int_(1)}),
                            hs::Val::object({}));
    CHECK(wait_for(5000, [&] { return tries.load() >= 1; }), "first try ran");
    ClockGuard::at(1'000'000 + 2000);  // past the 1s backoff
    CHECK(wait_for(5000, [&] { return hs::queue_dlq_list(hs::Val::text("q_fail")).arr.size() == 1; }),
          "buried after max attempts");
    CHECK_EQ(tries.load(), 2, "exactly two tries for max_attempts 2");
    hs::Val buried = hs::queue_dlq_list(hs::Val::text("q_fail")).arr[0];
    CHECK_EQ(buried.find("attempts")->iv, int64_t(2), "attempts recorded");
    CHECK(buried.find("error")->sv.find("always") != std::string::npos, "error recorded");
}

void workers_run_in_parallel() {
    ClockGuard clock;
    hs::queue_runtime().use_backend(new hs::MemoryBackend());
    hs::queue_declare_job(hs::Val::text("q_par"), hs::Val::list({hs::Val::text("n")}), hs::Val::nil());
    std::atomic<int> inflight{0};
    std::atomic<int> peak{0};
    hs::queue_register_worker(
        hs::Val::text("q_par"), hs::Val::int_(4),
        [&](const hs::Val&) -> hs::Val {
            int cur = inflight.fetch_add(1) + 1;
            int prev = peak.load();
            while (prev < cur && !peak.compare_exchange_weak(prev, cur)) {
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(30));
            inflight.fetch_sub(1);
            return hs::Val::nil();
        });
    uint64_t done_before = hs::queue_runtime().completed();
    for (int i = 0; i < 8; i++) {
        (void)hs::queue_enqueue(hs::Val::text("q_par"), hs::Val::list({hs::Val::int_(i)}),
                                hs::Val::object({}));
    }
    // Completions, not depth: a claimed job is not waiting, but its handler
    // may still run. Returning while it does would free the counters under
    // it — the use-after-return ASan caught.
    CHECK(wait_for(10000, [&] { return hs::queue_runtime().completed() == done_before + 8; }),
          "all eight finished");
    CHECK(peak.load() > 1, "more than one in flight at once");
}

void depth_and_dlq_surface_state() {
    ClockGuard clock;
    hs::queue_runtime().use_backend(new hs::MemoryBackend());
    hs::queue_declare_job(hs::Val::text("q_st"), hs::Val::list({hs::Val::text("n")}), hs::Val::nil());
    CHECK_EQ(hs::queue_depth(hs::Val::nil()).iv, int64_t(0), "empty everywhere");
    (void)hs::queue_enqueue(hs::Val::text("q_st"), hs::Val::list({hs::Val::int_(1)}),
                            hs::Val::object({}));
    CHECK_EQ(hs::queue_depth(hs::Val::nil()).iv, int64_t(1), "across types");
    CHECK_EQ(hs::queue_dlq_list(hs::Val::nil()).arr.size(), size_t(0), "nothing buried");
    bool threw = false;
    try {
        hs::queue_depth(hs::Val::int_(1));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "non-text filter refused");
}

// ---------------------------------------------------------------------------
// Throughput smoke: 50k jobs/s is the milestone floor. Prints its rate and
// fails only far below it; M6.9 measures properly.
// ---------------------------------------------------------------------------

void enqueue_poll_completes_at_rate() {
    hs::MemoryBackend b;
    const int N = 50000;
    auto t0 = std::chrono::steady_clock::now();
    for (int i = 0; i < N; i++) {
        hs::Job j = mkjob("bench");
        b.push(j);
    }
    hs::Job out;
    int n = 0;
    while (b.poll({"bench"}, 1'000'000, out)) {
        b.track(out);
        b.complete(out.id);
        n++;
    }
    auto dt = std::chrono::duration<double>(std::chrono::steady_clock::now() - t0).count();
    double per_sec = (2.0 * N) / dt;
    printf("  [info] %.0f queue ops/sec (enqueue+poll+complete)\n", per_sec);
    CHECK_EQ(n, N, "every job came back out");
    CHECK(per_sec > 50000.0, "above the 50k/s floor");
}

}  // namespace
}  // namespace qa

int main() {
    qa::waiting_jobs_come_out_oldest_ready_first();
    qa::priority_beats_age();
    qa::the_future_waits();
    qa::types_do_not_leak_into_each_other();
    qa::depth_counts_the_waiting();
    qa::complete_forgets_and_fail_remembers();
    qa::fail_with_a_time_requeues();
    qa::failing_unknown_ids_is_quiet();
    qa::clear_empties_everything();
    qa::backoff_doubles_to_a_minute();

    qa::sql_push_poll_complete_round_trip();
    qa::sql_orders_by_time_priority_id();
    qa::sql_fail_requeues_and_buries();
    qa::sql_claims_recover_on_open();
    qa::sql_clear_and_depth();
    qa::sql_bad_payload_decodes_empty();

    qa::declare_registers_arity_and_budget();
    qa::enqueue_validates_everything();
    qa::enqueue_options_land_on_the_job();
    qa::worker_registration_needs_a_handler();
    qa::workers_run_jobs_to_completion();
    qa::failing_jobs_retry_then_bury();
    qa::workers_run_in_parallel();
    qa::depth_and_dlq_surface_state();

    qa::enqueue_poll_completes_at_rate();

    return qa::report("queue");
}
