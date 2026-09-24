#ifndef HS_RUNTIME_HTTP_HPP
#define HS_RUNTIME_HTTP_HPP
#include <netinet/tcp.h>
#include "hs_runtime_value.hpp"
#include "hs_runtime_arena.hpp"
// ===========================================================================
// HTTP
// ===========================================================================
// Graceful shutdown: SIGINT/SIGTERM set a flag so the accept loop can break
// out and main() can return normally (clean destructor + leak-sanitizer run).
// Kept intentionally tiny: no per-request cleanup depends on it, only the
// ability to stop the listener without killing the process abruptly.
inline std::atomic<bool> g_hs_stop{false};
inline std::atomic<int> g_hs_conn{0};
inline void hs_install_shutdown_signals() {
    struct sigaction sa;
    std::memset(&sa, 0, sizeof sa);
    sa.sa_handler = [](int) { g_hs_stop.store(true); };
    sigemptyset(&sa.sa_mask);
    sigaction(SIGINT, &sa, nullptr);
    sigaction(SIGTERM, &sa, nullptr);
}

// Per-milestone keep-alive toggle. M1.5 flips this and the request loop in
// handle_connection() closes over it; the parser is keep-alive aware from M1.1.
static constexpr bool g_hs_keepalive = true;
static constexpr size_t kMaxRequestBytes = 16 * 1024 * 1024;
static constexpr unsigned long long kMaxRequestsPerConn = 10000;

// ---------------------------------------------------------------------------
// Connection worker pool (ms1.3): reused handler threads instead of one thread
// per connection. The pool grows on demand (keep-alive connections pin a
// worker for their lifetime) up to kMaxWorkers, and idle workers return to
// the pool, so the per-worker thread_local arena amortizes to ~0 allocations.
struct ConnPool {
    std::mutex m;
    std::condition_variable cv;
    std::vector<int> q;
    std::atomic<bool> stop{false};
    std::atomic<unsigned> live{0}; // live (non-exited) workers, for pool shrink
    std::atomic<int> idle{0};
    std::atomic<unsigned> spawned{0};
    std::vector<std::thread> workers;
    size_t q_cap = 4096;
};
inline constexpr size_t kMaxWorkers = 256;
inline ConnPool& conn_pool() { static ConnPool p; return p; }

// ---------------------------------------------------------------------------
// Zero-allocation helpers
// ---------------------------------------------------------------------------
inline char fold_ascii(char c) {
    return (c >= 'A' && c <= 'Z') ? (char)(c - 'A' + 'a') : c;
}
inline bool fold_eq(std::string_view a, std::string_view b) {
    if (a.size() != b.size()) return false;
    for (size_t i = 0; i < a.size(); i++) {
        if (fold_ascii(a[i]) != fold_ascii(b[i])) return false;
    }
    return true;
}
inline uint64_t hs_fold_hash(std::string_view s) {
    uint64_t h = 1469598103934665603ull; // FNV-1a, case-folded
    for (char c : s) { h ^= (uint8_t)fold_ascii(c); h *= 1099511628211ull; }
    return h;
}

// A single header stored as zero-copy views into the connection buffer.
// `hkey` is the FNV-1a case-folded hash of `name` for O(1) lookups.
struct Hdr {
    uint64_t hkey = 0;
    std::string_view name;
    std::string_view value;
};

struct Request {
    std::string method;   // small copy (verb), fine
    std::string_view path;    // decoded path, no query — view into conn buffer
    std::string_view query;   // raw query string — view into conn buffer
    std::string_view body;    // request body — view into conn buffer
    std::vector<Hdr> headers; // lowercased-key lookup via fold hashing
    // ms2.6: param map is now view-based. Keys point into the route pattern
    // (process lifetime), values point into the request path — so dispatch and
    // handler entry cost no per-param heap allocation.
    std::map<std::string_view, std::string_view> params;
    // ms1.6: zero-copy param views filled by the router (values point into
    // req.path, valid for the duration of dispatch).
    std::pair<std::string_view, std::string_view> pv[16] = {};
    size_t pv_n = 0;

    // Zero-copy param accessor: returns a view into the request path.
    std::string_view pview(const std::string_view name) const {
        for (size_t i = 0; i < pv_n; i++)
            if (pv[i].first == name) return pv[i].second;
        return {};
    }

