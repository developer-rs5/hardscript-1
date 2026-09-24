// ms2.2: 024-sso-small.cpp — Small String Optimization: strings up to kSso
// (24) bytes live inside the Value with zero heap traffic; the 25th byte moves
// the value to the heap (std::string object + buffer = 2 allocs). Also pins
// the empty-string fix: str("") no longer reads indeterminate union memory,
// and copying an SSO string never allocates.
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

    check(hs::Value::kSso == 24, "kSso is 24");

    // 0..24 bytes stay inline: zero allocations, content intact.
    for (size_t s : {size_t(0), size_t(1), size_t(8), size_t(16), size_t(23), size_t(24)}) {
        std::string in(s, 'a');
        g_allocs = 0; g_bytes = 0;
        hs::Value v = hs::Value::str(in);
        check(g_allocs == 0, ("inline size=" + std::to_string(s)).c_str());
        check(v.as_str().size() == s, ("as_str size=" + std::to_string(s)).c_str());
        check(hs::value_to_json_string(v) == "\"" + in + "\"", "inline json");
    }

    // Empty string: valid content, empty JSON, zero allocs (was UB before the
    // SSO storage flag).
    {
        hs::Value e = hs::Value::str("");
        check(e.as_str().empty(), "empty as_str");
        check(e.as_str().data() != nullptr, "empty as_str has valid pointer");
        check(hs::value_to_json_string(e) == "\"\"", "empty json");
        g_allocs = 0;
        hs::Value c = e;
        check(g_allocs == 0, "empty copy zero alloc");
        check(c.as_str().empty(), "empty copy content");
    }

    // Copying an SSO string is a plain byte copy: zero allocations.
    {
        std::string in(24, 'q');
        hs::Value v = hs::Value::str(in);
        g_allocs = 0;
        hs::Value c = v;
        check(g_allocs == 0, "sso copy zero alloc");
        check(c.as_str() == in, "sso copy content");
        std::string orig = in;
        in[0] = 'Z';
        check(v.as_str() == orig, "sso copy is independent");
    }

    // Boundary: 25 bytes moves to the heap — exactly 2 allocs (object + buf).
    {
        std::string in(25, 'b');
        g_allocs = 0; g_bytes = 0;
        hs::Value v = hs::Value::str(in);
        check(g_allocs == 2, "25-byte value is 2 allocs");
        check(v.as_str() == in, "25-byte content");
        check(hs::value_size(v) == 2 + 25, "25-byte size accounting");
    }

    std::printf("ok\n");
    return fails ? 1 : 0;
}