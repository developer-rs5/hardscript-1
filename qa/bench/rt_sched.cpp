// rt_sched.cpp -- scheduler engine benchmark lanes (M6.9)
//
// The scheduler spends almost all of its time deciding *when* a job is next
// due, so that is what is measured: interval arithmetic, daily and weekly
// civil-time arithmetic with a zone offset, and the cost of registering a
// timer. Nothing here waits for a clock to elapse -- the point is the cost of
// the decision, not the interval.
//
// Prints `RESULT <workload> <backend> <ops> <seconds>`;
// qa/bench_runtime.sh turns those into reports/scheduler-performance.md.

#include "hs_runtime_schedule.hpp"

#include "bench.hpp"

#include <atomic>
#include <thread>

namespace {

// A fixed "now" in UTC so a daily lane is reproducible: 2026-01-15 12:00:00.
const int64_t kNow = 1768478400000LL;

hs::Val noop() { return hs::Val::int_(1); }

/// A fresh runtime per lane: the scheduler owns a thread, and a bench that
/// leaves eight of them running measures its own cleanup.
struct Bench {
    hs::SchedRuntime sched;
    ~Bench() {}
};

/// The sum of what a lane computed, printed so the work cannot be optimized
/// away: an inlined `after + secs*1000` folds to a constant, and a benchmark
/// of a deleted loop is worse than no benchmark.
long long g_sink = 0;

/// A walk of plausible `after` times. Not `kNow + i`: a linear walk is an
/// induction variable the compiler closes in a multiply, and the lane then
/// measures nothing. The multiply is a few cycles against ~40 for the civil
/// date math, so it stays under a tenth of the lane.
struct ClockWalk {
    int64_t t;
    ClockWalk() : t(kNow) {}
    int64_t next() {
        t = t * 6364136223846793005LL + 1442695040888963407LL;
        return (t >> 20) + kNow;
    }
};

void next_interval() {
    bench::Timer t("next_interval", "memory");
    const int kIters = 1000000;
    long long sum = 0;
    ClockWalk walk;
    for (int i = 0; i < kIters; i++) sum += hs::sched_next_interval_ms(walk.next(), 3600);
    t.add(kIters);
    t.done();
    g_sink += sum;
}

void next_daily() {
    bench::Timer t("next_daily_utc", "memory");
    const int kIters = 500000;
    long long sum = 0;
    ClockWalk walk;
    for (int i = 0; i < kIters; i++) sum += hs::sched_next_daily_ms(walk.next(), 3, 30, 0, 0);
    t.add(kIters);
    t.done();
    g_sink += sum;
}

void next_daily_offset() {
    // The half-hour zone, which is where naive local-time math goes wrong.
    bench::Timer t("next_daily_tz+0530", "memory");
    const int kIters = 500000;
    long long sum = 0;
    ClockWalk walk;
    for (int i = 0; i < kIters; i++) sum += hs::sched_next_daily_ms(walk.next(), 3, 30, 0, 330);
    t.add(kIters);
    t.done();
    g_sink += sum;
}

void next_weekly() {
    bench::Timer t("next_weekly", "memory");
    const int kIters = 500000;
    long long sum = 0;
    ClockWalk walk;
    for (int i = 0; i < kIters; i++) sum += hs::sched_next_weekly_ms(walk.next(), 4, 9, 0, 0, 0);
    t.add(kIters);
    t.done();
    g_sink += sum;
}

void prev_daily() {
    // Catch-up uses the previous-fire maths, so it is on the same path a
    // missed job takes.
    bench::Timer t("prev_daily", "memory");
    const int kIters = 500000;
    long long sum = 0;
    ClockWalk walk;
    for (int i = 0; i < kIters; i++) sum += hs::sched_prev_daily_ms(walk.next(), 3, 30, 0, 0);
    t.add(kIters);
    t.done();
    g_sink += sum;
}

void next_across_dst() {
    // March and November in a zone with DST: the two weeks where a fixed
    // offset would be an hour out. `sched_next_daily_ms` takes minutes, so the
    // lane is the two offsets either side of a transition.
    bench::Timer t("next_daily_dst_pair", "memory");
    const int kIters = 500000;
    long long sum = 0;
    ClockWalk walk;
    for (int i = 0; i < kIters; i++) {
        int64_t after = walk.next();
        sum += hs::sched_next_daily_ms(after, 2, 30, 0, 300);  // UTC-5
        sum += hs::sched_next_daily_ms(after, 2, 30, 0, 240);  // UTC-4
    }
    t.add(kIters);
    t.done();
    g_sink += sum;
}

void register_interval() {
    // Registering a timer is what a program does once at startup; the cost is
    // the map insert and the hand-off to the timer heap, not the arithmetic.
    Bench b;
    bench::Timer t("register", "memory");
    const int kIters = 20000;
    for (int i = 0; i < kIters; i++) {
        hs::Schedule s;
        s.name = "job-" + std::to_string(i);
        s.kind = hs::SchedKind::Interval;
        s.every_secs = 3600;
        b.sched.register_schedule(std::move(s), noop);
    }
    t.add(kIters);
    t.done();
}

void register_from_threads() {
    Bench b;
    const int kThreads = 4;
    const int kEach = 2000;
    std::string backend = "register-" + std::to_string(kThreads) + "t";
    bench::Timer t("register", backend.c_str());
    std::vector<std::thread> ts;
    std::atomic<long long> done{0};
    for (int th = 0; th < kThreads; th++) {
        ts.emplace_back([&b, &done, th] {
            long long n = 0;
            for (int i = 0; i < kEach; i++) {
                hs::Schedule s;
                s.name = "t" + std::to_string(th) + "-" + std::to_string(i);
                s.kind = hs::SchedKind::Interval;
                s.every_secs = 60;
                b.sched.register_schedule(std::move(s), noop);
                n++;
            }
            done.fetch_add(n);
        });
    }
    for (auto& x : ts)
        x.join();
    t.add(done.load());
    t.done();
}

/// End to end through the scheduler thread: register a timer that is already
/// due, and count the firings. This is the only lane that waits, and it waits
/// on 100ms sleep slices by design.
void fire_through_thread() {
    Bench b;
    std::atomic<int> fired{0};
    const int kIters = 200;
    hs::Schedule s;
    s.name = "tick";
    s.kind = hs::SchedKind::Interval;
    s.every_secs = 0;  // rejected: intervals must be positive
    bool rejected = false;
    try {
        b.sched.register_schedule(s, [&fired] {
            fired.fetch_add(1);
            return hs::Val::int_(1);
        });
    } catch (const std::exception&) {
        rejected = true;
    }
    if (!rejected) {
        fprintf(stderr, "bench: a zero interval was accepted\n");
        exit(1);
    }
    s.every_secs = 1;
    b.sched.register_schedule(s, [&fired] {
        fired.fetch_add(1);
        return hs::Val::int_(1);
    });
    // One firing a second, so this lane is about the scheduler thread noticing
    // a due timer and calling the handler, not about throughput.
    double start = bench::now();
    while (fired.load() < 2 && bench::now() - start < 10.0) {
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
    double elapsed = bench::now() - start;
    if (fired.load() < 2) {
        fprintf(stderr, "bench: only %d firings in %.1fs\n", fired.load(), elapsed);
        exit(1);
    }
    bench::Timer t("fire_latency", "memory");
    t.add(fired.load());
    t.start = bench::now() - elapsed;
    t.done();
}

}  // namespace

int main() {
    next_interval();
    next_daily();
    next_daily_offset();
    next_weekly();
    prev_daily();
    next_across_dst();
    register_interval();
    register_from_threads();
    fire_through_thread();
    bench::metrics("memory");
    // Printed so the sums above are observably used, and so a lane that folded
    // itself away shows up as a zero here rather than as a fast number.
    printf("SINK %lld\n", g_sink);
    return 0;
}