    void clear_views() {
        path = {};
        query = {};
        body = {};
        headers.clear();
        params.clear();
    }
    void add_header(std::string_view n, std::string_view v) {
        headers.push_back({ hs_fold_hash(n), n, v });
    }
    std::string header(const std::string& name) const {
        uint64_t h = hs_fold_hash(name);
        for (auto& hd : headers) {
            if (hd.hkey == h && fold_eq(hd.name, name)) return std::string(hd.value);
        }
        return "";
    }
    bool header_eq(std::string_view name, std::string_view value) const {
        uint64_t h = hs_fold_hash(name);
        for (auto& hd : headers) {
            if (hd.hkey == h && fold_eq(hd.name, name)) return fold_eq(hd.value, value);
        }
        return false;
    }
    std::string q(const std::string& key) const {
        std::string_view qq = query;
        std::string out;
        while (true) {
            size_t amp = qq.find('&');
            std::string_view pair = qq.substr(0, amp == std::string_view::npos ? qq.size() : amp);
            size_t eqpos = pair.find('=');
            std::string_view k = pair.substr(0, eqpos);
            if (k == key) {
                std::string_view v = eqpos == std::string_view::npos ? std::string_view{} : pair.substr(eqpos + 1);
                out.reserve(v.size());
                for (size_t j = 0; j < v.size(); j++) {
                    if (v[j] == '+' ) out += ' ';
                    else if (v[j] == '%' && j + 2 < v.size()) {
                        auto hv = [](char c) {
                            if (c >= '0' && c <= '9') return c - '0';
                            if (c >= 'a' && c <= 'f') return c - 'a' + 10;
                            if (c >= 'A' && c <= 'F') return c - 'A' + 10;
                            return 0;
                        };
                        out += (char)((hv(v[j + 1]) << 4) | hv(v[j + 2]));
                        j += 2;
                    } else out += v[j];
                }
                return out;
            }
            if (amp == std::string_view::npos) break;
            qq.remove_prefix(amp + 1);
        }
        return "";
    }
    // ms2.6: the request body is parsed once, into a value-engine Value
    // (SSO strings, small-vector containers). `json()` keeps the legacy Val
    // signature as a compatibility adapter over the cached engine value.
    Value json_value() const;
    Val json() const {
        if (body.empty()) return Val::object({});
        return val_from_value(json_value());
    }
    std::string form(const std::string& name) const {
        Request tmp;
        tmp.query = body;
        return tmp.q(name);
    }

private:
    // ms2.6: memoized engine parse of the request body (lazy, once per
    // request — repeated json()/json_value() calls never re-parse).
    mutable Value json_engine_;
    mutable bool json_parsed_ = false;
};

// Lazy, memoized engine-Value parse of the request body. The empty body
// yields an empty object (matching the legacy json() contract). Defined
// after the Request struct so it can touch the cache members.
inline Value Request::json_value() const {
    if (!json_parsed_) {
        json_engine_ = body.empty() ? Value::obj() : parse_json_value(body);
        json_parsed_ = true;
    }
    return json_engine_;
}

struct Response {
    int status = 200;
    std::string ctype = "text/plain; charset=utf-8";
    std::string body;
    Val json_v;               // JSON body kept as a value when has_json is set
    bool has_json = false;
    std::vector<std::pair<std::string, std::string>> headers;
    Response() = default;
    static Response json(const Val& v) {
        Response r;
        r.ctype = "application/json; charset=utf-8";
        r.json_v = v;
        r.has_json = true;
        return r;
    }
    static Response json(Val&& v) {
        Response r;
        r.ctype = "application/json; charset=utf-8";
        r.json_v = std::move(v);
        r.has_json = true;
        return r;
    }
    static Response text(const std::string& s) {
        Response r;
        r.body = s;
        return r;
    }
    static Response html(const std::string& s) {
        Response r;
        r.ctype = "text/html; charset=utf-8";
        r.body = s;
        return r;
    }
    static Response empty(int status = 204) {
        Response r;
        r.status = status;
        return r;
    }
    static Response error(int status, const std::string& msg) {
        Response r;
        r.status = status;
        r.ctype = "application/json; charset=utf-8";
        r.body = "{\"error\":\"" + msg + "\"}";
        return r;
    }
};

using Handler = std::function<Response(const Request&)>;
using Middleware = std::function<Response(const Request&, const std::function<Response()>& next)>;

// split a path into at most `cap` segments, zero copies (views into `p`).
// returns segment count, or (size_t)-1 when the path is too deep for `cap`.
static size_t split_sv(std::string_view p, std::string_view* out, size_t cap) {
    size_t n = 0;
    size_t i = 0;
    while (true) {
        while (i < p.size() && p[i] == '/') i++;
        if (i >= p.size()) break;
        size_t st = i;
        while (i < p.size() && p[i] != '/') i++;
        if (n >= cap) return (size_t)-1;
        out[n++] = p.substr(st, i - st);
    }
    return n;
}

// ===========================================================================
// ms1.6 router: open-addressing static table + literal/param/wildcard trie.
// Dispatch is read-only and zero-allocation: static hits are O(1), trie walks
// are O(depth) with bounded backtracking (literal > param > wildcard).
// ===========================================================================

enum Method : uint8_t { M_ANY = 0, M_GET = 1, M_POST = 2, M_PUT = 3, M_DELETE = 4,
                        M_PATCH = 5, M_HEAD = 6, M_OPTIONS = 7 };

static uint8_t method_id(const char* m, size_t n) {
    if (n == 3 && m[0] == 'G' && m[1] == 'E' && m[2] == 'T') return M_GET;
    if (n == 4 && m[0] == 'P' && m[1] == 'O' && m[2] == 'S' && m[3] == 'T') return M_POST;
    if (n == 3 && m[0] == 'P' && m[1] == 'U' && m[2] == 'T') return M_PUT;
    if (n == 6 && m[0] == 'D' && m[1] == 'E' && m[2] == 'L' && m[3] == 'E' &&
        m[4] == 'T' && m[5] == 'E') return M_DELETE;
    if (n == 5 && m[0] == 'P' && m[1] == 'A' && m[2] == 'T' && m[3] == 'C' &&
        m[4] == 'H') return M_PATCH;
    if (n == 4 && m[0] == 'H' && m[1] == 'E' && m[2] == 'A' && m[3] == 'D') return M_HEAD;
    if (n == 7 && m[0] == 'O' && m[1] == 'P' && m[2] == 'T' && m[3] == 'I' &&
        m[4] == 'O' && m[5] == 'N' && m[6] == 'S') return M_OPTIONS;
    return M_ANY;
}

