// ms2.6: 037-request-json-memo.cpp — Request::json_value parses the request
// body once into an engine Value and memoizes it; repeated calls return a
// fresh copy of the cached value (one box) instead of re-parsing. json() keeps
// the legacy Val adapter contract for empty and non-empty bodies.
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

    std::string b("{\"x\":1,\"y\":2}");
    hs::Request r;
    r.body = b;

    g_allocs = 0; g_bytes = 0;
    hs::Value v1 = r.json_value();
    std::printf("first json_value allocs=%zu bytes=%zu\n", g_allocs, g_bytes);

    g_allocs = 0; g_bytes = 0;
    hs::Value v2 = r.json_value();
    std::printf("memoized json_value allocs=%zu bytes=%zu\n", g_allocs, g_bytes);

    check(hs::value_to_json_string(v1) == "{\"x\":1,\"y\":2}", "engine canonical json");
    check(hs::value_to_json_string(v2) == "{\"x\":1,\"y\":2}", "memoized engine json");
    check(v1.find("x") && v1.find("x")->as_i64() == 1, "x == 1");
    check(v1.find("y") && v1.find("y")->as_i64() == 2, "y == 2");

    g_allocs = 0; g_bytes = 0;
    hs::Val a = r.json();
    check(a.to_json() == "{\"x\":1,\"y\":2}", "json() adapter body json");
    check(hs::val_from_value(r.json_value()).to_json() == "{\"x\":1,\"y\":2}", "adapter parity");

    hs::Request e;
    check(e.json().to_json() == "{}", "empty body json() == {}");
    check(e.json_value().is(hs::ValueKind::Object) && e.json_value().size() == 0, "empty body engine object");
    check(hs::value_to_json_string(e.json_value()) == "{}", "empty body canonical");

    std::printf("ok\n");

    if (fails) return 1;
    return 0;
}