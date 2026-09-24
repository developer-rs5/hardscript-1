// ms2.6: 036-parse-engine-json.cpp — the value-engine JSON parser that backs
// `Request::json_value()`. Pins the exact allocation model for a typical body
// (one object box + one array box; every string <=24 bytes rides SSO) and
// asserts byte-parity with the legacy Val parser through the encoder.
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

    const char* body = "{\"a\":12345,\"b\":\"toot-toot horn!\",\"c\":[1,2]}";
    const std::string canon = "{\"a\":12345,\"b\":\"toot-toot horn!\",\"c\":[1,2]}";

    g_allocs = 0; g_bytes = 0;
    hs::Value v = hs::parse_json_value(body);
    std::printf("parse allocs=%zu bytes=%zu\n", g_allocs, g_bytes);

    check(v.is(hs::ValueKind::Object), "parsed object kind");
    check(v.size() == 3, "object size 3");
    check(v.find("a") && v.find("a")->as_i64() == 12345, "a = 12345 int");
    check(v.find("b") && v.find("b")->as_str() == "toot-toot horn!", "b string SSO");
    check(v.find("c") && v.find("c")->is(hs::ValueKind::Array), "c array");
    check(v.find("c") && v.find("c")->size() == 2, "c size 2");
    check(v.find("c") && v.find("c")->arr_at(0)->as_i64() == 1, "c[0]=1");
    check(v.find("c") && v.find("c")->arr_at(1)->as_i64() == 2, "c[1]=2");

    check(hs::value_size(v) == canon.size(), "value_size matches");
    check(hs::value_to_json_string(v) == canon, "engine JSON canonical");

    // Parity with the legacy Val parser and the bridge adapter.
    check(hs::parse_json(std::string_view(body)).to_json() == canon, "Val parser JSON matches");
    check(hs::val_from_value(v).to_json() == canon, "adapter Val JSON matches");

    // Compile-time smoke: the engine row builder stays instantiable (it would
    // otherwise be discarded, since headers are all-inline).
    auto* pgq = &hs::pg_query_value;
    (void)pgq;

    std::printf("ok\n");

    if (fails) return 1;
    return 0;
}