static uint64_t fnv1a(uint8_t mid, std::string_view s) {
    uint64_t h = 0xcbf29ce484222325ull;
    h = (h ^ (uint64_t)mid) * 0x100000001b3ull;
    for (unsigned char c : s) h = (h ^ (uint64_t)c) * 0x100000001b3ull;
    return h;
}

struct Route {
    std::string method;
    std::string path; // "/users/:id"
    uint8_t m_id = M_ANY;
    Handler handler;
};

struct StaticSlot {
    uint64_t hash = 0;
    int32_t route = -1; // <0 == empty slot
};

struct RouterNode {
    std::vector<std::pair<std::string, int>> lit; // literal edges, sorted by key
    int param = -1;                               // ':name' child (one segment)
    int wild = -1;                                // '*name' child (rest of path)
    std::string pname;                            // param/wild segment name, stored on child
    std::vector<int> term;                        // routes terminating here, registration order
};

struct Server {
    std::vector<Middleware> middlewares;
    std::vector<Route> routes;
    int port = 3000;

    // ms1.6 router tables (read-only during dispatch)
    std::vector<StaticSlot> stat; // open-addressing, power-of-two capacity
    size_t stat_cap = 0;
    size_t stat_n = 0;
    std::vector<RouterNode> nodes; // nodes[0] = root, built incrementally in handle()

    void before(Middleware m) { middlewares.push_back(std::move(m)); }

    void handle(const std::string& method, const std::string& path, Handler h) {
        uint8_t mid = method_id(method.data(), method.size());
        int idx = (int)routes.size();
        routes.push_back({ method, path, mid, std::move(h) });
        if (nodes.empty()) nodes.emplace_back(); // root
        if (path.find(':') == std::string::npos && path.find('*') == std::string::npos)
            insert_static();
        else
            insert_trie(idx);
    }

    void grow_static() {
        size_t ncap = stat_cap ? stat_cap * 2 : 16;
        std::vector<StaticSlot> nx(ncap);
        for (auto& s : stat) {
            if (s.route < 0) continue;
            size_t i = s.hash & (ncap - 1);
            while (nx[i].route >= 0) i = (i + 1) & (ncap - 1);
            nx[i] = s;
        }
        stat.swap(nx);
        stat_cap = ncap;
    }

    void insert_static() {
        const Route& r = routes.back();
        if (stat_cap == 0 || stat_n * 10 >= stat_cap * 7) grow_static(); // load <= 0.7
        uint64_t h = fnv1a(r.m_id, r.path);
        size_t i = h & (stat_cap - 1);
        for (;;) {
            StaticSlot& s = stat[i];
            if (s.route < 0) { s.hash = h; s.route = (int32_t)(routes.size() - 1); stat_n++; return; }
            const Route& old = routes[(size_t)s.route];
            if (s.hash == h && old.m_id == r.m_id && old.path == r.path) return; // first wins
            i = (i + 1) & (stat_cap - 1);
        }
    }

    int trie_ensure_literal(int node, std::string_view key) {
        RouterNode& n = nodes[(size_t)node];
        auto it = std::lower_bound(n.lit.begin(), n.lit.end(), key,
            [](const std::pair<std::string, int>& a, std::string_view b) { return std::string_view(a.first) < b; });
        if (it != n.lit.end() && std::string_view(it->first) == key) return it->second;
        std::string ks(key); // copy key before any realloc
        nodes.emplace_back();
        int child = (int)nodes.size() - 1;
        RouterNode& n2 = nodes[(size_t)node]; // re-fetch: emplace_back may have moved nodes
        auto it2 = std::lower_bound(n2.lit.begin(), n2.lit.end(), std::string_view(ks),
            [](const std::pair<std::string, int>& a, std::string_view b) { return std::string_view(a.first) < b; });
        n2.lit.insert(it2, { std::move(ks), child });
        return child;
    }

    void insert_trie(int idx) {
        const Route& r = routes[(size_t)idx];
        std::string_view segs[32];
        size_t gn = split_sv(r.path, segs, 32);
        if (gn == (size_t)-1 || gn == 0) return;
        int node = 0;
        for (size_t k = 0; k < gn; k++) {
            std::string_view s = segs[k];
            if (!s.empty() && s[0] == ':') {
                if (nodes[(size_t)node].param < 0) {
                    nodes.emplace_back();
                    nodes[(size_t)node].param = (int)nodes.size() - 1;
                }
                node = nodes[(size_t)node].param;
                nodes[(size_t)node].pname = std::string(s.substr(1));
            } else if (!s.empty() && s[0] == '*') {
                if (nodes[(size_t)node].wild < 0) {
                    nodes.emplace_back();
                    nodes[(size_t)node].wild = (int)nodes.size() - 1;
                }
                int wc = nodes[(size_t)node].wild;
                nodes[(size_t)wc].pname = std::string(s.substr(1));
                nodes[(size_t)wc].term.push_back(idx);
                return; // wildcard consumes the rest of the path
            } else {
                node = trie_ensure_literal(node, s);
            }
        }
        nodes[(size_t)node].term.push_back(idx);
    }

