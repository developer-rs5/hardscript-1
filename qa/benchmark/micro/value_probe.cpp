// ms2.0 escape-analysis probe: measures (a) sizeof the value-engine types,
// (b) heap allocations for the SSO + inline-small-vector fast paths, and
// (c) const-pool interning cost + dedup behaviour. Analysis only — no
// optimization is enabled; the numbers feed reports/value-foundation.md.
#include <hs_runtime.hpp>
#include <unistd.h>
#include <cstdlib>

static std::size_t g_allocs = 0;
static std::size_t g_bytes = 0;

static void* counted_new(std::size_t n) {
    g_allocs++;
    g_bytes += n;
    void* p = std::malloc(n ? n : 1);
    if (!p) throw std::bad_alloc();
    return p;
}
void* operator new(std::size_t n) { return counted_new(n); }
void* operator new[](std::size_t n) { return counted_new(n); }
void operator delete(void* p) noexcept { std::free(p); }
void operator delete[](void* p) noexcept { std::free(p); }
void operator delete(void* p, std::size_t) noexcept { std::free(p); }
void operator delete[](void* p, std::size_t) noexcept { std::free(p); }

struct Meas { std::size_t allocs; std::size_t bytes; };
static Meas snap() {
    Meas s{g_allocs, g_bytes};
    g_allocs = 0; g_bytes = 0;
    return s;
}
static void emit(const char* s) { (void)!::write(1, s, __builtin_strlen(s)); }

int main() {
    using namespace hs;
    char buf[256];

    // Warm the process: first std::string / malloc calls pull in glibc
    // bootstrap allocations (tcache, locales) that are not attributable to
    // the value engine. Discard everything before this marker.
    {
        std::string warm("warm-me-up");
        (void)warm;
        Value v = Value::obj();
        v.set("warm", Value::i64(1));
        (void)v;
    }
    (void)value_to_json_string(Value::boolean(true));  // touch serializer
    snap();

    // 1) SSO boundary: 14 chars stay inline (0 allocs), 15 chars spill (1).
    snap();
    Value s14 = Value::str("fourteen-char!");   // exactly 14
    Value s15 = Value::str("fifteen-char-s!");  // exactly 15
    Meas sso = snap();
    (void)s14; (void)s15;

    // 2) Inline small-vector: 4 elements -> no per-element allocations.
    snap();
    Value a4 = Value::arr();
    for (int i = 0; i < 4; i++) a4.push(Value::i64(i));
    Meas arr4 = snap();
    (void)a4;

    // 3) 5th element spills to the enclosed fallback vector.
    Value a9 = Value::arr();
    for (int i = 0; i < 9; i++) a9.push(Value::i64(i));
    volatile std::size_t live = a9.size(); (void)live;
    Meas arr9 = snap();

    // 4) Const pool: N identical interns -> pool_size 1; steady-state dedup
    //    adds no allocations (interns return the stored pointer).
    const char* first = const_pool().intern("boom-header-name");
    Meas pool = snap();
    const char* steady[4] = {};
    for (int i = 0; i < 1000; i++) { steady[i % 4] = const_pool().intern("boom-header-name"); }
    Meas dedup = snap();
    bool dedup_ok = first == steady[0] && steady[0] == steady[1] && steady[1] == steady[2];
    std::size_t pool_size = const_pool().size();

    int n = snprintf(buf, sizeof buf,
        "sizeof(Value)=%zu sizeof(ValueVec)=%zu sizeof(ValueObj)=%zu\n"
        "sso14+sso15: allocs=%zu (bytes=%zu)\n"
        "arr4 inline: allocs=%zu (bytes=%zu)\n"
        "arr9 spill: allocs=%zu (bytes=%zu)\n"
        "pool first intern: allocs=%zu (bytes=%zu) pool_size=%zu\n"
        "pool 1000 dedup interns: allocs=%zu (bytes=%zu) dedup=%s\n",
        sizeof(Value), sizeof(ValueVec), sizeof(ValueObj),
        sso.allocs, sso.bytes, arr4.allocs, arr4.bytes, arr9.allocs, arr9.bytes,
        pool.allocs, pool.bytes, pool_size, dedup.allocs, dedup.bytes,
        dedup_ok ? "yes" : "no");
    (void)n;
    emit(buf);
    return 0;
}