// rt_web.cpp -- the rest of the runtime, benchmark lanes (M6.9)
//
// The roll-up report needs the numbers the cache, queue and scheduler reports
// do not carry: a signed session round trip, a rate-limit check, a metrics
// scrape, a lock and an idempotency claim, and one key placement. Same shape
// as the other bench programs: `RESULT` lines and RSS/HEAP.
//
// Prints into reports/runtime-performance-v0.7.md via qa/bench_runtime.sh.

#include "hs_runtime_cluster.hpp"
#include "hs_runtime_metrics.hpp"
#include "hs_runtime_ratelimit.hpp"
#include "hs_runtime_session.hpp"

#include "bench.hpp"

#include <atomic>
#include <thread>

namespace {

/// What `hs_respond` does for every route return: cookies queued by the
/// handler are written and the per-request state reset.
void drain(hs::Response& res) {
    if (hs::hs_respond_drain_hook) hs::hs_respond_drain_hook(res);
}

void session_roundtrip() {
    hs::session_set_secret_for_tests("bench-secret-0123456789abcdef");
    const int kIters = 100000;
    bench::Timer t("session_start_drain", "memory");
    for (int i = 0; i < kIters; i++) {
        hs::Request req;
        hs::Val user = hs::Val::object({{"id", hs::Val::int_(i % 1000)}});
        hs::session_start(req, user);
        hs::Response res;
        drain(res);
    }
    t.add(kIters);
    t.done();
    hs::session_clear_secret_for_tests();
}

void session_verify() {
    hs::session_set_secret_for_tests("bench-secret-0123456789abcdef");
    hs::Request req;
    hs::session_start(req, hs::Val::object({{"id", hs::Val::int_(1)}}));
    hs::Response res;
    drain(res);
    std::string cookie;
    for (const auto& h : res.headers)
        if (h.first == "Set-Cookie" && h.second.rfind("hs_session=", 0) == 0) cookie = h.second;
    if (cookie.empty()) {
        fprintf(stderr, "bench: no session cookie was written\n");
        exit(1);
    }
    std::string value = cookie.substr(cookie.find('=') + 1);
    size_t semi = value.find(';');
    if (semi != std::string::npos) value = value.substr(0, semi);
    const int kIters = 200000;
    bench::Timer t("session_verify", "memory");
    long long ok = 0;
    hs::Val payload;
    for (int i = 0; i < kIters; i++) {
        if (hs::session_verify(value, payload)) ok++;
    }
    t.add(ok);
    t.done();
    hs::session_clear_secret_for_tests();
    if (ok != kIters) {
        fprintf(stderr, "bench: %lld of %d cookies verified\\n", ok, kIters);
        exit(1);
    }
}

void ratelimit_check() {
    const int kIters = 1000000;
    bench::Timer t("limit_check", "memory");
    long long allowed = 0;
    for (int i = 0; i < kIters; i++) {
        // A budget no key can exhaust within the lane, so the lane measures the
        // check rather than the refusal path.
        if (hs::limit_check("bench", hs::LIMIT_TOKEN_BUCKET, 1000000000, 60000, "bench")
                .allowed)
            allowed++;
    }
    t.add(allowed);
    t.done();
    if (allowed != kIters) {
        fprintf(stderr, "bench: %lld of %d checks were allowed\\n", allowed, kIters);
        exit(1);
    }
}

void ratelimit_sliding() {
    const int kIters = 500000;
    bench::Timer t("limit_check_sliding", "memory");
    long long n = 0;
    for (int i = 0; i < kIters; i++) {
        if (hs::limit_check("bench-sliding", hs::LIMIT_SLIDING_WINDOW, 1000000000, 60000, "bench")
                .allowed)
            n++;
    }
    t.add(n);
    t.done();
}

void ratelimit_under_threads(int threads) {
    const int kEach = 200000;
    std::string backend = "memory-" + std::to_string(threads) + "t";
    bench::Timer t("limit_check", backend.c_str());
    std::vector<std::thread> ts;
    std::atomic<long long> done{0};
    for (int th = 0; th < threads; th++) {
        ts.emplace_back([&done, th] {
            long long n = 0;
            for (int i = 0; i < kEach; i++) {
                if (hs::limit_check("bench-mt", hs::LIMIT_TOKEN_BUCKET, 1000000000, 60000,
                                    "key-" + std::to_string((i + th) % 64))
                        .allowed)
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

void metrics_record() {
    const int kIters = 1000000;
    bench::Timer t("metrics_incr", "memory");
    for (int i = 0; i < kIters; i++) {
        hs::metrics_registry().incr("bench_counter", "");
    }
    t.add(kIters);
    t.done();
}

void metrics_scrape() {
    // Record a realistic number of series first: a scrape walks them all.
    for (int i = 0; i < 200; i++) {
        hs::metrics_registry().incr("bench_series_" + std::to_string(i), "{path=\"/x\"}");
    }
    const int kIters = 2000;
    bench::Timer t("metrics_scrape", "memory");
    long long bytes = 0;
    for (int i = 0; i < kIters; i++) bytes += (long long)hs::metrics_prometheus_text().size();
    t.add(kIters);
    t.done();
    printf("SCRAPE memory %lld\n", bytes / kIters);
}

void lock_cycle() {
    hs::MemoryCoordStore store;
    const int kIters = 200000;
    bench::Timer t("lock_acquire_release", "memory");
    long long n = 0;
    for (int i = 0; i < kIters; i++) {
        if (store.acquire("l", "n", "t", 1000, 1000)) n++;
        store.release("l", "t");
    }
    t.add(n);
    t.done();
    if (n != kIters) {
        fprintf(stderr, "bench: %lld of %d acquisitions succeeded\\n", n, kIters);
        exit(1);
    }
}

void idem_cycle() {
    hs::MemoryCoordStore store;
    const int kIters = 200000;
    bench::Timer t("idem_claim", "memory");
    long long n = 0;
    for (int i = 0; i < kIters; i++) {
        if (store.claim("k" + std::to_string(i), "n", "t", 60000, 1000)) n++;
    }
    t.add(n);
    t.done();
    if (n != kIters) {
        fprintf(stderr, "bench: %lld of %d claims won\\n", n, kIters);
        exit(1);
    }
}

void placement() {
    // A five-node ring, which is the size a small deployment actually is.
    std::vector<std::string> ring = {"n1", "n2", "n3", "n4", "n5"};
    const int kIters = 500000;
    bench::Timer t("cluster_owner", "memory");
    long long n = 0;
    for (int i = 0; i < kIters; i++) {
        if (!hs::cluster_owner_for(ring, "user:" + std::to_string(i)).empty()) n++;
    }
    t.add(n);
    t.done();
}

}  // namespace

int main() {
    int threads = (int)std::thread::hardware_concurrency();
    if (threads <= 0) threads = 4;
    session_roundtrip();
    session_verify();
    ratelimit_check();
    ratelimit_sliding();
    ratelimit_under_threads(threads);
    metrics_record();
    metrics_scrape();
    lock_cycle();
    idem_cycle();
    placement();
    bench::metrics("memory");
    return 0;
}
