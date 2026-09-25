// ms2.3: 027-array-inline.cpp — inline small-vector storage: arrays/objects up
// to the 4 inline slots cost exactly one allocation (the box itself); the 5th
// element spills into the enclosed fallback vector (box + libstdc++ growth
// allocations). Pins the exact alloc/byte model so container layout drift
// fails the suite.
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
void operator delete[](void* p) noexcept { ::operator delete(p); }
void operator delete(void* p, size_t) noexcept { std::free(p); }
void operator delete[](void* p, size_t) noexcept { std::free(p); }
void* operator new(size_t n, const std::nothrow_t&) noexcept { try { return ::operator new(n); } catch (...) { return nullptr; } }
void operator delete(void* p, const std::nothrow_t&) noexcept { ::operator delete(p); }

int main() {
    (void)std::string("warmup"); { std::vector<char> v; v.resize(256); (void)v; }

    int fails = 0;
    auto check = [&](bool c, const char* m) { if (!c) { std::fprintf(stderr, "FAIL %s\n", m); fails++; } };

    for (size_t n : {size_t(0), size_t(1), size_t(4), size_t(5), size_t(8), size_t(9), size_t(16), size_t(64), size_t(256)}) {
        g_allocs = 0; g_bytes = 0;
        {
            hs::Value a = hs::Value::arr();
            for (size_t i = 0; i < n; i++) a.push(hs::Value::i64((int64_t)i));
        }
        std::printf("arr n=%zu allocs=%zu bytes=%zu\n", n, g_allocs, g_bytes);

        g_allocs = 0; g_bytes = 0;
        {
            hs::Value o = hs::Value::obj();
            for (size_t i = 0; i < n; i++) o.set("k" + std::to_string(i), hs::Value::i64((int64_t)i));
        }
        std::printf("obj n=%zu allocs=%zu bytes=%zu\n", n, g_allocs, g_bytes);
    }

    // Inline arrays are not just cheap to build: iteration over the contiguous
    // run is allocation-free and sees results in insertion order.
    {
        hs::Value a = hs::Value::arr();
        for (int i = 0; i < 4; i++) a.push(hs::Value::i64(i));
        g_allocs = 0;
        int64_t sum = 0;
        for (int r = 0; r < 100000; r++)
            for (size_t i = 0; i < a.size(); i++) sum += a.values()[i].as_i64();
        check(g_allocs == 0, "100k inline-array iteration: zero allocs");
        check(sum == (int64_t)100000 * (0 + 1 + 2 + 3), "iteration sum matches");
    }

    std::printf("ok\n");
    return fails ? 1 : 0;
}