// ms2.1: 020-bool-inline.cpp — Bool is a tagged-union inline primitive with
// zero heap cost. Checks kind mapping, as_bool, JSON rendering and size.
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

    hs::Value t = hs::Value::boolean(true);
    hs::Value f = hs::Value::boolean(false);

    check(t.kind() == hs::ValueKind::Bool, "kind Bool");
    check(f.kind() == hs::ValueKind::Bool, "kind Bool false");
    check(t.as_bool() == true, "as_bool");
    check(f.as_bool() == false, "as_bool false");
    check(std::strcmp(hs::value_kind_name(hs::ValueKind::Bool), "Bool") == 0, "kind name");
    check(hs::value_to_json_string(t) == "true", "json true");
    check(hs::value_to_json_string(f) == "false", "json false");
    check(hs::value_size(t) == 4 && hs::value_size(f) == 5, "bool sizes");
    check(hs::val_from_value(t).is_bool(), "bridge to Val bool");

    size_t a0 = g_allocs;
    volatile bool sink = false;
    for (int i = 0; i < 200000; i++) {
        hs::Value x = hs::Value::boolean((i & 1) == 0);
        sink = x.as_bool();
    }
    (void)sink;
    check(g_allocs == a0, "200k bools create zero allocations");

    std::printf("ok\n");
    return fails ? 1 : 0;
}