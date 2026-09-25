// ms2.4: 032-const-pool-bench.cpp — steady-state interning is allocation-free:
// once a constant is pooled, repeating it through the typed helpers or the
// generic `intern_value` dispatcher is a lookup only. All values the loops
// touch are warmed first so the numbers measure pure steady-state dedup.
#include <hs_runtime.hpp>
#include <cstdio>
#include <cstdlib>
#include <new>
#include <string>
#include <vector>

static size_t g_allocs = 0;
static size_t g_bytes = 0;

void* operator new(size_t n) { g_allocs++; g_bytes += n; if (void* p = std::malloc(n)) return p; throw std::bad_alloc(); }
void* operator new[](size_t n) { return ::operator new(n); }
void operator delete(void* p) noexcept { std::free(p); }
void operator delete[](void* p) noexcept { std::free(p); }
void operator delete(void* p, size_t) noexcept { std::free(p); }
void operator delete[](void* p, size_t) noexcept { std::free(p); }
void* operator new(size_t n, const std::nothrow_t&) noexcept { try { return ::operator new(n); } catch (...) { return nullptr; } }
void operator delete(void* p, const std::nothrow_t&) noexcept { ::operator delete(p); }

int main() {
    (void)std::string("warmup"); { std::vector<char> v; v.resize(256); (void)v; }

    int fails = 0;
    auto check = [&](bool c, const char* m) { if (!c) { std::fprintf(stderr, "FAIL %s\n", m); fails++; } };

    using namespace hs;
    std::vector<std::string> seeds;
    for (int i = 0; i < 16; i++) seeds.push_back("pooled-header-value-" + std::to_string(i));  // > 24 B

    // Full warm set: every constant the steady-state loops reference.
    for (auto& s : seeds) { const_pool().intern(s); }
    for (int i = 0; i < 8; i++) { const_pool().intern_i64(i); }
    const_pool().intern_f64(2.5);
    const_pool().intern_bool(true);
    const_pool().intern_bool(false);
    const_pool().intern_empty_array();
    const_pool().intern_empty_object();
    const size_t warmed = const_pool().count_items();
    check(warmed == 16 + 8 + 1 + 2 + 1 + 1, "warm set size");

    // Steady-state string interning: repeats over the pooled set.
    {
        std::vector<const Value*> refs;
        refs.reserve(100000);                      // before the counted window
        g_allocs = 0; g_bytes = 0;
        for (int r = 0; r < 100000; r++) refs.push_back(const_pool().intern_str(seeds[r % 16]));
        volatile size_t live = refs.size();
        std::printf("100k string dedup: allocs=%zu bytes=%zu\n", g_allocs, g_bytes);
        check(g_allocs == 0, "string dedup repeats: zero allocs");
        (void)live;
    }

    // Steady-state number dedup over a small set.
    g_allocs = 0; g_bytes = 0;
    for (int r = 0; r < 100000; r++) { const_pool().intern_i64(r % 8); }
    std::printf("100k int dedup: allocs=%zu bytes=%zu\n", g_allocs, g_bytes);
    check(g_allocs == 0, "int dedup repeats: zero allocs");

    // Steady-state booleans + empty containers + strings via intern_value. The
// dispatched values are constructed allocation-free (inline booleans, view
// strings, and the already-pooled empty singletons), so any allocation here
// would come from the pool itself.
    {
        const Value* ea = const_pool().intern_empty_array();
        const Value* eo = const_pool().intern_empty_object();
        g_allocs = 0; g_bytes = 0;
        for (int r = 0; r < 100000; r++) {
            const_pool().intern_value(Value::boolean(r % 2 == 0));
            const_pool().intern_value(Value::str_view(seeds[r % 16]));
            const_pool().intern_value(*ea);
            const_pool().intern_value(*eo);
        }
        std::printf("100k intern_value dedup: allocs=%zu bytes=%zu\n", g_allocs, g_bytes);
        check(g_allocs == 0, "intern_value repeats: zero allocs");
    }

    // freeze() is now idempotent for dedup: re-interning still returns the
    // same slots and nothing grows.
    const_pool().freeze();
    const size_t after = const_pool().count_items();
    g_allocs = 0;
    for (int r = 0; r < 100000; r++) { const_pool().intern_value(Value::str(seeds[r % 16])); }
    check(g_allocs == 0, "post-freeze dedup: zero allocs");
    check(const_pool().count_items() == after && after == warmed, "pool count frozen");
    check(const_pool().size() == 16, "legacy string count unchanged");

    std::printf("ok\n");
    return fails ? 1 : 0;
}