    // Depth-first trie walk with bounded backtracking. Precedence: literal >
    // param > wildcard, so /users/me beats /users/:id and both beat
    // /files/*path. Zero heap allocation: captured params are views either
    // into the request path (values) or the route pattern (names).
    bool trie_walk(int node, std::string_view* got, size_t gn, size_t k, uint8_t mid,
                   std::pair<std::string_view, std::string_view>* vp, size_t& vpn, int& out) const {
        if (k == gn) {
            for (int rt : nodes[(size_t)node].term) {
                const Route& r = routes[(size_t)rt];
                if (r.m_id == M_ANY || r.m_id == mid) { out = rt; return true; }
            }
            return false;
        }
        const RouterNode& n = nodes[(size_t)node];
        std::string_view seg = got[k];
        auto it = std::lower_bound(n.lit.begin(), n.lit.end(), seg,
            [](const std::pair<std::string, int>& a, std::string_view b) { return std::string_view(a.first) < b; });
        if (it != n.lit.end() && std::string_view(it->first) == seg) {
            if (trie_walk(it->second, got, gn, k + 1, mid, vp, vpn, out)) return true;
        }
        if (n.param >= 0 && vpn < 16) {
            const RouterNode& pc = nodes[(size_t)n.param];
            vp[vpn] = { std::string_view(pc.pname), seg };
            vpn++;
            if (trie_walk(n.param, got, gn, k + 1, mid, vp, vpn, out)) return true;
            vpn--;
        }
        if (n.wild >= 0) {
            const RouterNode& wc = nodes[(size_t)n.wild];
            const char* end = got[gn - 1].data() + got[gn - 1].size();
            std::string_view rest(seg.data(), (size_t)(end - seg.data()));
            while (!rest.empty() && rest.front() == '/') rest.remove_prefix(1);
            vp[vpn] = { std::string_view(wc.pname), rest };
            vpn++;
            for (int rt : wc.term) {
                const Route& r = routes[(size_t)rt];
                if (r.m_id == M_ANY || r.m_id == mid) { out = rt; return true; }
            }
            vpn--;
        }
        return false;
    }

    // returns index of matching route or -1. Zero heap allocation during
    // dispatch; zero-copy params (views). Static paths hit the open-addressing
    // table in O(1); parameterized/wildcard paths walk the trie in O(depth).
    int match(const std::string& method, std::string_view path, Request& req) const {
        uint8_t mid = method_id(method.data(), method.size());
        std::string_view got[32];
        size_t gn = split_sv(path, got, 32);
        if (gn == (size_t)-1) return -1;

        if (stat_cap) {
            uint64_t h = fnv1a(mid, path);
            size_t i = h & (stat_cap - 1);
            for (size_t probe = 0; probe <= stat_cap + 1; probe++) {
                const StaticSlot& s = stat[i];
                if (s.route < 0) break; // empty slot: not a static route
                if (s.hash == h) {
                    const Route& r = routes[(size_t)s.route];
                    if (r.m_id == mid && r.path == path) {
                        req.pv_n = 0;
                        return s.route;
                    }
                }
                i = (i + 1) & (stat_cap - 1);
            }
        }

        if (nodes.empty()) return -1;
        std::pair<std::string_view, std::string_view> vp[16];
        size_t vpn = 0;
        int ri = -1;
        if (trie_walk(0, got, gn, 0, mid, vp, vpn, ri)) {
            for (size_t k = 0; k < vpn; k++)
                req.params[vp[k].first] = vp[k].second;   // ms2.6: view-backed map, no copies
            req.pv_n = vpn;
            for (size_t k = 0; k < vpn; k++) req.pv[k] = vp[k];
            return ri;
        }
        return -1;
    }

    Response dispatch(Request& req) {
        try {
            if (middlewares.empty()) {
                int ri = match(req.method, req.path, req);
                if (ri < 0) return Response::error(404, "not found");
                try {
                    return routes[(size_t)ri].handler(req);
                } catch (const std::exception& e) {
                    return Response::error(500, std::string("internal error: ") + e.what());
                }
            }
            std::function<Response(size_t)> run = [&](size_t mi) -> Response {
                if (mi < middlewares.size()) {
                    Middleware& mw = middlewares[mi];
                    std::function<Response()> next = [&]() { return run(mi + 1); };
                    return mw(req, next);
                }
                int ri = match(req.method, req.path, req);
                if (ri < 0) return Response::error(404, "not found");
                try {
                    return routes[(size_t)ri].handler(req);
                } catch (const std::exception& e) {
                    return Response::error(500, std::string("internal error: ") + e.what());
                }
            };
            return run(0);
        } catch (const std::exception& e) {
            return Response::error(500, std::string("middleware error: ") + e.what());
        }
    }

    // in-process request (used by tests)
    Val call(const std::string& method, const std::string& raw_url, const Val& body) {
        Request req;
        size_t qpos = raw_url.find('?');
        req.path = std::string_view(raw_url).substr(0, qpos);
        req.query = qpos == std::string::npos ? std::string_view{} : std::string_view(raw_url).substr(qpos + 1);
        req.method = method;
        std::string body_str = body.is_str() ? body.sv : to_json(body);
        req.body = body_str;
        req.add_header("content-type", "application/json");
        Response r = dispatch(req);
        Val out = Val::object({});
        out.set("status", Val::int_(r.status));
        Val h = Val::object({});
        h.set("content-type", Val::text(r.ctype));
        out.set("headers", h);
        if (r.has_json) out.set("body", std::move(r.json_v));
        else if (!r.body.empty()) {
            try { out.set("body", val_from_value(parse_json_value(r.body))); }
            catch (...) { out.set("body", Val::text(r.body)); }
        }
        return out;
    }

