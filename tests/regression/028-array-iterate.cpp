// ms2.3: 028-array-iterate.cpp — iteration optimization: JSON encoding and
// size accounting iterate a hoisted contiguous run pointer (ms2.3 accessor
// `values()`), so repeated encode of the same inline or spilled array into a
// reused sink is allocation-free. Nested arrays-of-objects iterate the same
// way. Also locks that a spilled (9-element) array encodes identically to its
// inline (4-element) sibling.
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

    hs::Value inline4 = hs::Value::arr();
    for (int i = 0; i < 4; i++) inline4.push(hs::Value::i64(i));
    hs::Value spill9 = hs::Value::arr();
    for (int i = 0; i < 9; i++) spill9.push(hs::Value::i64(i * 10));

    // Reused sink keeps capacity, so encodes must allocate nothing after the
    // first priming encode (beyond constructing `sink` itself).
    std::string sink;
    sink.reserve(128);

    for (int r = 0; r < 2; r++) {
        // Ordering established in 027: 4-element -> box only; 9 -> box + growth.
        sink.clear();
        g_allocs = 0; g_bytes = 0;
        hs::value_to_json(inline4, sink);
        check(sink == "[0,1,2,3]", "inline4 encodes once");
        check(g_allocs == 0, "inline4 one-shot encode: zero allocs");
        check((size_t)hs::value_size(inline4) == sink.size(), "inline4 size == encoded len");

        g_allocs = 0;
        for (int rpt = 0; rpt < 100000; rpt++) { sink.clear(); hs::value_to_json(inline4, sink); }
        check(g_allocs == 0, "100k inline4 encodes: zero allocs");
        check(sink == "[0,1,2,3]", "inline4 encode stable after loop");

        g_allocs = 0;
        for (int rpt = 0; rpt < 100000; rpt++) { sink.clear(); hs::value_to_json(spill9, sink); }
        check(g_allocs == 0, "100k spill9 encodes: zero allocs");
        check(sink == "[0,10,20,30,40,50,60,70,80]", "spill9 encode correct");
    }

    // Nested iteration: array of 4 structurally-identical inline objects.
    {
        hs::Value nested = hs::Value::arr();
        for (int i = 0; i < 4; i++) {
            hs::Value o = hs::Value::obj();
            o.set("id", hs::Value::i64(i));
            o.set("ok", hs::Value::boolean(true));
            nested.push(std::move(o));
        }
        check(hs::value_to_json_string(nested) ==
            "[{\"id\":0,\"ok\":true},{\"id\":1,\"ok\":true},{\"id\":2,\"ok\":true},{\"id\":3,\"ok\":true}]",
            "nested array-of-objects json");

        g_allocs = 0;
        for (int rpt = 0; rpt < 100000; rpt++) { sink.clear(); hs::value_to_json(nested, sink); }
        check(g_allocs == 0, "100k nested encodes: zero allocs");
    }

    std::printf("ok\n");
    return fails ? 1 : 0;
}