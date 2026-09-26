// mock_pg.hpp -- a scriptable PostgreSQL wire server for tests and benches
//
// Shared with qa/orm/pgsql.cpp, which grew it: the benchmark harness speaks
// to the same bytes the backend tests assert on. A mock is not PostgreSQL;
// it is a socket that answers Parse/Bind/Describe/Execute with whatever rows
// the script names, which is exactly what makes client framing, field counts
// and NULLs observable.
#ifndef HS_QA_MOCK_PG_HPP
#define HS_QA_MOCK_PG_HPP

#include <atomic>
#include <cstring>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include <arpa/inet.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/socket.h>
#include <unistd.h>

namespace qa {

// ---------------------------------------------------------------------------
// What the server should answer
// ---------------------------------------------------------------------------

struct PgCell {
    bool is_null = false;
    std::string text;
    PgCell() = default;
    PgCell(const char* t) : text(t) {}
    PgCell(const std::string& t) : text(t) {}
    static PgCell null() {
        PgCell c;
        c.is_null = true;
        return c;
    }
};

struct PgColumn {
    std::string name;
    uint32_t oid = 25;  // text
    PgColumn() = default;
    PgColumn(const std::string& n, uint32_t o) : name(n), oid(o) {}
};

struct PgReply {
    /// Substring that identifies this reply among the statements received.
    std::string match;
    std::vector<PgColumn> columns;
    std::vector<std::vector<PgCell>> rows;
    /// The tag to report. Empty means "work it out": one row per row sent.
    std::string tag;
    int64_t affected = -1;
    /// When set, the statement fails with this message and SQLSTATE.
    std::string error;
    std::string error_code = "42601";
    std::string error_detail;
    std::string error_hint;
    /// Transaction status reported with ReadyForQuery after this statement.
    char ready = 'I';
    /// Answer this statement once, then fall through to the next entry.
    /// Without it two identical statements get the same answer, which is right
    /// for a query and wrong for a contended write: the second attempt at a
    /// lock is the interesting one.
    bool once = false;
};

/// One statement as the server received it: the text and the values that came
/// with it, decoded back out of the Bind message. Asserting on these is
/// asserting on what the database would have seen.
struct PgSeen {
    std::string sql;
    /// Each bound value, with `is_null` telling NULL from the empty string.
    std::vector<PgCell> params;
};

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

class MockPg {
  public:
    explicit MockPg(const std::vector<PgReply>& replies)
        : replies_(replies), used_(replies.size(), false) {
        int one = 1;
        listen_fd_ = socket(AF_INET, SOCK_STREAM, 0);
        if (listen_fd_ < 0) throw std::runtime_error("mock pg: socket");
        setsockopt(listen_fd_, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
        struct sockaddr_in a;
        memset(&a, 0, sizeof a);
        a.sin_family = AF_INET;
        a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        a.sin_port = 0;  // any free port, so a test run cannot collide
        if (::bind(listen_fd_, (struct sockaddr*)&a, sizeof a) != 0) throw std::runtime_error("mock pg: bind");
        if (listen(listen_fd_, 4) != 0) throw std::runtime_error("mock pg: listen");
        socklen_t len = sizeof a;
        getsockname(listen_fd_, (struct sockaddr*)&a, &len);
        port_ = ntohs(a.sin_port);
        worker_ = std::thread([this] { serve(); });
    }

    ~MockPg() {
        stop();
        if (worker_.joinable()) worker_.join();
    }

    std::string conninfo() const { return "host=127.0.0.1 port=" + std::to_string(port_) + " user=tester dbname=qa"; }
    int port() const { return port_; }

    std::vector<PgSeen> seen() {
        std::lock_guard<std::mutex> lock(mu_);
        return seen_;
    }
    /// One entry per Execute message, in order. `statements()` collapses a
    /// batch to its Parse; this keeps every bound set.
    std::vector<PgSeen> executions() {
        std::lock_guard<std::mutex> lock(mu_);
        return executions_;
    }
    std::vector<PgSeen> statements() {
        std::lock_guard<std::mutex> lock(mu_);
        std::vector<PgSeen> out;
        for (const auto& s : seen_)
            if (s.sql != "BEGIN" && s.sql != "COMMIT" && s.sql != "ROLLBACK") out.push_back(s);
        return out;
    }
    size_t count(const std::string& sql) {
        std::lock_guard<std::mutex> lock(mu_);
        size_t n = 0;
        for (const auto& s : seen_)
            if (s.sql == sql) n++;
        return n;
    }
    void clear() {
        std::lock_guard<std::mutex> lock(mu_);
        seen_.clear();
    }
    /// The startup message the server was given, as it was sent.
    /// Hang up mid-conversation, the way a restarted server does.
    ///
    /// Only `shutdown`, never `close`: the thread reading this socket is
    /// blocked in `recv` with that descriptor, and closing it under another
    /// thread is a race with the kernel, not with a mutex. The shutdown wakes
    /// the read, the thread closes what it owns and exits.
    void drop_client() {
        int fd = client_fd_.load();
        if (fd < 0) return;
        ::shutdown(fd, SHUT_RDWR);
    }

    std::string startup_user() { return startup_user_; }
    std::string startup_db() { return startup_db_; }
    int auth_requests() const { return auth_requests_; }
    std::string md5_response() { return md5_response_; }
    std::string startup_password() { return startup_password_; }

  private:
    // -- framing ----------------------------------------------------------
    static void put_i32(std::string& s, int32_t v) {
        s += (char)(uint8_t)((uint32_t)v >> 24);
        s += (char)(uint8_t)((uint32_t)v >> 16);
        s += (char)(uint8_t)((uint32_t)v >> 8);
        s += (char)(uint8_t)(uint32_t)v;
    }
    static void put_i16(std::string& s, int16_t v) {
        s += (char)(uint8_t)((uint16_t)v >> 8);
        s += (char)(uint8_t)(uint16_t)v;
    }
    void send(int fd, char type, const std::string& body) {
        std::string out;
        out += type;
        put_i32(out, (int32_t)body.size() + 4);
        out += body;
        if (::send(fd, out.data(), out.size(), 0) != (ssize_t)out.size())
            throw std::runtime_error("mock pg: short write");
    }

    /// Read one message into `body`, or return 0 at end of stream.
    char recv_msg(int fd, std::string& body) {
        char hdr[5];
        if (::recv(fd, hdr, 5, MSG_WAITALL) != 5) return 0;
        uint32_t len = (uint8_t)hdr[1] << 24 | (uint8_t)hdr[2] << 16 | (uint8_t)hdr[3] << 8 | (uint8_t)hdr[4];
        body.assign(len - 4, '\0');
        size_t got = 0;
        while (got < body.size()) {
            ssize_t n = ::recv(fd, &body[got], body.size() - got, MSG_WAITALL);
            if (n <= 0) return 0;
            got += (size_t)n;
        }
        return hdr[0];
    }

    struct Cursor {
        std::string s;
        size_t pos = 0;
        uint8_t u8() { return (uint8_t)s[pos++]; }
        int32_t i32() {
            int32_t v = (uint8_t)s[pos] << 24 | (uint8_t)s[pos + 1] << 16 | (uint8_t)s[pos + 2] << 8 | (uint8_t)s[pos + 3];
            pos += 4;
            return v;
        }
        int16_t i16() {
            int16_t v = (int16_t)((uint8_t)s[pos] << 8 | (uint8_t)s[pos + 1]);
            pos += 2;
            return v;
        }
        std::string cstr() {
            size_t end = s.find('\0', pos);
            std::string v = s.substr(pos, end - pos);
            pos = end + 1;
            return v;
        }
        std::string take(size_t n) {
            std::string v = s.substr(pos, n);
            pos += n;
            return v;
        }
        bool done() const { return pos >= s.size(); }
    };

    // -- handshake --------------------------------------------------------
    void serve() {
        // One client after another: a test (or bench) may open several
        // connections in sequence, and the second handshake must not wait on
        // a thread that already went home after the first goodbye.
        for (;;) {
            int fd = ::accept(listen_fd_, nullptr, nullptr);
            if (fd < 0) return;  // stopped: the listening socket is gone
            if (stopped_.load()) {
                ::close(fd);
                return;
            }
            // Same reason as the client: small ping-pong messages and no
            // Nagle, or loopback timings measure the 40ms delayed-ACK timer.
            int one = 1;
            ::setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
            client_fd_.store(fd);
            try {
                startup(fd);
                loop(fd);
            } catch (const std::exception& e) {
                if (server_error_.empty()) server_error_ = e.what();
            }
            // Whatever happened, this thread is the one that closes the
            // socket it accepted.
            client_fd_.store(-1);
            ::close(fd);
        }
    }

    void startup(int fd) {
        char hdr[4];
        if (::recv(fd, hdr, 4, MSG_WAITALL) != 4) throw std::runtime_error("mock pg: no startup");
        uint32_t len = (uint8_t)hdr[0] << 24 | (uint8_t)hdr[1] << 16 | (uint8_t)hdr[2] << 8 | (uint8_t)hdr[3];
        std::string body(len - 4, '\0');
        size_t got = 0;
        while (got < body.size()) {
            ssize_t n = ::recv(fd, &body[got], body.size() - got, MSG_WAITALL);
            if (n <= 0) throw std::runtime_error("mock pg: short startup");
            got += (size_t)n;
        }
        Cursor c{body};
        c.i32();  // protocol version
        while (!c.done()) {
            std::string k = c.cstr();
            if (k.empty()) break;
            std::string v = c.cstr();
            if (k == "user") startup_user_ = v;
            if (k == "database") startup_db_ = v;
            if (k == "password") startup_password_ = v;
        }

        if (auth_mode == "cleartext") {
            std::string m;
            put_i32(m, 3);
            send(fd, 'R', m);
            std::string body2;
            if (recv_msg(fd, body2) != 'p') throw std::runtime_error("mock pg: expected a password message");
            auth_requests_++;
        } else if (auth_mode == "md5") {
            std::string m;
            put_i32(m, 5);
            m += "salt";
            send(fd, 'R', m);
            std::string body2;
            if (recv_msg(fd, body2) != 'p') throw std::runtime_error("mock pg: expected a password message");
            Cursor c2{body2};
            md5_response_ = c2.cstr();
            auth_requests_++;
        } else {
            std::string m;
            put_i32(m, 0);
            send(fd, 'R', m);
        }

        std::string s;
        s += "server_version";
        s += '\0';
        s += "15.0";
        s += '\0';
        send(fd, 'S', s);
        std::string k;
        put_i32(k, 4242);
        put_i32(k, 1);
        send(fd, 'K', k);
        std::string z;
        z += 'I';
        send(fd, 'Z', z);
    }

    // -- statements -------------------------------------------------------
    void loop(int fd) {
        std::string sql;
        std::vector<PgCell> params;
        for (;;) {
            std::string body;
            char typ = recv_msg(fd, body);
            if (typ == 0) return;   // the client hung up
            if (typ == 'X') return;  // Terminate
            if (typ == 'P') {
                Cursor c{body};
                c.cstr();  // statement name
                sql = c.cstr();
                int16_t ntypes = c.i16();
                for (int16_t i = 0; i < ntypes; i++) c.i32();
                {
                    std::lock_guard<std::mutex> lock(mu_);
                    seen_.push_back(PgSeen{sql, {}});
                }
                // ParseComplete, then the types the server inferred.
                send(fd, '1', "");
                std::string t;
                int nparams = count_params(sql);
                put_i16(t, (int16_t)nparams);
                for (int i = 0; i < nparams; i++) put_i32(t, 0);
                send(fd, 't', t);
            } else if (typ == 'B') {
                Cursor c{body};
                c.cstr();  // portal
                c.cstr();  // statement
                int16_t nformats = c.i16();
                for (int16_t i = 0; i < nformats; i++) c.i16();
                int16_t nparams = c.i16();
                params.clear();
                for (int16_t i = 0; i < nparams; i++) {
                    int32_t len = c.i32();
                    if (len < 0) {
                        params.push_back(PgCell::null());
                        continue;
                    }
                    params.push_back(PgCell(c.take((size_t)len)));
                }
                int16_t nresult = c.i16();
                for (int16_t i = 0; i < nresult; i++) c.i16();
                {
                    std::lock_guard<std::mutex> lock(mu_);
                    if (!seen_.empty()) seen_.back().params = params;
                }
                send(fd, '2', "");
            } else if (typ == 'D') {
                // A Describe only needs the row shape, so it must not spend a
                // one-shot reply: that belongs to the Execute that follows.
                const PgReply* r = find_reply(sql, /*consume=*/false);
                if (is_control(sql)) {
                    send(fd, 'n', "");
                    continue;
                }
                if (r && !r->columns.empty()) {
                    std::string d;
                    put_i16(d, (int16_t)r->columns.size());
                    for (const auto& c : r->columns) {
                        d += c.name;
                        d += '\0';
                        put_i32(d, 0);   // table oid
                        put_i16(d, 0);   // column number
                        put_i32(d, (int32_t)c.oid);
                        put_i16(d, -1);  // type size: variable
                        put_i32(d, -1);  // type modifier
                        put_i16(d, 0);   // text format
                    }
                    send(fd, 'T', d);
                } else {
                    send(fd, 'n', "");
                }
            } else if (typ == 'E') {
                {
                    // One entry per execution, not per parse: batched sets
                    // share a Parse but bind separately, and the test wants
                    // to see every set that crossed the socket.
                    std::lock_guard<std::mutex> lock(mu_);
                    executions_.push_back(PgSeen{sql, params});
                }
                execute(fd, sql, params);
            } else if (typ == 'S') {
                send(fd, 'Z', std::string(1, ready_));
            }
        }
    }

    void execute(int fd, const std::string& sql, const std::vector<PgCell>& params) {
        (void)params;
        // Transaction control is not a table a test has to script: a server
        // accepts it, and the status it then reports is the whole point of it.
        std::string trimmed = trim(sql);
        if (trimmed == "BEGIN" || trimmed == "START TRANSACTION") {
            ready_ = 'T';
            std::string c;
            c += "BEGIN";
            c += '\0';
            send(fd, 'C', c);
            return;
        }
        if (trimmed == "COMMIT") {
            ready_ = 'I';
            std::string c;
            c += "COMMIT";
            c += '\0';
            send(fd, 'C', c);
            return;
        }
        if (trimmed == "ROLLBACK") {
            ready_ = 'I';
            std::string c;
            c += "ROLLBACK";
            c += '\0';
            send(fd, 'C', c);
            return;
        }
        // Savepoints are control too: the mock tracks nothing, because the
        // client under test tracks everything. What matters on the wire is
        // the tag the server answers with.
        if (trimmed.compare(0, 9, "SAVEPOINT") == 0) {
            std::string c;
            c += "SAVEPOINT";
            c += '\0';
            send(fd, 'C', c);
            return;
        }
        if (trimmed.compare(0, 7, "RELEASE") == 0) {
            std::string c;
            c += "RELEASE";
            c += '\0';
            send(fd, 'C', c);
            return;
        }
        if (trimmed.compare(0, 11, "ROLLBACK TO") == 0) {
            ready_ = 'T';
            std::string c;
            c += "ROLLBACK";
            c += '\0';
            send(fd, 'C', c);
            return;
        }
        const PgReply* r = find_reply(sql, /*consume=*/true);
        // No script matched: a real server would answer this statement, and
        // returning an empty result instead would look like a query that
        // matched nothing, which is a different bug and a silent one.
        std::string err = r ? r->error
                            : "relation \"" + first_word(sql) + "\" does not exist (no script for it)";
        if (!err.empty()) {
            std::string e;
            e += 'S';
            e += "ERROR";
            e += '\0';
            e += 'V';
            e += "ERROR";
            e += '\0';
            e += 'C';
            e += (r ? r->error_code : std::string("42P01"));
            e += '\0';
            e += 'M';
            e += err;
            e += '\0';
            if (r && !r->error_detail.empty()) {
                e += 'D';
                e += r->error_detail;
                e += '\0';
            }
            if (r && !r->error_hint.empty()) {
                e += 'H';
                e += r->error_hint;
                e += '\0';
            }
            e += '\0';
            send(fd, 'E', e);
            if (r) ready_ = r->ready;
            return;
        }
        ready_ = r->ready;
        for (const auto& row : r->rows) {
            std::string d;
            put_i16(d, (int16_t)row.size());
            for (const auto& cell : row) {
                if (cell.is_null) {
                    put_i32(d, -1);
                    continue;
                }
                put_i32(d, (int32_t)cell.text.size());
                d += cell.text;
            }
            send(fd, 'D', d);
        }
        int64_t n = r->affected >= 0 ? r->affected : (int64_t)r->rows.size();
        std::string tag = r->tag.empty() ? "SELECT " + std::to_string(n) : r->tag;
        std::string c;
        c += tag;
        c += '\0';
        send(fd, 'C', c);
    }

    static bool is_control(const std::string& sql) {
        std::string t = trim(sql);
        return t == "BEGIN" || t == "COMMIT" || t == "ROLLBACK" || t == "START TRANSACTION" ||
               t.compare(0, 9, "SAVEPOINT") == 0 || t.compare(0, 7, "RELEASE") == 0 ||
               t.compare(0, 11, "ROLLBACK TO") == 0;
    }

    const PgReply* find_reply(const std::string& sql, bool consume) {
        for (size_t i = 0; i < replies_.size(); i++) {
            if (!replies_[i].match.empty() && sql.find(replies_[i].match) == std::string::npos)
                continue;
            if (replies_[i].once && used_[i]) continue;  // spent: try the next entry
            if (consume) used_[i] = true;
            return &replies_[i];
        }
        return nullptr;
    }

    static int count_params(const std::string& sql) {
        int maxn = 0;
        for (size_t i = 0; i + 1 < sql.size(); i++) {
            if (sql[i] != '$') continue;
            size_t j = i + 1;
            int n = 0;
            while (j < sql.size() && sql[j] >= '0' && sql[j] <= '9') {
                n = n * 10 + (sql[j] - '0');
                j++;
            }
            if (n > maxn) maxn = n;
        }
        return maxn;
    }

    static std::string trim(const std::string& s) {
        size_t a = 0, b = s.size();
        while (a < b && isspace((unsigned char)s[a])) a++;
        while (b > a && isspace((unsigned char)s[b - 1])) b--;
        return s.substr(a, b - a);
    }

    static std::string first_word(const std::string& sql) {
        size_t i = 0;
        while (i < sql.size() && isspace((unsigned char)sql[i])) i++;
        size_t j = i;
        while (j < sql.size() && !isspace((unsigned char)sql[j]) && sql[j] != '(') j++;
        return sql.substr(i, j - i);
    }

    void stop() {
        if (stopped_.exchange(true)) return;
        int lfd = listen_fd_.exchange(-1);
        if (lfd >= 0) {
            ::shutdown(lfd, SHUT_RDWR);
            ::close(lfd);
        }
        // The same rule as `drop_client`: the reading thread closes.
        int fd = client_fd_.load();
        if (fd >= 0) ::shutdown(fd, SHUT_RDWR);
    }

  public:
    /// How to answer the authentication request. Empty is AuthenticationOk.
    std::string auth_mode;
    std::string server_error() { return server_error_; }

  private:
    std::vector<PgReply> replies_;
    std::vector<bool> used_;
    std::vector<PgSeen> seen_;
    std::vector<PgSeen> executions_;
    std::mutex mu_;
    std::thread worker_;
    std::atomic<int> client_fd_{-1};
    // Atomic because `stop()` clears it from another thread while `serve()` is
    // blocked in `accept()` on it: a plain int here is a data race, and TSan
    // says so the first time a test shuts its server down.
    std::atomic<int> listen_fd_{-1};
    int port_ = 0;
    char ready_ = 'I';
    int auth_requests_ = 0;
    std::string startup_user_, startup_db_, startup_password_, md5_response_, server_error_;
    std::atomic<bool> stopped_{false};
};

}  // namespace qa

#endif  // HS_QA_MOCK_PG_HPP
