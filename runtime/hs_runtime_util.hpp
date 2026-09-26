#ifndef HS_RUNTIME_UTIL_HPP
#define HS_RUNTIME_UTIL_HPP
#include "hs_runtime_http.hpp"
namespace hs {

// ===========================================================================
// CLI helpers used by generated code
// ===========================================================================
inline std::vector<std::string>& g_args() {
    static std::vector<std::string> v;
    return v;
}
inline void set_args(int argc, char** argv) {
    g_args().clear();
    for (int i = 1; i < argc; i++) g_args().push_back(argv[i]);
}
inline int argc() { return (int)g_args().size(); }
inline std::string argv(int i) {
    if (i < 0 || i >= (int)g_args().size()) return "";
    return g_args()[(size_t)i];
}
inline std::string pid() { return std::to_string((long long)getpid()); }
inline std::string hostname() {
    char b[256];
    return gethostname(b, sizeof b) == 0 ? std::string(b) : "";
}
inline std::string platform() {
#if defined(__linux__)
    return "linux";
#elif defined(__APPLE__)
    return "macos";
#elif defined(_WIN32)
    return "windows";
#else
    return "unix";
#endif
}
inline int64_t cpus() {
    long n = sysconf(_SC_NPROCESSORS_ONLN);
    return n > 0 ? n : 1;
}

// ===========================================================================
// Codegen helpers (used by generated C++)
// ===========================================================================
inline std::string random_uuid() {
    std::string h = random_hex(16);
    h[12] = '4';
    h[16] = ('8' + (h[16] % 2)); // variant 10xx
    return h.substr(0, 8) + "-" + h.substr(8, 4) + "-" + h.substr(12, 4) + "-" +
           h.substr(16, 4) + "-" + h.substr(20, 12);
}

// Turn a HardScript route/fn value into an HTTP response: strings become
// text responses, maps/lists become JSON, nil becomes 204. The drain hook
// (hs_respond_drain_hook in hs_runtime_http.hpp, set by the session manager)
// appends any cookies the handler queued and resets per-request state, so
// every route return carries its cookies exactly once.
inline Response hs_respond_inner(const Val& v) {
    if (v.is_nil()) return Response::empty(204);
    if (v.is_str()) return Response::text(v.sv);
    return Response::json(v);
}
inline Response hs_respond(const Val& v) {
    Response r = hs_respond_inner(v);
    if (hs_respond_drain_hook) hs_respond_drain_hook(r);
    return r;
}
// Rvalue overload: `<- {...}` temporaries move their payload straight into
// the response so the JSON serializer streams the value to the socket arena.
inline Response hs_respond_inner(Val&& v) {
    if (v.is_nil()) return Response::empty(204);
    if (v.is_str()) return Response::text(std::move(v.sv));
    return Response::json(std::move(v));
}
inline Response hs_respond(Val&& v) {
    Response r = hs_respond_inner(std::move(v));
    if (hs_respond_drain_hook) hs_respond_drain_hook(r);
    return r;
}

inline Val args_list() {
    std::vector<Val> v;
    for (int i = 0; i < argc(); i++) v.push_back(Val::text(argv(i)));
    return Val::list(std::move(v));
}

inline bool want_test() {
    for (int i = 0; i < argc(); i++) {
        std::string a = argv(i);
        if (a == "--test" || a == "test") return true;
    }
    return false;
}

inline Server*& g_server() {
    static Server* s = nullptr;
    return s;
}
inline void hs_set_server(Server* s) { g_server() = s; }
inline Val test_call(const std::string& verb, const std::string& path, const Val& body) {
    Server* s = g_server();
    if (!s) throw std::runtime_error("http call used outside the running app");
    return s->call(verb, path, body);
}

// Run tasks concurrently and return the first result to complete.
inline Val race_val(std::vector<std::function<Val()>> tasks) {
    const size_t n = tasks.size();
    std::vector<Val> results(n);
    Val first;
    std::mutex m;
    bool done = false;
    if (n == 1) return tasks[0]();
    std::vector<std::thread> ts;
    ts.reserve(n);
    for (size_t i = 0; i < n; i++) {
        ts.emplace_back([&, i]() {
            Val r;
            try { r = tasks[i](); }
            catch (...) { r = Val::nil(); }
            std::lock_guard<std::mutex> lk(m);
            results[i] = r;
            if (!done) { first = r; done = true; }
        });
    }
    for (auto& t : ts) t.join();
    return first;
}

} // namespace hs

#endif
