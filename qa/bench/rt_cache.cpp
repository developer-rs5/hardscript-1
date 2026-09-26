// rt_cache.cpp -- cache engine benchmark lanes (M6.9)
//
// Four things a cache is actually judged on: a hit, a miss, a write, and a
// mixed workload under concurrency. Memory is reported too, because a cache
// that is fast because it is small is not a cache.
//
// Prints `RESULT <workload> <backend> <ops> <seconds>` and RSS/HEAP lines;
// qa/bench_runtime.sh turns those into reports/cache-performance.md.

#include "hs_runtime_cache.hpp"

#include "bench.hpp"

#include <atomic>
#include <thread>

namespace {

// Enough keys to leave the hot path and the sharded path both honest.
const int kKeys = 10000;

void fill(hs::Cache& c) {
    for (int i = 0; i < kKeys; i++) {
        c.set("k" + std::to_string(i), hs::Val::object({{"n", hs::Val::int_(i)},
                                                         {"name", hs::Val::text("value")}}));
    }
}

void get_hit() {
    hs::Cache& c = hs::cache_default();
    fill(c);
    const int kIters = 200000;
    bench::Timer t("get_hit", "memory");
    long long hits = 0;
    for (int i = 0; i < kIters; i++) {
        // Every key, so the working set is real rather than one line.
        hs::Val v = c.get("k" + std::to_string(i % kKeys));
        if (!v.is_nil()) hits++;
    }
    t.add(hits);
    t.done();
    if (hits != kIters) {
        fprintf(stderr, "bench: get_hit saw %lld of %d\n", hits, kIters);
        exit(1);
    }
}

void get_miss() {
    hs::Cache& c = hs::cache_default();
    const int kIters = 200000;
    bench::Timer t("get_miss", "memory");
    long long misses = 0;
    for (int i = 0; i < kIters; i++) {
        if (c.get("absent-" + std::to_string(i)).is_nil()) misses++;
    }
    t.add(misses);
    t.done();
    if (misses != kIters) {
        fprintf(stderr, "bench: get_miss saw %lld of %d\n", misses, kIters);
        exit(1);
    }
}

void set_ops() {
    hs::Cache& c = hs::cache_default();
    const int kIters = 200000;
    bench::Timer t("set", "memory");
    for (int i = 0; i < kIters; i++) {
        c.set("s" + std::to_string(i % 1000), hs::Val::int_(i), 60);
    }
    t.add(kIters);
    t.done();
}

void incr_ops() {
    hs::Cache& c = hs::cache_default();
    c.set("counter", hs::Val::int_(0), 0);
    const int kIters = 200000;
    bench::Timer t("incr", "memory");
    for (int i = 0; i < kIters; i++) c.incr("counter");
    t.add(kIters);
    t.done();
    if (c.get("counter").as_int() != kIters) {
        fprintf(stderr, "bench: incr lost counts\n");
        exit(1);
    }
}

void exists_ops() {
    hs::Cache& c = hs::cache_default();
    fill(c);
    const int kIters = 200000;
    bench::Timer t("exists", "memory");
    long long found = 0;
    for (int i = 0; i < kIters; i++) {
        if (c.exists("k" + std::to_string(i % kKeys))) found++;
    }
    t.add(found);
    t.done();
}

void ttl_expiry() {
    // The cost of the lazy path: a read that finds an expired entry drops it.
    hs::Cache c;
    for (int i = 0; i < 1000; i++) c.set("e" + std::to_string(i), hs::Val::int_(1), 0);
    const int kIters = 200000;
    bench::Timer t("expired_read", "memory");
    long long n = 0;
    for (int i = 0; i < kIters; i++) {
        if (c.get("e" + std::to_string(i % 1000)).is_nil()) n++;
    }
    t.add(n);
    t.done();
}

/// The mixed workload the milestone quoted: 60% hits, 20% misses, 15% writes,
/// 5% increments, on one thread.
void mixed_single() {
    hs::Cache& c = hs::cache_default();
    fill(c);
    const int kIters = 1000000;
    bench::Timer t("mixed", "memory-1t");
    for (int i = 0; i < kIters; i++) {
        int r = i % 20;
        if (r < 12) {
            c.get("k" + std::to_string(i % kKeys));
        } else if (r < 16) {
            c.get("miss-" + std::to_string(i));
        } else if (r < 19) {
            c.set("w" + std::to_string(i % 1000), hs::Val::int_(i), 60);
        } else {
            c.incr("counter");
        }
    }
    t.add(kIters);
    t.done();
}

/// The same mix across every core, which is the number that decides whether
/// the sharding is worth its memory.
void mixed_threads(int threads) {
    hs::Cache& c = hs::cache_default();
    fill(c);
    const int kIters = 400000;
    std::string backend = "memory-" + std::to_string(threads) + "t";
    bench::Timer t("mixed", backend.c_str());
    std::vector<std::thread> ts;
    std::atomic<long long> done{0};
    for (int th = 0; th < threads; th++) {
        ts.emplace_back([&, th] {
            long long local = 0;
            for (int i = 0; i < kIters; i++) {
                int r = (i + th) % 20;
                if (r < 12) {
                    c.get("k" + std::to_string((i * 7 + th) % kKeys));
                } else if (r < 16) {
                    c.get("tmiss-" + std::to_string(th) + "-" + std::to_string(i));
                } else if (r < 19) {
                    c.set("w" + std::to_string((i + th) % 1000), hs::Val::int_(i), 60);
                } else {
                    c.incr("counter");
                }
                local++;
            }
            done.fetch_add(local);
        });
    }
    for (auto& x : ts)
        x.join();
    t.add(done.load());
    t.done();
}

/// Memory for a large working set, which is the number an operator sizes a
/// box with.
void memory_at_scale() {
    hs::Cache c;
    const int kBig = 200000;
    bench::Timer t("insert_200k", "memory");
    for (int i = 0; i < kBig; i++) {
        c.set("big:" + std::to_string(i),
              hs::Val::object({{"n", hs::Val::int_(i)}, {"name", hs::Val::text("value")}}), 3600);
    }
    t.add(kBig);
    t.done();
    // Measured while the entries are still live: the heap after the cache goes
    // out of scope says nothing about what a filled cache costs.
    printf("RSS live %lld\n", bench::peak_rss_kb());
    printf("HEAP live %lld\n", bench::heap_held_bytes());
    printf("RESULT live_keys memory %lld %.6f\n", (long long)c.live_entries(), 1.0);
    fflush(stdout);
}

}  // namespace

int main() {
    int threads = (int)std::thread::hardware_concurrency();
    if (threads <= 0) threads = 4;
    get_hit();
    get_miss();
    set_ops();
    incr_ops();
    exists_ops();
    ttl_expiry();
    mixed_single();
    mixed_threads(threads);
    memory_at_scale();
    bench::metrics("memory");
    return 0;
}
