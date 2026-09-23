#ifndef HS_RUNTIME_HTTP_HPP
#define HS_RUNTIME_HTTP_HPP
#include "hs_runtime_value.hpp"
// ===========================================================================
// HTTP
// ===========================================================================
struct Request {
    std::string method;
    std::string path;    // decoded path, no query
    std::string query;
    std::string body;
    std::map<std::string, std::string> headers;  // lowercased keys
    std::map<std::string, std::string> params;

    std::string header(const std::string& name) const {
        std::string k = name;
        std::transform(k.begin(), k.end(), k.begin(), ::tolower);
        auto it = headers.find(k);
        return it == headers.end() ? "" : it->second;
    }
    std::string q(const std::string& key) const {
        std::string qq = query;
        std::string out;
        size_t i = 0;
        while (i <= qq.size()) {
            size_t amp = qq.find('&', i);
            std::string pair = qq.substr(i, amp == std::string::npos ? qq.size() - i : amp - i);
            size_t eqpos = pair.find('=');
            std::string k = pair.substr(0, eqpos);
            if (k == key) {
                std::string v = eqpos == std::string::npos ? "" : pair.substr(eqpos + 1);
                for (size_t j = 0; j < v.size(); j++) {
                    if (v[j] == '+' ) out += ' ';
                    else if (v[j] == '%' && j + 2 < v.size()) {
                        auto hv = [](char c) { if (c >= '0' && c <= '9') return c - '0'; if (c >= 'a' && c <= 'f') return c - 'a' + 10; if (c >= 'A' && c <= 'F') return c - 'A' + 10; return 0; };
                        out += (char)((hv(v[j + 1]) << 4) | hv(v[j + 2]));
                        j += 2;
                    } else out += v[j];
                }
                return out;
            }
            if (amp == std::string::npos) break;
            i = amp + 1;
        }
        return "";
    }
    Val json() const {
        if (body.empty()) return Val::object({});
        return parse_json(body);
    }
    std::string form(const std::string& name) const {
        Request tmp;
        tmp.query = body;
        return tmp.q(name);
    }
};

