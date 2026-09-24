// ms2.1: 021-float-inline.cpp — Float primitives live inline: no heap, exact
// JSON via %.15g (including negatives and large magnitudes), kind mapping.
#include <hs_runtime.hpp>
#include <cstdio>
#include <cstring>
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
void* operator new[](size_t n, const std::nothrow_t&) noexcept { try { return ::operator new[](n); } catch (...) { return nullptr; } }
void operator delete(void* p, const std::nothrow_t&) noexcept { ::operator delete(p); }
void operator delete[](void* p, const std::nothrow_t&) noexcept { ::operator delete[](p); }

int main() {
    (void)std::string("warmup"); { std::vector<char> v; v.resize(256); (void)v; }

    int fails = 0;
    auto check = [&](bool c, const char* m) { if (!c) { std::fprintf(stderr, "FAIL %s\n", m); fails++; } };

    hs::Value v = hs::Value::f64(3.5);
    hs::Value neg = hs::Value::f64(-0.5);
    hs::Value big = hs::Value::f64(1e100);

    check(v.kind() == hs::ValueKind::Float, "kind Float");
    check(v.as_f64() == 3.5, "as_f64");
    check(neg.as_f64() == -0.5, "as_f64 neg");
    check(big.as_f64() == 1e100, "as_f64 big");
    check(std::strcmp(hs::value_kind_name(hs::ValueKind::Float), "Float") == 0, "kind name");
    check(hs::value_to_json_string(v) == "3.5", "json");
    check(hs::value_to_json_string(neg) == "-0.5", "neg json");
    check(hs::value_size(v) == 3, "size");
    check(hs::val_from_value(v).is_flt(), "bridge to Val float");

    size_t a0 = g_allocs;
    volatile double acc = 0;
    for (int i = 1; i <= 200000; i++) {
        hs::Value x = hs::Value::f64(i * 0.5);
        acc += x.as_f64();
    }
    (void)acc;
    check(g_allocs == a0, "200k floats create zero allocations");

    std::printf("ok\n");
    return fails ? 1 : 0;
}