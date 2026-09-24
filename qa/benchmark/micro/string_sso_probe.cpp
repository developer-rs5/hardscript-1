// ms2.2 string-SSO probe: for strings of 8/16/24/32/64/128 bytes, measures per
// value-engine allocations + bytes and RSS growth while holding 200k values
// live (spec: bench 8/16/24/32/64/128 B strings, allocs + RSS). Mirrors
// value_probe.cpp: operator-new counter, warmup, raw write emit; RSS read from
// /proc/self/status (VmRSS). Numbers feed reports/value-sso.md.
#include <hs_runtime.hpp>
#include <unistd.h>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

static std::size_t g_allocs = 0;
static std::size_t g_bytes = 0;

static void* counted_new(std::size_t n) {
    g_allocs++;
    g_bytes += n;
    void* p = std::malloc(n ? n : 1);
    if (!p) throw std::bad_alloc();
    return p;
}
void* operator new(std::size_t n) { return counted_new(n); }
void* operator new[](std::size_t n) { return counted_new(n); }
void operator delete(void* p) noexcept { std::free(p); }
void operator delete[](void* p) noexcept { std::free(p); }
void operator delete(void* p, std::size_t) noexcept { std::free(p); }
void operator delete[](void* p, std::size_t) noexcept { std::free(p); }

static std::size_t vmrss_kb() {
    FILE* f = std::fopen("/proc/self/status", "r");
    char line[256];
    while (f && std::fgets(line, sizeof line, f)) {
        if (std::strncmp(line, "VmRSS:", 6) == 0) {
            std::size_t rss = (std::size_t)std::atoll(line + 6);
            std::fclose(f);
            return rss;
        }
    }
    if (f) std::fclose(f);
    return 0;
}

static void emit(const char* s) { (void)!::write(1, s, __builtin_strlen(s)); }

int main() {
    using namespace hs;
    char buf[256];

    // Warm std::string (libstdc++ SSO machinery) so per-size deltas are clean.
    {
        std::string warm("warm-me-up");
        (void)warm;
        std::vector<Value> w;
        w.reserve(8);
        w.emplace_back(Value::str("warm-value"));
        (void)const_pool().intern("x");
    }
    g_allocs = 0; g_bytes = 0;

    const std::size_t sizes[] = {8, 16, 24, 32, 64, 128};
    const std::size_t kKind = sizeof sizes / sizeof sizes[0];
    const std::size_t kValues = 200000;

    std::string in;               // built once per size, outside the window
    for (std::size_t k = 0; k < kKind; k++) {
        std::size_t s = sizes[k];
        in.assign(s, 'a');

        std::vector<Value> held;
        held.reserve(kValues);    // 6.4 MB live Value storage, before the window
        std::size_t rss_before = vmrss_kb();

        g_allocs = 0; g_bytes = 0;
        for (std::size_t i = 0; i < kValues; i++) held.emplace_back(Value::str(in));
        std::size_t allocs = g_allocs, bytes = g_bytes;
        std::size_t rss_growth_kb = vmrss_kb() > rss_before ? vmrss_kb() - rss_before : 0;

        int n = std::snprintf(buf, sizeof buf,
            "size=%-4zu n=%-7zu allocs=%-6zu bytes=%-8zu allocs_per_value=%.2f bytes_per_value=%.2f rss_growth_kb=%zu\n",
            s, kValues, allocs, bytes,
            (double)allocs / kValues, (double)bytes / kValues, rss_growth_kb);
        (void)n;
        emit(buf);
    }
    return 0;
}