    void listen();
    int test_main();
};

// ---- HTTP wire ----
// ms1.9: static const reason table (no per-call switch branch table data).
inline constexpr std::pair<int, const char*> kHsReasons[] = {
    {200, "OK"}, {201, "Created"}, {204, "No Content"}, {400, "Bad Request"},
    {401, "Unauthorized"}, {403, "Forbidden"}, {404, "Not Found"},
    {405, "Method Not Allowed"}, {418, "I'm a teapot"},
    {500, "Internal Server Error"}, {501, "Not Implemented"}, {503, "Service Unavailable"},
};
static std::string http_reason(int status) {
    for (auto& r : kHsReasons)
        if (r.first == status) return const_pool().intern(r.second);
    return const_pool().intern("Status");
}

static bool send_all(int fd, const char* data, size_t len) {
    bool ok = true;
    size_t off = 0;
    while (off < len) {
        ssize_t n = send(fd, data + off, len - off, MSG_NOSIGNAL);
        if (n > 0) { off += (size_t)n; continue; }
        if (n < 0) {
            if (errno == EINTR) continue;
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                struct pollfd p;
                memset(&p, 0, sizeof p);
                p.fd = fd;
                p.events = POLLOUT;
                if (poll(&p, 1, 500) > 0) continue;
            }
        }
        ok = false;
        break; // peer gone or persistently unreadable -> abandon
    }
    return ok;
}
static bool send_all(int fd, const std::string& data) {
    return send_all(fd, data.data(), data.size());
}

// Write the status line + headers straight to the socket, then the body:
// the response bytes are never glued into one contiguous copy, so a large
// body transfers in linear time with zero intermediate allocation. The head
// is serialized into an arena-backed buffer (request-scoped, reset after).
// JSON responses stream the serialized body into that same arena buffer
// (exact Content-Length via json_size beforehand), landing head + body in
// one buffer and one send().
static bool http_respond(int fd, const Response& r, bool keep, hs::Arena::Str& out) {
    out.reset();
    size_t cl = r.has_json ? json_size(r.json_v) : r.body.size();
    out.append("HTTP/1.1 ");
    json_append_int(out, (int64_t)r.status);
    out.append(' ');
    out.append(http_reason(r.status));
    out.append("\r\nContent-Type: ");
    out.append(r.ctype);
    out.append("\r\nContent-Length: ");
    json_append_int(out, (int64_t)cl);
    out.append("\r\nConnection: ");
    out.append((keep && g_hs_keepalive) ? "keep-alive" : "close");
    out.append("\r\nServer: hardscript/");
    out.append(HS_VERSION_STRING);
    out.append("\r\n");
    for (auto& h : r.headers) { out.append(h.first); out.append(": "); out.append(h.second); out.append("\r\n"); }
    out.append("\r\n");
    if (r.has_json) val_to_json(r.json_v, out); // body lands in the same buffer
    if (!send_all(fd, out.data, out.len)) return false;
    if (!r.has_json && !r.body.empty() && !send_all(fd, r.body.data(), r.body.size())) return false;
    return true;
}

// WebSocket frame send
static void ws_send(int fd, const std::string& payload, bool binary) {
    std::string f;
    f += (char)(0x80 | (binary ? 0x2 : 0x1));
    size_t len = payload.size();
    if (len < 126) {
        f += (char)len;
    } else if (len < 65536) {
        f += (char)126; f += (char)(len >> 8); f += (char)(len & 0xff);
    } else {
        f += (char)127;
        for (int i = 7; i >= 0; i--) f += (char)((uint64_t)len >> (i * 8));
    }
    f += payload;
    send_all(fd, f);
}

struct WsHandler {
    std::string path;
    std::function<void(const Request&)> on_open;
    std::function<void(const std::string&, bool binary)> on_msg;
    std::function<void()> on_close;
    std::function<void(const std::string&, const std::string&)> on_broadcast_room;
};

// global ws registry (single server)
static std::vector<WsHandler>& ws_registry() {
    static std::vector<WsHandler> r;
    return r;
}
static std::mutex& ws_bcast_mutex() {
    static std::mutex m;
    return m;
}
static std::map<std::string, int>& ws_clients() {
    static std::map<std::string, int> m;
    return m;
}
static std::map<std::string, std::set<std::string>>& ws_rooms() {
    static std::map<std::string, std::set<std::string>> m;
    return m;
}
static std::map<std::string, std::string>& ws_client_room() {
    static std::map<std::string, std::string> m;
    return m;
}

inline void ws_join(const std::string& client_id, const std::string& room) {
    std::lock_guard<std::mutex> lk(ws_bcast_mutex());
    ws_rooms()[room].insert(client_id);
    ws_client_room()[client_id] = room;
}
inline void ws_leave(const std::string& client_id) {
    std::lock_guard<std::mutex> lk(ws_bcast_mutex());
    auto it = ws_client_room().find(client_id);
    if (it == ws_client_room().end()) return;
    auto rit = ws_rooms().find(it->second);
    if (rit != ws_rooms().end()) {
        rit->second.erase(client_id);
        if (rit->second.empty()) ws_rooms().erase(rit);
    }
    ws_client_room().erase(it);
}

