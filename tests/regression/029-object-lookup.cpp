// ms2.3: 029-object-lookup.cpp — inline object iteration/lookup: the ms2.3
// `pairs()`/`find()` paths hoist one contiguous run, so repeated lookup,
// in-place update (set over an existing key) and encoding of a 4-key inline
// object are all allocation-free.
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

    hs::Value o = hs::Value::obj();
    o.set("name", hs::Value::str("hardscript"));
    o.set("id", hs::Value::i64(42));
    o.set("ratio", hs::Value::f64(2.5));
    o.set("ok", hs::Value::boolean(true));

    check(o.size() == 4, "four inline keys");
    check(o.find("name") && o.find("name")->as_str() == "hardscript", "find string");
    check(o.find("id") && o.find("id")->as_i64() == 42, "find int");
    check(o.find("ratio") && o.find("ratio")->as_f64() == 2.5, "find float");
    check(o.find("ok") && o.find("ok")->as_bool(), "find bool");
    check(o.find("missing") == nullptr, "missing key is null");

    // Pairs run hands back the same values, in insertion order.
    {
        const std::pair<std::string, hs::Value>* p = o.pairs();
        check(p != nullptr, "pairs non-null");
        check(p[0].first == "name" && p[3].first == "ok", "pairs insertion order");
    }

    // 100k lookups: existing + missing keys, zero allocations.
    {
        std::string_view hit("id"), miss("nope");
        g_allocs = 0;
        int64_t t = 0;
        for (int r = 0; r < 100000; r++) {
            const hs::Value* v = o.find(r % 2 ? hit : miss);
            if (v) t += v->as_i64();
        }
        check(g_allocs == 0, "100k find: zero allocs");
        check(t == (int64_t)50000 * 42, "find sums hits only");
    }

    // 100k in-place updates of an existing key: zero allocations.
    {
        g_allocs = 0;
        for (int r = 0; r < 100000; r++) o.set(std::string("id"), hs::Value::i64(r));
        check(g_allocs == 0, "100k set-over-existing-key: zero allocs");
        check(o.find("id")->as_i64() == 99999, "set updates value");
        check(o.size() == 4, "set does not grow the object");
    }

    // Repeated encode with a reused sink: zero allocations.
    {
        std::string sink;
        sink.reserve(128);
        g_allocs = 0;
        for (int r = 0; r < 100000; r++) { sink.clear(); hs::value_to_json(o, sink); }
        check(g_allocs == 0, "100k object encodes: zero allocs");
        check(sink == "{\"name\":\"hardscript\",\"id\":99999,\"ratio\":2.5,\"ok\":true}", "object json stable");
    }

    std::printf("ok\n");
    return fails ? 1 : 0;
}