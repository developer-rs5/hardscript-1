// ms2.2: 025-sso-large.cpp — long strings: past the 24-byte SSO threshold every
// value owns a heap std::string (exactly 2 allocs: 32-byte object + size+1
// buffer), survives copy-independence, and round-trips through JSON. Exact
// per-instance byte accounting is pinned (libstdc++ on this toolchain).
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

    for (size_t s : {size_t(32), size_t(64), size_t(128), size_t(512), size_t(1024)}) {
        std::string in(s, 'y');
        g_allocs = 0; g_bytes = 0;
        hs::Value v = hs::Value::str(in);
        size_t a = g_allocs, b = g_bytes;   // snapshot BEFORE json exercises the encoder
        check(a == 2, ("2 allocs at size=" + std::to_string(s)).c_str());
        check(b == 32 + s + 1, ("exact bytes at size=" + std::to_string(s)).c_str());
        check(v.as_str().size() == s, ("as_str size=" + std::to_string(s)).c_str());
        check(v.as_str() == in, ("content=" + std::to_string(s)).c_str());

        // The value owns a copy: mutating the source does not leak through.
        std::string orig = in;
        in[0] = 'X';
        check(v.as_str() == orig, "long value is an independent copy");

        // JSON round-trip.
        std::string j = hs::value_to_json_string(v);
        check(j == "\"" + orig + "\"", ("json round-trip size=" + std::to_string(s)).c_str());
    }

    std::printf("ok\n");
    return fails ? 1 : 0;
}