inline void ws_broadcast(const std::string& payload) {
    std::lock_guard<std::mutex> lk(ws_bcast_mutex());
    for (auto& kv : ws_clients()) ws_send(kv.second, payload, false);
}
inline void ws_broadcast_room(const std::string& room, const std::string& payload) {
    std::lock_guard<std::mutex> lk(ws_bcast_mutex());
    auto it = ws_rooms().find(room);
    if (it == ws_rooms().end()) return;
    for (auto& id : it->second) {
        auto c = ws_clients().find(id);
        if (c != ws_clients().end()) ws_send(c->second, payload, false);
    }
}

// parse a single ws frame from buffer at offset; returns consumed bytes or 0
static size_t ws_frame(const std::string& buf, size_t off, std::string& payload_out, bool handle_close, int fd) {
    if (off + 2 > buf.size()) return 0;
    uint8_t b0 = (uint8_t)buf[off];
    uint8_t b1 = (uint8_t)buf[off + 1];
    int opcode = b0 & 0x0f;
    bool fin = (b0 & 0x80) != 0;
    size_t len = b1 & 0x7f;
    bool masked = (b1 & 0x80) != 0;
    size_t pos = off + 2;
    if (len == 126) {
        if (pos + 2 > buf.size()) return 0;
        len = ((uint8_t)buf[pos] << 8) | (uint8_t)buf[pos + 1];
        pos += 2;
    } else if (len == 127) {
        if (pos + 8 > buf.size()) return 0;
        len = 0;
        for (int i = 0; i < 8; i++) len = (len << 8) | (uint8_t)buf[pos + i];
        pos += 8;
    }
    uint8_t mask[4] = { 0, 0, 0, 0 };
    if (masked) {
        if (pos + 4 > buf.size()) return 0;
        memcpy(mask, buf.data() + pos, 4);
        pos += 4;
    }
    if (pos + len > buf.size()) return 0;
    // masked payload is unmasked in place, reusing the connection buffer
    std::string_view payload(buf.data() + pos, len);
    payload_out.assign(payload);
    if (masked) {
        for (size_t i = 0; i < payload_out.size(); i++) payload_out[i] ^= mask[i % 4];
    }
    size_t consumed = (pos + len) - off;
    (void)fin;
    if (opcode == 0x8) { // close
        if (handle_close) {
            std::string close_frame;
            close_frame += (char)0x88;
            close_frame += (char)0;
            send_all(fd, close_frame);
        }
        payload_out.clear();
        return consumed | (1ULL << 63); // signal close via high bit
    } else if (opcode == 0x9) { // ping -> pong
        std::string pong;
        pong += (char)0x8A;
        pong += (char)payload_out.size();
        pong += payload_out;
        send_all(fd, pong);
        payload_out.clear();
    } else if (opcode == 0xA) { // pong
        payload_out.clear();
    } else { // 1 text, 2 binary
    }
    return consumed;
}

// thread-local identity for the connection currently being served
inline std::string& ws_cur() { static thread_local std::string s; return s; }
inline int& ws_cur_fd() { static thread_local int s = -1; return s; }
inline std::string ws_self() { return ws_cur(); }
inline void ws_reply(const std::string& payload) {
    int fd = ws_cur_fd();
    if (fd >= 0) ws_send(fd, payload, false);
}

// ---------------------------------------------------------------------------
// Request-level parser: grows `buf` until exactly one full request is buffered
// (read-block with poll; zero-copy; no per-header allocations).
// ---------------------------------------------------------------------------
static size_t hs_content_length(std::string_view head) {
    size_t p = 0;
    while (p < head.size()) {
        size_t e = head.find("\r\n", p);
        if (e == std::string_view::npos) e = head.size();
        std::string_view line = head.substr(p, e - p);
        size_t c = line.find(':');
        if (c != std::string_view::npos) {
            std::string_view k = line.substr(0, c);
            if (fold_eq(k, "content-length")) {
                std::string_view v = line.substr(c + 1);
                while (!v.empty() && v.front() <= ' ') v.remove_prefix(1);
                size_t cl = 0;
                for (char ch : v) {
                    if (ch < '0' || ch > '9') break;
                    cl = cl * 10 + (size_t)(ch - '0');
                }
                return cl;
            }
        }
        if (e == head.size()) break;
        p = e + 2;
    }
    return 0;
}

// Read until a complete request (headers + Content-Length body) is buffered.
// Returns false on EOF / error / oversized / idle timeout.
static bool hs_fill_request(int fd, std::string& buf, int idle_ms) {
    static thread_local char chunk[16384];
    for (;;) {
        size_t hend = buf.find("\r\n\r\n");
        if (hend != std::string::npos) {
            size_t cl = hs_content_length(std::string_view(buf.data(), hend));
            if (buf.size() >= hend + 4 + cl) return true;
        }
        struct pollfd pfd;
        memset(&pfd, 0, sizeof pfd);
        pfd.fd = fd;
        pfd.events = POLLIN;
        int pr = poll(&pfd, 1, idle_ms);
        if (pr <= 0) return false;
        ssize_t n = recv(fd, chunk, sizeof chunk, 0);
        if (n <= 0) return false;
        buf.append(chunk, (size_t)n);
        if (buf.size() > kMaxRequestBytes) return false;
    }
}

