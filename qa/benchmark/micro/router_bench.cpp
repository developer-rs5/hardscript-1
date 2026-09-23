// Router dispatch micro-benchmark (ms1.6).
//
// Registers K routes (mixed static / :param / *wildcard, 3 per group) and
// measures `Server::match` lookup latency for static hits, param hits,
// wildcard hits and 404 misses, plus steady-state heap allocations and idle
// RSS growth of the route table. The pre-ms1.6 linear matcher is reproduced
// side-by-side so the report can state a before/after delta in one binary.
//
// Build:  g++ -std=c++17 -O3 -flto -march=native -pthread router_bench.cpp -o router_bench
// Run:    ./router_bench
#include "../../../runtime/hs_runtime.hpp"
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <new>

using namespace hs;

// ---------------------------------------------------------------------------
// allocation counter (exactly one TU overrides global new)
// ---------------------------------------------------------------------------
static thread_local bool g_count = false;
static thread_local long long g_allocs = 0;
void* operator new(size_t n) {
    if (g_count) g_allocs++;
    void* p = std::malloc(n ? n : 1);
    if (!p) throw std::bad_alloc();
    return p;
}
void* operator new[](size_t n) {
    if (g_count) g_allocs++;
    void* p = std::malloc(n ? n : 1);
    if (!p) throw std::bad_alloc();
    return p;
}
void operator delete(void* p) noexcept { std::free(p); }
void operator delete[](void* p) noexcept { std::free(p); }
void operator delete(void* p, size_t) noexcept { std::free(p); }
void operator delete[](void* p, size_t) noexcept { std::free(p); }

static long long vm_rss_kb() {
    FILE* f = fopen("/proc/self/status", "r");
    if (!f) return -1;
    long long kb = -1;
    char line[256];
    while (fgets(line, sizeof line, f)) {
        if (strncmp(line, "VmRSS:", 6) == 0) { kb = atoll(line + 6); break; }
    }
    fclose(f);
    return kb;
}

static Response noop(const Request&) { return Response(); }

// Pre-ms1.6 linear matcher, reproduced byte-for-byte semantics.
static int old_match(const std::vector<Route>& routes, const std::string& method,
                     std::string_view path, Request& req) {
    std::string_view got[32];
    size_t gn = split_sv(path, got, 32);
    if (gn == (size_t)-1) return -1;
    for (size_t i = 0; i < routes.size(); i++) {
        const Route& r = routes[i];
        if (r.method != method) continue;
        std::string_view want[32];
        size_t wn = split_sv(r.path, want, 32);
        if (wn == (size_t)-1 || wn != gn) continue;
        bool ok = true;
        std::pair<std::string_view, std::string_view> pbuf[16];
        size_t pn = 0;
        for (size_t k = 0; k < wn; k++) {
            if (!want[k].empty() && want[k][0] == ':') {
                if (pn >= 16) { ok = false; break; }
                pbuf[pn] = { want[k].substr(1), got[k] };
                pn++;
            } else if (want[k] != got[k]) { ok = false; break; }
        }
        if (ok) {
            if (pn)
                for (size_t k = 0; k < pn; k++)
                    req.params[std::string(pbuf[k].first)] = std::string(pbuf[k].second);
            return (int)i;
        }
    }
    return -1;
}

template <class F>
static double ns_per_op(F&& f, size_t reps) {
    for (int i = 0; i < 500; i++) f(); // warm
    auto t0 = std::chrono::steady_clock::now();
    for (size_t i = 0; i < reps; i++) f();
    auto t1 = std::chrono::steady_clock::now();
    return (double)std::chrono::duration_cast<std::chrono::nanoseconds>(t1 - t0).count() / (double)reps;
}

int main() {
    const int totals[] = { 12, 99, 501, 999, 5001 };
    const size_t reps = 200000;

    printf("%-8s %-10s %-8s %-8s %-8s %-8s | %-8s %-8s %-8s %-8s\n",
           "routes", "rss_kb", "static", "param", "wild", "miss",
           "old_st", "old_pm", "old_miss", "factor");
    for (size_t t = 0; t < 5; t++) {
        int total = totals[t];
        int groups = total / 3;
        Server s;
        for (int g = 0; g < groups; g++) {
            char p1[64], p2[64], p3[64];
            snprintf(p1, sizeof p1, "/api/v%d/users", g);
            snprintf(p2, sizeof p2, "/api/v%d/users/:id", g);
            snprintf(p3, sizeof p3, "/api/v%d/files/*path", g);
            s.handle("GET", p1, noop);
            s.handle("GET", p2, noop);
            s.handle("GET", p3, noop);
        }
        int g = groups / 2;
        char hit_s[64], hit_p[64], hit_w[64], miss_p[64];
        snprintf(hit_s, sizeof hit_s, "/api/v%d/users", g);
        snprintf(hit_p, sizeof hit_p, "/api/v%d/users/123456", g);
        snprintf(hit_w, sizeof hit_w, "/api/v%d/files/a/b/c/d", g);
        snprintf(miss_p, sizeof miss_p, "/api/v%d/nothing/here", g);

        Request req;
        double new_st = ns_per_op([&]() { s.match("GET", hit_s, req); }, reps);
        double new_pm = ns_per_op([&]() { s.match("GET", hit_p, req); }, reps);
        double new_wd = ns_per_op([&]() { s.match("GET", hit_w, req); }, reps);
        double new_ms = ns_per_op([&]() { s.match("GET", miss_p, req); }, reps);

        double old_st = ns_per_op([&]() { old_match(s.routes, "GET", hit_s, req); }, reps);
        double old_pm = ns_per_op([&]() { old_match(s.routes, "GET", hit_p, req); }, reps);
        double old_ms = ns_per_op([&]() { old_match(s.routes, "GET", miss_p, req); }, reps);

        // steady-state allocation proof on the four worst-case series (mixed)
        g_allocs = 0;
        g_count = true;
        for (size_t i = 0; i < 100000; i++) {
            s.match("GET", i % 2 ? hit_p : hit_w, req);
        }
        g_count = false;

        long long rss = vm_rss_kb();
        double factor = old_ms / new_ms; // miss path: pure lookup cost
        printf("%-8d %-10lld %-8.2f %-8.2f %-8.2f %-8.2f | %-8.2f %-8.2f %-8.2f %-8.2f   allocs/100k=%lld\n",
               (int)s.routes.size(), rss, new_st, new_pm, new_wd, new_ms, old_st, old_pm, old_ms, factor, g_allocs);
    }
    return 0;
}