// ms2.2: 026-sso-utf8.cpp — UTF-8 boundary safety: SSO copies raw bytes
// without ever interpreting them, so a multibyte codepoint is never truncated
// mid-sequence on the inline path. Exercises 3-byte (U+20AC) and 4-byte
// (U+1F980) sequences at and just past the 24-byte kSso threshold, including a
// codepoint straddling the boundary, plus JSON pass-through of raw UTF-8.
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

    // Plain accented text (non-ASCII but ≤ 24 bytes) stays inline.
    {
        std::string in = "héllo wörld";
        g_allocs = 0;
        hs::Value v = hs::Value::str(in);
        check(g_allocs == 0, "accented text inline");
        check(v.as_str() == in, "accented text intact");
    }

    // U+20AC is 3 bytes: 8 x = 24 bytes -> inline, zero allocs.
    {
        std::string in;
        for (int i = 0; i < 8; i++) in += "€";
        check(in.size() == 24, "8x EUR is 24 bytes");
        g_allocs = 0;
        hs::Value v = hs::Value::str(in);
        check(g_allocs == 0, "24-byte EUR inline");
        check(v.as_str() == in, "24-byte EUR intact");
    }

    // Same + 1 ASCII byte = 25 bytes -> heap, bytes preserved exactly.
    {
        std::string in;
        for (int i = 0; i < 8; i++) in += "€";
        in += "x";
        check(in.size() == 25, "9 chars EUR+x is 25 bytes");
        g_allocs = 0; g_bytes = 0;
        hs::Value v = hs::Value::str(in);
        check(g_allocs == 2, "25-byte EUR heap");
        check(v.as_str() == in, "25-byte EUR intact");
    }

    // U+1F980 (crab) is 4 bytes: 5 x = 20 bytes -> inline; 7 x = 28 -> heap.
    {
        std::string in;
        for (int i = 0; i < 5; i++) in += "🦀";
        check(in.size() == 20, "5x crab is 20 bytes");
        g_allocs = 0;
        hs::Value v = hs::Value::str(in);
        check(g_allocs == 0, "20-byte emoji inline");
        check(v.as_str() == in, "20-byte emoji intact");
    }
    {
        std::string in;
        for (int i = 0; i < 7; i++) in += "🦀";
        check(in.size() == 28, "7x crab is 28 bytes");
        g_allocs = 0;
        hs::Value v = hs::Value::str(in);
        check(g_allocs == 2, "28-byte emoji heap");
        check(v.as_str() == in, "28-byte emoji intact");
    }

    // Codepoint straddling the boundary: 23 ASCII + 3-byte EUR = 26 bytes.
    // The inline path would be byte 24-25 of the codepoint; ensure no
    // truncation occurs anywhere.
    {
        std::string in(23, 'a');
        in += "€";
        check(in.size() == 26, "straddle is 26 bytes");
        hs::Value v = hs::Value::str(in);
        check(v.as_str() == in, "straddling codepoint intact");
        check(v.as_str().substr(23) == "€", "straddling codepoint bytes preserved");
    }

    // JSON encodes non-ASCII bytes verbatim (no mangling of UTF-8).
    {
        std::string in = "héllo wörld";
        hs::Value v = hs::Value::str(in);
        check(hs::value_to_json_string(v) == "\"" + in + "\"", "utf8 json pass-through");
        check(hs::value_size(v) == 2 + in.size(), "utf8 size accounting");
    }

    std::printf("ok\n");
    return fails ? 1 : 0;
}