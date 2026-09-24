// ms2.1: 022-nil-inline.cpp — Nil is the default-constructed inline tag:
// no allocation for any kind of nil construction/copy, JSON renders null.
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

    hs::Value n = hs::Value::nil();
    hs::Value d;

    check(n.kind() == hs::ValueKind::Nil, "kind Nil");
    check(d.kind() == hs::ValueKind::Nil, "default kind Nil");
    check(n.is(hs::ValueKind::Nil), "is Nil");
    check(std::strcmp(hs::value_kind_name(hs::ValueKind::Nil), "Nil") == 0, "kind name");
    check(hs::value_to_json_string(n) == "null", "json null");
    check(hs::value_size(n) == 4, "nil size");
    check(hs::val_from_value(n).is_nil(), "bridge to Val nil");
    check(hs::value_to_json_string(hs::Value::nil()) == "null", "nil() json");

    size_t a0 = g_allocs;
    volatile bool sink = false;
    for (int i = 0; i < 200000; i++) {
        hs::Value x = hs::Value::nil();
        sink = x.is(hs::ValueKind::Nil);
    }
    (void)sink;
    check(g_allocs == a0, "200k nils create zero allocations");

    std::printf("ok\n");
    return fails ? 1 : 0;
}