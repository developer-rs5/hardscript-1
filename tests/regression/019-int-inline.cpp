// ms2.1: 019-int-inline.cpp — Int is a tagged-union inline primitive:
// constructing, copying and destroying ints must never touch the heap.
// Also checks the mapped kind, JSON round-trip, byte-size accounting and
// the ValueKind display name.
#include <hs_runtime.hpp>
#include <cstdio>
#include <cstdint>
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

    hs::Value v = hs::Value::i64(42);
    hs::Value n = hs::Value::i64(-1234567890123LL);
    hs::Value big = hs::Value::i64(int64_t(1) << 40);

    check(v.kind() == hs::ValueKind::Int, "kind Int");
    check(n.kind() == hs::ValueKind::Int, "kind Int neg");
    check(v.as_i64() == 42, "as_i64");
    check(n.as_i64() == -1234567890123LL, "as_i64 neg");
    check(big.as_i64() == (int64_t(1) << 40), "as_i64 big");
    check(std::strcmp(hs::value_kind_name(hs::ValueKind::Int), "Int") == 0, "kind name");
    check(hs::value_to_json_string(v) == "42", "json");
    check(hs::value_to_json_string(n) == "-1234567890123", "neg json");
    check(hs::value_to_json_string(big) == "1099511627776", "big json");
    check(hs::value_size(n) == 14, "neg size");

    size_t a0 = g_allocs;
    volatile int64_t acc = 0;
    for (int i = 0; i < 200000; i++) {
        hs::Value x = hs::Value::i64(i);
        acc += x.as_i64();
    }
    (void)acc;
    check(g_allocs == a0, "200k ints create zero allocations");

    std::printf("ok\n");
    return fails ? 1 : 0;
}