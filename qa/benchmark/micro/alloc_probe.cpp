// Allocation probe for the HardScript runtime (M1.2 arena metrics).
//
// Starts the real HTTP server (thread-per-connection) on :8091 with the same
// benchmark routes, drives N request/response exchanges over fresh TCP
// connections, then reports the runtime.stats counters:
//   runtime.stats.allocations   bump allocations served by the request arena
//   runtime.stats.arena_bytes   bytes reserved by request arenas
//
// Build:  g++ -std=c++17 -O2 -pthread alloc_probe.cpp -o alloc_probe
// Run:    ./alloc_probe 20000
#include "../../../runtime/hs_runtime.hpp"
#include <cstdio>
#include <cstring>
#include <netinet/in.h>
#include <sys/socket.h>
#include <arpa/inet.h>
#include <unistd.h>
#include <thread>
#include <chrono>
#include <csignal>

using namespace hs;

static Response r_root(const Request&) {
    return Response::json(Val::object({ {"ok", Val::boolean(true)} }));
}
static Response r_hello(const Request& req) {
    auto it = req.params.find("name");
    Val v = it != req.params.end() ? Val::text(it->second) : Val::nil();
    return Response::json(Val::object({ {"hello", v} }));
}
static Response r_echo(const Request& req) {
    return Response::json(Val::object({ {"echo", req.json()} }));
}

// One GET / over a fresh connection, read until the end of headers.
static void one_exchange() {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) return;
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons(8091);
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (connect(fd, (struct sockaddr*)&a, sizeof a) != 0) { close(fd); return; }
    const char* req = "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
    send(fd, req, strlen(req), MSG_NOSIGNAL);
    char buf[4096];
    size_t got = 0;
    while (got + 1 < sizeof buf) {
        ssize_t n = recv(fd, buf + got, sizeof buf - got, 0);
        if (n <= 0) break;
        got += (size_t)n;
        if (got >= 4) {
            bool hdr_end = false;
            for (size_t j = 0; j + 4 <= got; j++) {
                if (buf[j] == '\r' && buf[j + 1] == '\n' && buf[j + 2] == '\r' && buf[j + 3] == '\n') { hdr_end = true; break; }
            }
            if (hdr_end) break;
        }
    }
    close(fd);
}

int main(int argc, char** argv) {
    int N = argc > 1 ? atoi(argv[1]) : 20000;
    Server app;
    app.port = 8091;
    app.handle("GET", "/", r_root);
    app.handle("GET", "/hello/:name", r_hello);
    app.handle("POST", "/echo", r_echo);
    std::thread srv([&] { app.listen(); });
    std::this_thread::sleep_for(std::chrono::milliseconds(500));

    // warmup: server + allocator steady state
    for (int i = 0; i < 300; i++) one_exchange();

    Stats before = runtime_stats();
    for (int i = 0; i < N; i++) one_exchange();
    Stats after = runtime_stats();

    size_t alloc_delta = after.allocations - before.allocations;
    fprintf(stderr, "runtime.stats allocations_delta=%zu arena_bytes=%zu requests=%d alloc_per_req=%.3f\n",
            alloc_delta, after.arena_bytes, N,
            N ? (double)alloc_delta / (double)N : 0.0);

    // interrupt the accept loop: the flag is checked on the poll() wake-up.
    g_hs_stop.store(true);
    int d = socket(AF_INET, SOCK_STREAM, 0); // nudge a wake if mid-accept
    if (d >= 0) {
        struct sockaddr_in wa; memset(&wa, 0, sizeof wa);
        wa.sin_family = AF_INET; wa.sin_port = htons(8091); wa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        connect(d, (struct sockaddr*)&wa, sizeof wa);
        close(d);
    }
    srv.join();
    return 0;
}