struct Response {
    int status = 200;
    std::string ctype = "text/plain; charset=utf-8";
    std::string body;
    std::vector<std::pair<std::string, std::string>> headers;
    Response() = default;
    static Response json(const Val& v) {
        Response r;
        r.ctype = "application/json; charset=utf-8";
        r.body = to_json(v);
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

struct Route {
    std::string method;
    std::string path; // "/users/:id"
    Handler handler;
};

struct Server {
    std::vector<Middleware> middlewares;
    std::vector<Route> routes;
    int port = 3000;

    void before(Middleware m) { middlewares.push_back(std::move(m)); }
    void handle(const std::string& method, const std::string& path, Handler h) {
        routes.push_back({ method, path, std::move(h) });
    }

    static std::vector<std::string> split(const std::string& p) {
        std::vector<std::string> out;
        std::string cur;
        for (char c : p) {
            if (c == '/') { if (!cur.empty()) { out.push_back(cur); cur.clear(); } }
            else cur += c;
        }
        if (!cur.empty()) out.push_back(cur);
        return out;
    }

    // returns index of matching route or -1
    int match(const std::string& method, const std::string& path, Request& req) const {
        std::vector<std::string> got = split(path);
        for (size_t i = 0; i < routes.size(); i++) {
            const Route& r = routes[i];
            if (r.method != method) continue;
            std::vector<std::string> want = split(r.path);
            if (want.size() != got.size()) continue;
            bool ok = true;
            std::map<std::string, std::string> params;
            for (size_t k = 0; k < want.size(); k++) {
                if (!want[k].empty() && want[k][0] == ':') params[want[k].substr(1)] = got[k];
                else if (want[k] != got[k]) { ok = false; break; }
            }
            if (ok) { req.params = std::move(params); return (int)i; }
        }
        return -1;
    }

    Response dispatch(Request& req) {
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
        try {
            return run(0);
        } catch (const std::exception& e) {
            return Response::error(500, std::string("middleware error: ") + e.what());
        }
    }

    // in-process request (used by tests)
    Val call(const std::string& method, const std::string& raw_url, const Val& body) {
        Request req;
        size_t qpos = raw_url.find('?');
        req.path = qpos == std::string::npos ? raw_url : raw_url.substr(0, qpos);
        req.query = qpos == std::string::npos ? "" : raw_url.substr(qpos + 1);
        req.method = method;
        req.headers["content-type"] = "application/json";
        req.body = body.is_str() ? body.sv : to_json(body);
        Response r = dispatch(req);
        Val out = Val::object({});
        out.set("status", Val::int_(r.status));
        Val h = Val::object({});
        h.set("content-type", Val::text(r.ctype));
        out.set("headers", h);
        if (!r.body.empty()) {
            try { out.set("body", parse_json(r.body)); }
            catch (...) { out.set("body", Val::text(r.body)); }
        }
        return out;
    }

    void listen();
    int test_main();
};

// ---- HTTP wire ----
static std::string http_reason(int status) {
    switch (status) {
        case 200: return "OK";
        case 201: return "Created";
        case 204: return "No Content";
        case 400: return "Bad Request";
        case 401: return "Unauthorized";
        case 403: return "Forbidden";
        case 404: return "Not Found";
        case 405: return "Method Not Allowed";
        case 418: return "I'm a teapot";
        case 500: return "Internal Server Error";
        case 501: return "Not Implemented";
        default: return "Status";
    }
}

static void send_all(int fd, const std::string& data) {
    size_t off = 0;
    while (off < data.size()) {
        ssize_t n = send(fd, data.data() + off, data.size() - off, MSG_NOSIGNAL);
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
        break; // peer gone or persistently unreadable -> abandon
    }
}

static Response http_respond(int fd, const Response& r) {
    std::string out = "HTTP/1.1 " + std::to_string(r.status) + " " + http_reason(r.status) + "\r\n";
    out += "Content-Type: " + r.ctype + "\r\n";
    out += "Content-Length: " + std::to_string(r.body.size()) + "\r\n";
    out += "Connection: close\r\n";
    out += "Server: hardscript/" + std::string(HS_VERSION_STRING) + "\r\n";
    for (auto& h : r.headers) out += h.first + ": " + h.second + "\r\n";
    out += "\r\n";
    out += r.body;
    send_all(fd, out);
    return r;
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
    std::string payload = buf.substr(pos, len);
    if (masked) {
        for (size_t i = 0; i < payload.size(); i++) payload[i] ^= mask[i % 4];
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
        pong += (char)payload.size();
        pong += payload;
        send_all(fd, pong);
        payload_out.clear();
    } else if (opcode == 0xA) { // pong
        payload_out.clear();
    } else { // 1 text, 2 binary
        payload_out = payload;
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

static void handle_connection(int fd, Server& srv) {
    std::string buf;
    char chunk[4096];
    struct pollfd pfd;
    memset(&pfd, 0, sizeof pfd);
    pfd.fd = fd;
    pfd.events = POLLIN;
    // read headers
    while (buf.find("\r\n\r\n") == std::string::npos) {
        int pr = poll(&pfd, 1, 5000);
        if (pr <= 0) break;
        ssize_t n = recv(fd, chunk, sizeof chunk, 0);
        if (n <= 0) break;
        buf.append(chunk, (size_t)n);
        if (buf.size() > 16 * 1024 * 1024) break;
    }
    size_t hend = buf.find("\r\n\r\n");
    if (hend == std::string::npos) { close(fd); return; }
    std::string head = buf.substr(0, hend);
    std::string body = buf.substr(hend + 4);
    // request line
    size_t eol = head.find("\r\n");
    std::string rl = eol == std::string::npos ? head : head.substr(0, eol);
    size_t sp1 = rl.find(' ');
    size_t sp2 = rl.rfind(' ');
    if (sp1 == std::string::npos || sp2 == sp1) { close(fd); return; }
    Request req;
    req.method = rl.substr(0, sp1);
    std::string target = rl.substr(sp1 + 1, sp2 - sp1 - 1);
    size_t qpos = target.find('?');
    req.path = qpos == std::string::npos ? target : target.substr(0, qpos);
    req.query = qpos == std::string::npos ? "" : target.substr(qpos + 1);
    // headers
    size_t p = eol == std::string::npos ? head.size() : eol + 2;
    size_t cl = 0;
    while (p < head.size()) {
        size_t e = head.find("\r\n", p);
        if (e == std::string::npos) e = head.size();
        std::string line = head.substr(p, e - p);
        size_t c = line.find(':');
        if (c != std::string::npos) {
            std::string k = line.substr(0, c);
            std::string v = line.substr(c + 1);
            while (!v.empty() && v[0] == ' ') v.erase(v.begin());
            while (!v.empty() && (v.back() == '\r' || v.back() == '\n')) v.pop_back();
            std::transform(k.begin(), k.end(), k.begin(), ::tolower);
            req.headers[k] = v;
            if (k == "content-length") cl = (size_t)strtoull(v.c_str(), nullptr, 10);
        }
        p = e + 2;
    }
    while (body.size() < cl) {
        ssize_t n = recv(fd, chunk, sizeof chunk, 0);
        if (n <= 0) break;
        body.append(chunk, (size_t)n);
    }
    if (req.headers["upgrade"] == "websocket") {
        // websocket upgrade
        for (auto& ws : ws_registry()) {
            if (ws.path != req.path) continue;
            std::string key = req.headers["sec-websocket-key"];
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
                    if (ws.on_msg && !(base == 0 && payload.empty())) {
                        if (!payload.empty() || true) {
                            if (ws.on_msg) ws.on_msg(payload, false);
                        }
                    }
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
    req.body = body;
    Response r;
    try {
        r = srv.dispatch(req);
    } catch (const std::exception& e) {
        r = Response::error(500, std::string("error: ") + e.what());
    }
    http_respond(fd, r);
    close(fd);
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
    printf("HardScript server listening on http://127.0.0.1:%d\n", port);
    fflush(stdout);
    for (;;) {
        struct sockaddr_in ca;
        socklen_t clen = sizeof ca;
        int cfd = accept(sfd, (struct sockaddr*)&ca, &clen);
        if (cfd < 0) {
            if (errno == EINTR) continue;
            break;
        }
        std::thread(handle_connection, cfd, std::ref(*this)).detach();
    }
    close(sfd);
}

#endif