// Parse one request out of `buf` without copying header/query/body bytes.
// `consumed` = bytes of buf belonging to this request.
// `keep_alive` = whether the connection may be reused.
// On success returns true; on a malformed request line false.
static bool hs_parse_request(const std::string& buf, Request& req, size_t& consumed, bool& keep_alive) {
    size_t hend = buf.find("\r\n\r\n");
    if (hend == std::string::npos) { consumed = 0; keep_alive = false; return false; }
    size_t eol = buf.find("\r\n");
    if (eol == std::string::npos || eol > hend) eol = hend;
    std::string_view rl(buf.data(), eol);
    size_t sp1 = rl.find(' ');
    size_t sp2 = rl.rfind(' ');
    if (sp1 == std::string_view::npos || sp2 <= sp1) { consumed = 0; keep_alive = false; return false; }
    std::string_view target = rl.substr(sp1 + 1, sp2 - sp1 - 1);
    std::string_view proto = rl.substr(sp2 + 1);

    req.method.assign(rl.substr(0, sp1));
    size_t qpos = target.find('?');
    req.path = target.substr(0, qpos);
    req.query = qpos == std::string_view::npos ? std::string_view{} : target.substr(qpos + 1);

    size_t p = eol + 2;
    size_t cl = 0;
    while (p < hend) {
        size_t e = buf.find("\r\n", p);
        if (e == std::string::npos || e > hend) e = hend;
        std::string_view line(buf.data() + p, e - p);
        size_t c = line.find(':');
        if (c != std::string_view::npos) {
            std::string_view k = line.substr(0, c);
            std::string_view v = line.substr(c + 1);
            while (!v.empty() && v.front() == ' ') v.remove_prefix(1);
            req.add_header(k, v);
            if (fold_eq(k, "content-length")) {
                for (char ch : v) {
                    if (ch < '0' || ch > '9') break;
                    cl = cl * 10 + (size_t)(ch - '0');
                }
            }
        }
        p = e + 2;
    }

    req.body = cl ? std::string_view(buf.data() + hend + 4, cl)
                  : std::string_view{};
    consumed = hend + 4 + cl;
    // Persistent by default for HTTP/1.1; explicit Connection: close opts out.
    keep_alive = fold_eq(proto, "HTTP/1.1") && !fold_eq(req.header("connection"), "close");
    return true;
}

