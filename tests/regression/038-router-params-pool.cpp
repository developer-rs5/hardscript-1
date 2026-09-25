// ms2.6: 038-router-params-pool.cpp — router param map is view-based: keys
// point into the route pattern (process lifetime), values into the request
// path (valid while path storage lives). Parameterized dispatch costs one map
// node per param and zero const-pool interning; static routes dispatch with
// zero allocations. Pins the router integration model so a copy regression
// fails the suite.
#include <hs_runtime.hpp>
#include <cstdio>
#include <cstdlib>
#include <map>
#include <new>
#include <string>
#include <string_view>
#include <type_traits>
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

    static_assert(std::is_same<decltype(hs::Request::params),
                  std::map<std::string_view, std::string_view>>::value,
                  "ms2.6: Request::params must be a view map");

    hs::Server srv;
    auto h = [](const hs::Request&) { return hs::Response::empty(); };
    srv.handle("GET", "/users/:id/photos/:photo", h);
    srv.handle("GET", "/health", h);

    std::string path = "/users/42/photos/7";
    hs::Request r;
    r.method = "GET";
    r.path = path;
    int ri = srv.match("GET", path, r);
    check(ri >= 0, "param route matched");
    check(r.params.size() == 2, "two params captured");
    check(r.params.at("id") == "42", "id == 42");
    check(r.params.at("photo") == "7", "photo == 7");
    check(r.pview("id") == "42", "pview id == 42");
    std::string_view v = r.params["id"];
    check(v == "42", "value is a view over path storage");
    std::printf("params route id=%s photo=%s\n",
                std::string(r.params["id"]).c_str(),
                std::string(r.params["photo"]).c_str());

    size_t pool0 = hs::const_pool().count_items();
    const int N = 2000;

    g_allocs = 0;
    for (int i = 0; i < N; i++) {
        hs::Request q;
        q.method = "GET";
        q.path = path;
        srv.match("GET", path, q);
    }
    size_t per = g_allocs / (size_t)N;
    size_t pool_delta = hs::const_pool().count_items() - pool0;
    std::printf("param match allocs per=%zu pool_delta=%zu\n", per, pool_delta);
    check(per == 2, "param dispatch: one map node per param");
    check(pool_delta == 0, "dispatch interns nothing into the const pool");

    std::string spath = "/health";
    g_allocs = 0;
    for (int i = 0; i < N; i++) {
        hs::Request q;
        q.method = "GET";
        q.path = spath;
        srv.match("GET", spath, q);
    }
    size_t sper = g_allocs / (size_t)N;
    std::printf("static match allocs per=%zu\n", sper);
    check(sper == 0, "static dispatch: zero allocations");

    check(v == "42", "view stays valid while path storage is alive");

    std::printf("ok\n");

    if (fails) return 1;
    return 0;
}