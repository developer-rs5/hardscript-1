// rt_queue.cpp -- queue and background job benchmark lanes (M6.9)
//
// A queue is judged on the round trip a program actually pays: enqueue, poll,
// complete, plus what happens when the work is durable (SQLite) and when it
// keeps failing (retries into the dead-letter queue).
//
// Prints `RESULT <workload> <backend> <ops> <seconds>` and RSS/HEAP lines;
// qa/bench_runtime.sh turns those into reports/queue-performance.md.

#include "hs_runtime_queue.hpp"
#include "hs_runtime_sqlite.hpp"

#include "bench.hpp"

#include <atomic>
#include <thread>

namespace {

// A clock the queue reads, so a delayed job is ready when the lane says so
// rather than when the wall clock happens to get there.
struct FrozenClock {
    FrozenClock() { hs::queue_set_now_for_tests(1'000'000); }
    ~FrozenClock() { hs::queue_clear_now_for_tests(); }
    static void at(int64_t ms) { hs::queue_set_now_for_tests(ms); }
};

/// The queue's clock, which a frozen test clock may be overriding.
int64_t clock_taken() { return hs::queue_now_ms(); }

hs::Job job(const std::string& type = "mail", int priority = 0) {
    hs::Job j;
    j.type = type;
    j.priority = priority;
    j.payload = hs::Val::object({{"to", hs::Val::text("ada@example.com")},
                                 {"subject", hs::Val::text("hello")},
                                 {"body", hs::Val::text("a message")},
                                 {"n", hs::Val::int_(7)}});
    j.max_attempts = 5;
    return j;
}

/// Enqueue, poll and complete, in one loop: the mixed number the milestone
/// quoted, and the one a request handler pays.
void roundtrip_memory() {
    FrozenClock clock;
    hs::MemoryBackend b;
    const int kIters = 200000;
    bench::Timer t("enqueue_poll_complete", "memory");
    for (int i = 0; i < kIters; i++) {
        int64_t id = b.push(job());
        hs::Job out;
        if (!b.poll({"mail"}, clock_taken(), out)) {
            fprintf(stderr, "bench: nothing to poll at %d\n", i);
            exit(1);
        }
        b.complete(id);
    }
    t.add(kIters);
    t.done();
}

void enqueue_only() {
    FrozenClock clock;
    hs::MemoryBackend b;
    const int kIters = 500000;
    bench::Timer t("enqueue", "memory");
    for (int i = 0; i < kIters; i++) b.push(job());
    t.add(kIters);
    t.done();
    // Drain so the process does not exit holding half a million jobs.
    hs::Job out;
    while (b.poll({"mail"}, clock_taken(), out)) b.complete(out.id);
}

void enqueue_with_payload() {
    // A bigger payload, which is what a real job carries: a queue that is fast
    // with an empty body is not fast.
    FrozenClock clock;
    hs::MemoryBackend b;
    const int kIters = 200000;
    bench::Timer t("enqueue_payload_1kb", "memory");
    std::string filler(1024, 'x');
    for (int i = 0; i < kIters; i++) {
        hs::Job j = job();
        j.payload.set("blob", hs::Val::text(filler));
        b.push(std::move(j));
    }
    t.add(kIters);
    t.done();
    hs::Job out;
    while (b.poll({"mail"}, clock_taken(), out)) b.complete(out.id);
}

void poll_priority() {
    FrozenClock clock;
    hs::MemoryBackend b;
    const int kIters = 200000;
    bench::Timer t("poll_priority", "memory");
    long long high = 0;
    for (int i = 0; i < kIters; i++) {
        b.push(job("mail", i % 2 ? 0 : 10));
        hs::Job out;
        if (b.poll({"mail"}, clock_taken(), out) && out.priority == 10) high++;
    }
    t.add(kIters);
    t.done();
    if (high != kIters / 2) {
        fprintf(stderr, "bench: priority poll took %lld of %d\n", high, kIters / 2);
        exit(1);
    }
    hs::Job out;
    while (b.poll({"mail"}, clock_taken(), out)) b.complete(out.id);
}

void dlq_and_retries() {
    FrozenClock clock;
    hs::MemoryBackend b;
    const int kJobs = 20000;
    bench::Timer t("fail_to_dlq", "memory");
    long long buried = 0;
    for (int i = 0; i < kJobs; i++) {
        int64_t id = b.push(job());
        for (int attempt = 1; attempt <= 5; attempt++) {
            // Five attempts, then the dead-letter queue: the fifth failure is
            // the one with nowhere left to retry, so there is nothing to poll
            // after it.
            bool last = attempt == 5;
            int64_t retry_at = last ? 0 : clock_taken() + hs::queue_backoff_ms(attempt);
            b.fail(id, attempt, "boom", retry_at);
            if (last) break;
            FrozenClock::at(retry_at);
            hs::Job out;
            if (!b.poll({"mail"}, clock_taken(), out)) {
                fprintf(stderr, "bench: retry %d never became ready\n", attempt);
                exit(1);
            }
            // The memory backend separates claiming from remembering, so a
            // caller that polls directly has to track what it claimed -- the
            // worker's job, and the only way `fail` can find the job to
            // requeue or bury.
            b.track(out);
            id = out.id;
        }
    }
    t.add(kJobs);
    t.done();
    // Asked once at the end: `dlq()` returns a copy, so checking it per job
    // would make this lane quadratic in the DLQ and time the copy instead of
    // the retries.
    buried = (long long)b.dlq("mail").size();
    if (buried != (long long)kJobs) {
        fprintf(stderr, "bench: only %lld of %d jobs were buried\n", buried, kJobs);
        exit(1);
    }
    bench::metrics("memory");
}

/// The same round trip against SQLite, which is what a program pays when the
/// queue has to survive a restart.
void roundtrip_sqlite() {
    FrozenClock clock;
    std::unique_ptr<hs::SqliteDb> db(new hs::SqliteDb(":memory:"));
    hs::SqlJobBackend b(db.get());
    const int kIters = 20000;
    bench::Timer t("enqueue_poll_complete", "sqlite");
    for (int i = 0; i < kIters; i++) {
        int64_t id = b.push(job());
        hs::Job out;
        if (!b.poll({"mail"}, clock_taken(), out)) {
            fprintf(stderr, "bench: sqlite had nothing at %d\n", i);
            exit(1);
        }
        b.complete(id);
    }
    t.add(kIters);
    t.done();
    printf("RSS sqlite %lld\n", bench::peak_rss_kb());
    printf("HEAP sqlite %lld\n", bench::heap_held_bytes());
}

void sqlite_durable_restart() {
    // The property memory cannot have: a claimed job that was never completed
    // comes back when the process reopens the database.
    std::string path = "/tmp/opencode/bench-queue-restart.db";
    ::remove(path.c_str());
    FrozenClock clock;
    {
        std::unique_ptr<hs::SqliteDb> db(new hs::SqliteDb(path));
        hs::SqlJobBackend b(db.get());
        b.push(job());
        hs::Job out;
        if (!b.poll({"mail"}, clock_taken(), out)) {
            fprintf(stderr, "bench: nothing to claim\n");
            exit(1);
        }
    }
    bench::Timer t("recover_after_restart", "sqlite");
    {
        std::unique_ptr<hs::SqliteDb> db(new hs::SqliteDb(path));
        hs::SqlJobBackend b(db.get());
        hs::Job out;
        bool back = b.poll({"mail"}, clock_taken(), out);
        t.add(back ? 1 : 0);
        if (back) b.complete(out.id);
    }
    t.done();
    ::remove(path.c_str());
}

void queue_depth() {
    FrozenClock clock;
    hs::MemoryBackend b;
    const int kIters = 100000;
    bench::Timer t("depth", "memory");
    long long n = 0;
    for (int i = 0; i < kIters; i++) n += b.depth("mail");
    t.add(kIters);
    t.done();
    if (n != 0) {
        fprintf(stderr, "bench: depth reported work for an empty queue\n");
        exit(1);
    }
}

/// A worker draining a queue with real threads: the concurrency a program
/// actually configures.
void worker_throughput(int threads) {
    FrozenClock clock;
    hs::QueueRuntime& q = hs::queue_runtime();
    q.declare_job("bench_job", {"n"}, 3);
    std::atomic<long long> done{0};
    q.register_worker("bench_job", threads, [&done](const hs::Val& payload) {
        if (payload.find("n")) done.fetch_add(1);
        return hs::Val::boolean(true);
    });
    const int kJobs = 20000;
    std::string backend = "memory-" + std::to_string(threads) + "t";
    bench::Timer t("worker_drain", backend.c_str());
    for (int i = 0; i < kJobs; i++) {
        q.enqueue("bench_job", hs::Val::list({hs::Val::int_(i)}), 0, 0, 0);
    }
    double start = bench::now();
    while (done.load() < kJobs && bench::now() - start < 60.0) {
        std::this_thread::sleep_for(std::chrono::milliseconds(1));
    }
    double elapsed = bench::now() - start;
    t.add(done.load());
    t.start = bench::now() - elapsed;
    t.done();
    if (done.load() < kJobs) {
        fprintf(stderr, "bench: only %lld of %d jobs ran\n", done.load(), kJobs);
        exit(1);
    }
}

}  // namespace

int main() {
    int threads = (int)std::thread::hardware_concurrency();
    if (threads <= 0) threads = 4;
    roundtrip_memory();
    enqueue_only();
    enqueue_with_payload();
    poll_priority();
    dlq_and_retries();
    queue_depth();
    sqlite_durable_restart();
    roundtrip_sqlite();
    worker_throughput(threads);
    return 0;
}