static void handle_connection(int fd, Server& srv, hs::Arena& arena, hs::Arena::Str& head) {
    ++g_hs_conn;
    struct ConnGuard { ~ConnGuard() { g_hs_conn.fetch_sub(1); } } conn_guard;
    arena.reset();
    head.reset();
    std::string buf;
    buf.reserve(4096);
    unsigned long long nreq = 0;
    for (;;) {
        if (!hs_fill_request(fd, buf, 5000)) break; // EOF / timeout / error
        Request req;
        size_t consumed = 0;
        bool keep = false;
        if (!hs_parse_request(buf, req, consumed, keep)) break;
        if (req.header_eq("upgrade", "websocket")) {
            // websocket upgrade
            for (auto& ws : ws_registry()) {
                if (ws.path != req.path) continue;
                std::string key = req.header("sec-websocket-key");
                std::string accept = base64_encode(sha1_bin(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"));
                std::string resp = "HTTP/1.1 101 Switching Protocols\r\n";
                resp += "Upgrade: websocket\r\n";
                resp += "Connection: Upgrade\r\n";
                resp += "Sec-WebSocket-Accept: " + accept + "\r\n\r\n";
                send_all(fd, resp);
                std::string client_id = random_hex(8);
                {
                    std::lock_guard<std::mutex> lk(ws_bcast_mutex());
                    ws_clients()[client_id] = fd;
                }
                ws_cur() = client_id;
                ws_cur_fd() = fd;
                if (ws.on_open) ws.on_open(req);
                // frame loop
                std::string acc;
                char wbuf[8192];
                bool open = true;
                while (open) {
                    struct pollfd wp;
                    memset(&wp, 0, sizeof wp);
                    wp.fd = fd;
                    wp.events = POLLIN;
                    int pr = poll(&wp, 1, 10000);
                    if (pr <= 0) break;
                    ssize_t n = recv(fd, wbuf, sizeof wbuf, 0);
                    if (n <= 0) break;
                    acc.append(wbuf, (size_t)n);
                    size_t base = 0;
                    std::string payload;
                    while (base < acc.size()) {
                        size_t r = ws_frame(acc, base, payload, true, fd);
                        if (r == 0) break;
                        if (r & (1ULL << 63)) { open = false; break; }
                        base += r;
                        if (ws.on_msg && !payload.empty()) ws.on_msg(payload, false);
                        payload.clear();
                    }
                    if (base > 0) acc.erase(0, base);
                }
                ws_leave(client_id);
                {
                    std::lock_guard<std::mutex> lk(ws_bcast_mutex());
                    ws_clients().erase(client_id);
                }
                ws_cur().clear();
                ws_cur_fd() = -1;
                if (ws.on_close) ws.on_close();
                close(fd);
                return;
            }
            // no ws route — fall through to 404
        }
        Response r;
        try {
            r = srv.dispatch(req);
        } catch (const std::exception& e) {
            r = Response::error(500, std::string("error: ") + e.what());
        }
        if (!http_respond(fd, r, keep, head)) break;
        arena.reset(); // request arena: reclaimed for the next request
        buf.erase(0, consumed); // drop the consumed request, keep pipelined tail
        ++nreq;
        if (g_hs_stop.load() || nreq >= kMaxRequestsPerConn || !g_hs_keepalive || !keep) break;
        // keep-alive: loop and read the next request on the same socket
    }
    close(fd);
}

// Pooled handler thread: the thread_local arena/Str live HERE, so one worker
// serves many connections and the bump allocator is reused across requests.
// ms1.9: the pool is lazy — after kIdleExitSeconds with an empty queue a
// surplus worker exits (RSS shrinks when idle); workers respawn on demand.
inline constexpr size_t kIdleFloor = 1;         // never shrink below this many live workers
inline constexpr unsigned kIdleExitSeconds = 5; // surplus workers idle this long exit
static void conn_worker(Server& srv) {
    thread_local hs::Arena arena;
    thread_local hs::Arena::Str head;
    struct Leave { ~Leave() { conn_pool().live.fetch_sub(1, std::memory_order_relaxed); } } leave;
    conn_pool().live.fetch_add(1, std::memory_order_relaxed);
    for (;;) {
        int fd;
        {
            std::unique_lock<std::mutex> lk(conn_pool().m);
            conn_pool().idle.fetch_add(1);
            conn_pool().cv.wait_for(lk, std::chrono::seconds(kIdleExitSeconds),
                                    [] { return conn_pool().stop.load() || !conn_pool().q.empty(); });
            conn_pool().idle.fetch_sub(1);
            if (conn_pool().q.empty()) {
                if (conn_pool().stop.load()) return;
                if (conn_pool().live.load() > kIdleFloor) return; // shrink idle pool
                continue;
            }
            fd = conn_pool().q.back();
            conn_pool().q.pop_back();
        }
        handle_connection(fd, srv, arena, head);
    }
}

inline void Server::listen() {
    int sfd = socket(AF_INET, SOCK_STREAM, 0);
    if (sfd < 0) throw std::runtime_error("cannot create socket");
    int one = 1;
    setsockopt(sfd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons((uint16_t)port);
    a.sin_addr.s_addr = htonl(INADDR_ANY);
    if (bind(sfd, (struct sockaddr*)&a, sizeof a) != 0) {
        close(sfd);
        throw std::runtime_error("cannot bind to port " + std::to_string(port));
    }
    if (::listen(sfd, 512) != 0) {
        close(sfd);
        throw std::runtime_error("cannot listen on port " + std::to_string(port));
    }
    conn_pool().stop.store(false);
    conn_pool().q.clear();
    conn_pool().idle.store(0);
    conn_pool().workers.reserve(8);
    unsigned nw = std::thread::hardware_concurrency();
    (void)nw;
#ifdef __GLIBC__
    // ms1.9 memory: cap malloc arenas (idle threads share few heaps) and give
    // the allocator a low mmap/trim threshold so large per-worker response
    // buffers (>64 KB) are returned to the OS when their worker exits idle.
    mallopt(M_ARENA_MAX, 32);
    mallopt(M_MMAP_THRESHOLD, 64 * 1024);
    mallopt(M_TRIM_THRESHOLD, 64 * 1024);
    mallopt(M_TOP_PAD, 16 * 1024);
#endif
    // ms1.9 memory: pool is lazy - climb from zero workers on first requests
    // and shrink to kIdleFloor after sustained idle (live thread exit returns
    // stacks + threaded heaps to the OS). Keeps idle RSS near the runtime
    // baseline; perf is unaffected (dispatch rate matches the eager pool).
    printf("HardScript server listening on http://127.0.0.1:%d (pool: lazy, max %d workers)\n", port, (int)kMaxWorkers);
    fflush(stdout);
    hs_install_shutdown_signals();
    for (;;) {
        struct pollfd pfd;
        memset(&pfd, 0, sizeof pfd);
        pfd.fd = sfd;
        pfd.events = POLLIN;
        int pr = poll(&pfd, 1, 250); // periodic wake so shutdown can break
        if (pr == 0) {
            if (g_hs_stop.load()) break;
            continue;
        }
        if (pr < 0) {
            if (errno == EINTR) {
                if (g_hs_stop.load()) break;
                continue;
            }
            break;
        }
        struct sockaddr_in ca;
        socklen_t clen = sizeof ca;
        int cfd = accept(sfd, (struct sockaddr*)&ca, &clen);
        if (cfd < 0) {
            if (errno == EINTR) continue;
            break;
        }
        int no = 1;
        setsockopt(cfd, IPPROTO_TCP, TCP_NODELAY, &no, sizeof no); // avoid Nagle stalls on keep-alive
        {
            std::unique_lock<std::mutex> lk(conn_pool().m);
            conn_pool().cv.wait(lk, [] { return conn_pool().stop.load() || conn_pool().q.size() < conn_pool().q_cap; });
            conn_pool().q.push_back(cfd);
            // Grow the pool when every worker is busy (keep-alive connections
            // pin a worker for their lifetime; spawn until we have slack).
            if (conn_pool().idle.load() == 0 && conn_pool().workers.size() < kMaxWorkers)
                conn_pool().workers.emplace_back(conn_worker, std::ref(*this));
        }
        conn_pool().cv.notify_one();
    }
    close(sfd);
    conn_pool().stop.store(true);
    conn_pool().cv.notify_all();
    for (int i = 0; i < 100 && g_hs_conn.load() > 0; ++i)
        std::this_thread::sleep_for(std::chrono::milliseconds(50));
    for (auto& w : conn_pool().workers) if (w.joinable()) w.join();
    conn_pool().workers.clear();
}

#endif