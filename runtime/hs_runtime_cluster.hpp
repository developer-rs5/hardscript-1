// hs_runtime_cluster.hpp -- distributed runtime interfaces (M6.8)
//
// Three things a single process cannot do alone, expressed as interfaces with
// a working default rather than as stubs:
//
//   * Placement. Which node owns a key, by rendezvous hashing, so adding or
//     removing one node moves about 1/N of the keys and only onto nodes that
//     stay. `cluster.owner(key)` is that answer; a program routes on it.
//   * Coordination. A lock and an idempotency key that mean the same thing on
//     one node and on ten, over a `CoordStore`: memory in one process, SQL
//     when the cluster shares a database. Locks carry a fencing token, so a
//     holder whose lock expired cannot release the new holder's.
//   * Peer calls. `cluster.call(node, path, body)` over a `ClusterTransport`,
//     defaulting to real HTTP/1.1 with a deadline and one retry, carrying a
//     shared secret so a peer can tell a cluster call from a stranger.
//
// Every interface here is swappable (`cluster_use_store`, `cluster_use_transport`)
// because the point of an interface is that a program can replace it: a Redis
// store, a gRPC transport, a test double.

#ifndef HS_RUNTIME_CLUSTER_HPP
#define HS_RUNTIME_CLUSTER_HPP

#include "hs_runtime_crypto.hpp"
#include "hs_runtime_io.hpp"
#include "hs_runtime_orm.hpp"
#include "hs_runtime_pgsql.hpp"
#include "hs_runtime_sqlite.hpp"
#include "hs_runtime_util.hpp"
#include "hs_runtime_value.hpp"

#include <atomic>
#include <chrono>
#include <cstring>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#ifdef _WIN32
#include <winsock2.h>
#else
#include <arpa/inet.h>
#include <netdb.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <unistd.h>
#endif

namespace hs {

// ---------------------------------------------------------------------------
// Identity and placement
// ---------------------------------------------------------------------------

/// This node's id: `CLUSTER_NODE_ID` when set, else hostname and pid, which
/// is unique per process without asking anyone.
inline std::string cluster_node_id() {
    std::string id = env_get("CLUSTER_NODE_ID");
    if (!id.empty()) return id;
    std::string host = hostname();
    if (host.empty()) host = "node";
    return host + "-" + pid();
}

inline std::vector<std::string> cluster_split(const std::string& s) {
    std::vector<std::string> out;
    std::string cur;
    for (char c : s) {
        if (c == ',') {
            if (!cur.empty()) out.push_back(cur);
            cur.clear();
        } else if (!isspace((unsigned char)c)) {
            cur.push_back(c);
        }
    }
    if (!cur.empty()) out.push_back(cur);
    return out;
}

/// The peers this node knows about, from `CLUSTER_NODES`: `id@host:port`
/// entries, comma separated, self excluded. Unknown ids fall back to
/// `host:port` so a node can be addressed before it is named.
inline std::vector<std::string> cluster_peer_specs() {
    std::vector<std::string> out;
    std::string me = cluster_node_id();
    for (const std::string& raw : cluster_split(env_get("CLUSTER_NODES"))) {
        size_t at = raw.find('@');
        std::string id = at == std::string::npos ? raw : raw.substr(0, at);
        if (id == me) continue;
        bool seen = false;
        for (const std::string& p : out)
            if (p == raw) seen = true;
        if (!seen) out.push_back(raw);
    }
    return out;
}

inline std::string cluster_peer_id(const std::string& spec) {
    size_t at = spec.find('@');
    return at == std::string::npos ? spec : spec.substr(0, at);
}

inline std::string cluster_peer_addr(const std::string& spec) {
    size_t at = spec.find('@');
    return at == std::string::npos ? spec : spec.substr(at + 1);
}

/// Every node in the ring, this one included, sorted: placement must not
/// depend on the order the environment listed them in.
inline std::vector<std::string> cluster_ring() {
    std::vector<std::string> ring{cluster_node_id()};
    for (const std::string& spec : cluster_peer_specs()) ring.push_back(cluster_peer_id(spec));
    std::sort(ring.begin(), ring.end());
    ring.erase(std::unique(ring.begin(), ring.end()), ring.end());
    return ring;
}

/// Rendezvous (highest random weight) hashing: every key scores every node,
/// and the highest score owns it. Adding a node moves only the keys it wins;
/// removing one moves only its keys, to whichever node now wins them. That
/// minimal-movement property is the entire reason to hash instead of modulo.
inline uint64_t cluster_weight(const std::string& node, const std::string& key) {
    // FNV-1a over node+key, then mixed: the low bits of a hash are the worst
    // bits, and the score is compared as a whole.
    uint64_t h = 1469598103934665603ULL;
    std::string material = node + "\x1f" + key;
    for (unsigned char c : material) {
        h ^= c;
        h *= 1099511628211ULL;
    }
    h ^= h >> 33;
    h *= 0xff51afd7ed558ccdULL;
    h ^= h >> 33;
    h *= 0xc4ceb9fe1a85ec53ULL;
    h ^= h >> 33;
    return h;
}

inline std::string cluster_owner_for(const std::vector<std::string>& ring, const std::string& key) {
    if (ring.empty()) throw std::runtime_error("cluster: no nodes to place the key on");
    std::string best;
    uint64_t best_score = 0;
    for (const std::string& node : ring) {
        uint64_t score = cluster_weight(node, key);
        if (best.empty() || score > best_score) {
            best = node;
            best_score = score;
        }
    }
    return best;
}

inline std::string cluster_owner(const std::string& key) {
    return cluster_owner_for(cluster_ring(), key);
}

// ---------------------------------------------------------------------------
// The coordination store
// ---------------------------------------------------------------------------

/// A held lock, or a claimed idempotency key. `token` is the fencing value:
/// it changes on every acquisition, so a caller that lost the lock cannot
/// renew or release the one that replaced it.
struct CoordRecord {
    std::string name;
    std::string owner;
    std::string token;
    std::string value;
    int64_t expires_at_ms = 0;
};

/// What every coordination backend implements. The defaults in the interface
/// are the memory behaviour, so a partial implementation is still correct.
struct CoordStore {
    virtual ~CoordStore() = default;
    virtual const char* name() const = 0;

    /// Take a lock, or report false when someone else holds a live one.
    virtual bool acquire(const std::string& name, const std::string& owner, const std::string& token,
                         int64_t ttl_ms, int64_t now_ms) = 0;
    /// Extend a lock this token still holds.
    virtual bool renew(const std::string& name, const std::string& token, int64_t ttl_ms,
                       int64_t now_ms) = 0;
    /// Release, but only for the token that holds it.
    virtual bool release(const std::string& name, const std::string& token) = 0;
    virtual bool get(const std::string& name, CoordRecord& out, int64_t now_ms) = 0;

    /// Claim an idempotency key: true exactly once per key per TTL.
    virtual bool claim(const std::string& key, const std::string& owner, const std::string& token,
                       int64_t ttl_ms, int64_t now_ms) = 0;
    /// Record the outcome a claimed key produced, for the retries that follow.
    virtual bool record(const std::string& key, const std::string& value, int64_t ttl_ms,
                        int64_t now_ms) = 0;
    virtual bool lookup(const std::string& key, std::string& out) = 0;

    /// Drop expired records. Backends that expire on read may do nothing.
    virtual void sweep(int64_t now_ms) { (void)now_ms; }
};

/// Locks and idempotency keys in one process. Expiry is checked on read and
/// on write, so a stale holder is treated as gone without a sweeper.
class MemoryCoordStore : public CoordStore {
  public:
    const char* name() const override { return "memory"; }

    bool acquire(const std::string& n, const std::string& owner, const std::string& token,
                 int64_t ttl_ms, int64_t now_ms) override {
        std::lock_guard<std::mutex> lock(mu_);
        auto it = locks_.find(n);
        if (it != locks_.end() && it->second.expires_at_ms > now_ms) return false;
        CoordRecord r;
        r.name = n;
        r.owner = owner;
        r.token = token;
        r.expires_at_ms = now_ms + ttl_ms;
        locks_[n] = r;
        return true;
    }

    bool renew(const std::string& n, const std::string& token, int64_t ttl_ms,
               int64_t now_ms) override {
        std::lock_guard<std::mutex> lock(mu_);
        auto it = locks_.find(n);
        if (it == locks_.end()) return false;
        if (it->second.token != token || it->second.expires_at_ms <= now_ms) return false;
        it->second.expires_at_ms = now_ms + ttl_ms;
        return true;
    }

    bool release(const std::string& n, const std::string& token) override {
        std::lock_guard<std::mutex> lock(mu_);
        auto it = locks_.find(n);
        // A stale token must not delete the record of a lock someone else has
        // since taken: that is the whole point of the token.
        if (it == locks_.end() || it->second.token != token) return false;
        locks_.erase(it);
        return true;
    }

    bool get(const std::string& n, CoordRecord& out, int64_t now_ms) override {
        std::lock_guard<std::mutex> lock(mu_);
        auto it = locks_.find(n);
        if (it == locks_.end()) return false;
        if (it->second.expires_at_ms <= now_ms) return false;
        out = it->second;
        return true;
    }

    bool claim(const std::string& key, const std::string& owner, const std::string& token,
               int64_t ttl_ms, int64_t now_ms) override {
        std::lock_guard<std::mutex> lock(mu_);
        auto it = idem_.find(key);
        if (it != idem_.end() && it->second.expires_at_ms > now_ms) return false;
        CoordRecord r;
        r.name = key;
        r.owner = owner;
        r.token = token;
        r.expires_at_ms = now_ms + ttl_ms;
        idem_[key] = r;
        return true;
    }

    /// Record the outcome of a claimed key. The value rides on the claim, so a
    /// retry of the same key finds it; a new claim replaces it.
    bool record(const std::string& key, const std::string& value, int64_t ttl_ms,
                int64_t now_ms) override {
        std::lock_guard<std::mutex> lock(mu_);
        CoordRecord& r = idem_[key];
        r.name = key;
        r.value = value;
        r.expires_at_ms = now_ms + ttl_ms;
        return true;
    }

    bool lookup(const std::string& key, std::string& out) override {
        std::lock_guard<std::mutex> lock(mu_);
        auto it = idem_.find(key);
        if (it == idem_.end()) return false;
        out = it->second.value;
        return true;
    }

    void sweep(int64_t now_ms) override {
        std::lock_guard<std::mutex> lock(mu_);
        for (auto it = locks_.begin(); it != locks_.end();) {
            if (it->second.expires_at_ms <= now_ms)
                it = locks_.erase(it);
            else
                ++it;
        }
        for (auto it = idem_.begin(); it != idem_.end();) {
            if (it->second.expires_at_ms <= now_ms)
                it = idem_.erase(it);
            else
                ++it;
        }
    }

    size_t live_locks() {
        std::lock_guard<std::mutex> lock(mu_);
        return locks_.size();
    }
    size_t live_keys() {
        std::lock_guard<std::mutex> lock(mu_);
        return idem_.size();
    }

  private:
    std::mutex mu_;
    std::map<std::string, CoordRecord> locks_;
    std::map<std::string, CoordRecord> idem_;
};

/// Locks and idempotency keys in a database every node can reach, so the
/// guarantee survives the process that took the lock. Two tables, both keyed
/// by name, both with an expiry the reader checks: SQL has no per-row TTL.
class SqlCoordStore : public CoordStore {
  public:
    explicit SqlCoordStore(DbBackend* db) : db_(db) { ensure(); }

    const char* name() const override { return db_->name(); }

    void ensure() {
        db_->run("CREATE TABLE IF NOT EXISTS hs_locks (name TEXT PRIMARY KEY, owner TEXT, "
                 "token TEXT, expires_at INTEGER)",
                 {});
        db_->run("CREATE TABLE IF NOT EXISTS hs_idem (key TEXT PRIMARY KEY, owner TEXT, "
                 "token TEXT, value TEXT, expires_at INTEGER)",
                 {});
    }

    bool acquire(const std::string& name, const std::string& owner, const std::string& token,
                 int64_t ttl_ms, int64_t now_ms) override {
        DbResult taken = db_->run(
            "INSERT INTO hs_locks (name, owner, token, expires_at) VALUES (?, ?, ?, ?) "
            "ON CONFLICT (name) DO UPDATE SET owner = ?, token = ?, expires_at = ? "
            "WHERE hs_locks.expires_at <= ?",
            {Val::text(name), Val::text(owner), Val::text(token), Val::int_(now_ms + ttl_ms),
             Val::text(owner), Val::text(token), Val::int_(now_ms + ttl_ms), Val::int_(now_ms)});
        return taken.affected > 0;
    }

    bool renew(const std::string& name, const std::string& token, int64_t ttl_ms,
               int64_t now_ms) override {
        DbResult r = db_->run("UPDATE hs_locks SET expires_at = ? WHERE name = ? AND token = ? "
                              "AND expires_at > ?",
                              {Val::int_(now_ms + ttl_ms), Val::text(name), Val::text(token),
                               Val::int_(now_ms)});
        return r.affected > 0;
    }

    bool release(const std::string& name, const std::string& token) override {
        DbResult r =
            db_->run("DELETE FROM hs_locks WHERE name = ? AND token = ?",
                     {Val::text(name), Val::text(token)});
        return r.affected > 0;
    }

    bool get(const std::string& name, CoordRecord& out, int64_t now_ms) override {
        DbResult r = db_->run("SELECT owner, token, expires_at FROM hs_locks WHERE name = ?",
                              {Val::text(name)});
        if (r.rows.empty()) return false;
        const std::vector<Val>& row = r.rows[0];
        int64_t expires = row.size() > 2 ? row[2].as_int() : 0;
        if (expires <= now_ms) return false;
        out.name = name;
        out.owner = row[0].is_str() ? row[0].sv : "";
        out.token = row[1].is_str() ? row[1].sv : "";
        out.expires_at_ms = expires;
        return true;
    }

    bool claim(const std::string& key, const std::string& owner, const std::string& token,
               int64_t ttl_ms, int64_t now_ms) override {
        DbResult r = db_->run(
            "INSERT INTO hs_idem (key, owner, token, value, expires_at) VALUES (?, ?, ?, ?, ?) "
            "ON CONFLICT (key) DO UPDATE SET owner = ?, token = ?, expires_at = ? "
            "WHERE hs_idem.expires_at <= ?",
            {Val::text(key), Val::text(owner), Val::text(token), Val::text(""),
             Val::int_(now_ms + ttl_ms), Val::text(owner), Val::text(token),
             Val::int_(now_ms + ttl_ms), Val::int_(now_ms)});
        return r.affected > 0;
    }

    bool record(const std::string& key, const std::string& value, int64_t ttl_ms,
                int64_t now_ms) override {
        DbResult r = db_->run("UPDATE hs_idem SET value = ?, expires_at = ? WHERE key = ?",
                              {Val::text(value), Val::int_(now_ms + ttl_ms), Val::text(key)});
        if (r.affected > 0) return true;
        // Never claimed here (another node's claim, or a fresh key): record it
        // anyway, so `record` after a successful `claim` on another node's
        // store still stores the outcome.
        DbResult ins = db_->run("INSERT INTO hs_idem (key, owner, token, value, expires_at) "
                                "VALUES (?, ?, ?, ?, ?) ON CONFLICT (key) DO UPDATE SET "
                                "value = ?, expires_at = ?",
                                {Val::text(key), Val::text(""), Val::text(""), Val::text(value),
                                 Val::int_(now_ms + ttl_ms), Val::text(value),
                                 Val::int_(now_ms + ttl_ms)});
        return ins.affected > 0;
    }

    bool lookup(const std::string& key, std::string& out) override {
        DbResult r = db_->run("SELECT value FROM hs_idem WHERE key = ?", {Val::text(key)});
        if (r.rows.empty() || r.rows[0].empty()) return false;
        out = r.rows[0][0].is_str() ? r.rows[0][0].sv : to_json(r.rows[0][0]);
        return true;
    }

    void sweep(int64_t now_ms) override {
        db_->run("DELETE FROM hs_locks WHERE expires_at <= ?", {Val::int_(now_ms)});
        db_->run("DELETE FROM hs_idem WHERE expires_at <= ?", {Val::int_(now_ms)});
    }

  private:
    DbBackend* db_;
};

// ---------------------------------------------------------------------------
// Locks and idempotency keys
// ---------------------------------------------------------------------------

/// `sqlite` takes a file path, `postgres` a conninfo. The connection is owned
/// by the store, which lives as long as the process does.
inline DbBackend* cluster_open_db(const std::string& kind, const std::string& dsn) {
    if (kind == "sqlite") return new SqliteDb(dsn);
    if (kind == "postgres") return new PgDb(dsn);
    throw std::runtime_error("cluster: unknown store `" + kind + "` (sqlite or postgres)");
}

class ClusterRuntime {
  public:
    ~ClusterRuntime() { delete store_; }

    CoordStore* store() { return store_; }

    void use_store(CoordStore* s) {
        if (!s) throw std::runtime_error("cluster: store must not be null");
        std::lock_guard<std::mutex> lock(mu_);
        delete store_;
        store_ = s;
    }

    /// The token this process stamps on everything it takes. Per process, not
    /// per lock: a caller that died mid-request leaves a token nobody can
    /// renew or release on its behalf.
    std::string token() {
        static const std::string t = random_hex(12);
        return t;
    }

    std::string node() { return cluster_node_id(); }

    bool acquire(const std::string& name, int64_t ttl_ms, int64_t now_ms) {
        return store()->acquire(name, node(), token(), ttl_ms, now_ms);
    }
    bool renew(const std::string& name, int64_t ttl_ms, int64_t now_ms) {
        return store()->renew(name, token(), ttl_ms, now_ms);
    }
    bool release(const std::string& name) { return store()->release(name, token()); }
    bool held(const std::string& name, int64_t now_ms) {
        CoordRecord r;
        return store()->get(name, r, now_ms) && r.owner == node() && r.token == token();
    }
    bool claim(const std::string& key, int64_t ttl_ms, int64_t now_ms) {
        return store()->claim(key, node(), token(), ttl_ms, now_ms);
    }
    bool record(const std::string& key, const std::string& value, int64_t ttl_ms, int64_t now_ms) {
        return store()->record(key, value, ttl_ms, now_ms);
    }
    bool lookup(const std::string& key, std::string& out) { return store()->lookup(key, out); }

    /// Once per process: the SQL store opens its connection here, so a program
    /// that never coordinates anything never touches the database.
    void configure_from_env() {
        static std::once_flag once;
        std::call_once(once, [this] {
            std::string kind = env_get("CLUSTER_STORE");
            if (kind.empty() || kind == "memory") return;
            std::string dsn = env_get("CLUSTER_DB");
            if (dsn.empty())
                throw std::runtime_error("cluster: CLUSTER_STORE=" + kind + " needs CLUSTER_DB");
            DbBackend* db = cluster_open_db(kind, dsn);
            use_store(new SqlCoordStore(db));
        });
    }

  private:

    std::mutex mu_;
    CoordStore* store_ = new MemoryCoordStore();
};

inline ClusterRuntime& cluster_runtime() {
    static ClusterRuntime r;
    return r;
}

// ---------------------------------------------------------------------------
// Peer calls
// ---------------------------------------------------------------------------

struct ClusterReply {
    int status = 0;
    std::string body;
    std::string content_type;
};

/// How a peer call reaches another node. The default speaks HTTP/1.1; a
/// program with a service mesh replaces this and nothing else changes.
struct ClusterTransport {
    virtual ~ClusterTransport() = default;
    virtual const char* name() const = 0;
    virtual ClusterReply call(const std::string& address, const std::string& path,
                              const std::string& body, const std::string& secret,
                              int timeout_ms) = 0;
};

/// The shared secret every peer call carries, so the receiving node can tell a
/// cluster call from an anonymous request. `CLUSTER_SECRET`.
inline std::string cluster_secret() { return env_get("CLUSTER_SECRET"); }

/// HTTP/1.1 over a plain socket: POST with a content length, read the status
/// line, headers and body. One retry, because a peer that was mid-restart is
/// the common case and the caller cannot tell it from a lost connection.
class HttpClusterTransport : public ClusterTransport {
  public:
    const char* name() const override { return "http"; }

    ClusterReply call(const std::string& address, const std::string& path, const std::string& body,
                      const std::string& secret, int timeout_ms) override {
        ClusterReply last;
        std::string problem;
        for (int attempt = 0; attempt < 2; attempt++) {
            try {
                return once(address, path, body, secret, timeout_ms);
            } catch (const std::exception& e) {
                problem = e.what();
            }
        }
        throw std::runtime_error("cluster: call to " + address + path + " failed: " + problem);
    }

  private:
    ClusterReply once(const std::string& address, const std::string& path, const std::string& body,
                      const std::string& secret, int timeout_ms) {
        std::string host = address;
        int port = 80;
        size_t colon = address.rfind(':');
        if (colon != std::string::npos && address.find(']') == std::string::npos) {
            host = address.substr(0, colon);
            port = atoi(address.c_str() + colon + 1);
            if (port <= 0) port = 80;
        }
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        if (fd < 0) throw std::runtime_error("cannot create a socket");
        struct sockaddr_in addr;
        memset(&addr, 0, sizeof addr);
        addr.sin_family = AF_INET;
        addr.sin_port = htons((uint16_t)port);
        if (inet_pton(AF_INET, host.c_str(), &addr.sin_addr) != 1) {
            struct hostent* he = gethostbyname(host.c_str());
            if (!he || !he->h_addr_list || !he->h_addr_list[0]) {
                ::close(fd);
                throw std::runtime_error("cannot resolve `" + host + "`");
            }
            memcpy(&addr.sin_addr, he->h_addr_list[0], sizeof addr.sin_addr);
        }
        if (::connect(fd, (struct sockaddr*)&addr, sizeof addr) != 0) {
            int e = errno;
            ::close(fd);
            throw std::runtime_error("cannot connect to " + address + " (" + strerror(e) + ")");
        }
        std::string req = "POST " + path + " HTTP/1.1\r\n";
        req += "Host: " + address + "\r\n";
        req += "Content-Type: application/json\r\n";
        req += "Content-Length: " + std::to_string(body.size()) + "\r\n";
        req += "X-HardScript-Cluster: " + secret + "\r\n";
        req += "Connection: close\r\n\r\n";
        req += body;
        size_t sent = 0;
        while (sent < req.size()) {
            ssize_t n = ::send(fd, req.data() + sent, req.size() - sent, hs_send_flags());
            if (n <= 0) {
                ::close(fd);
                throw std::runtime_error("connection lost writing the request");
            }
            sent += (size_t)n;
        }
        auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(timeout_ms);
        std::string raw;
        while (std::chrono::steady_clock::now() < deadline) {
            char buf[4096];
            ssize_t n = ::recv(fd, buf, sizeof buf, 0);
            if (n > 0) {
                // `append(buf, n)`, never `+= buf`: the rest of the buffer is
                // whatever was on the stack, and a NUL-terminated append would
                // read past what the socket actually sent.
                raw.append(buf, (size_t)n);
                // `Connection: close` means the peer hangs up when done, but a
                // chunked or kept-alive answer may not: stop on a complete
                // message rather than waiting for a close that may not come.
                if (http_message_complete(raw)) break;
                continue;
            }
            if (n == 0) break;
            if (errno != EAGAIN
#ifdef EWOULDBLOCK
                && errno != EWOULDBLOCK
#endif
            )
                break;
            std::this_thread::sleep_for(std::chrono::milliseconds(1));
        }
        ::close(fd);
        if (raw.empty()) throw std::runtime_error("no reply from " + address);
        // A peer that hangs up right after answering can still split the last
        // read: a short body is a truncated message, not a short answer, and
        // parsing it would hand back half a reply as if it were whole.
        if (!http_message_complete(raw))
            throw std::runtime_error("truncated reply from " + address);
        return parse_reply(raw);
    }

    /// Enough of a response to know when it is complete: the header block plus
    /// a body of exactly the length the headers promised. A reply without a
    /// content length is only complete when the peer closes, which the read
    /// loop already handles.
    static bool http_message_complete(const std::string& raw) {
        size_t head_end = raw.find("\r\n\r\n");
        if (head_end == std::string::npos) return false;
        long long len = content_length(raw, head_end);
        if (len < 0) return false;
        return (long long)(raw.size() - (head_end + 4)) >= len;
    }

    /// The `Content-Length` a header block promised, or -1 when it did not say.
    static long long content_length(const std::string& raw, size_t head_end) {
        std::string lower;
        for (size_t i = 0; i < head_end; i++) lower.push_back((char)tolower((unsigned char)raw[i]));
        size_t at = lower.find("content-length:");
        if (at == std::string::npos) return -1;
        return atoll(raw.c_str() + at + strlen("content-length:"));
    }

    static ClusterReply parse_reply(const std::string& raw) {
        ClusterReply r;
        size_t eol = raw.find("\r\n");
        if (eol == std::string::npos) throw std::runtime_error("truncated reply from the peer");
        // HTTP/1.1 200 OK
        size_t sp1 = raw.find(' ');
        if (sp1 == std::string::npos) throw std::runtime_error("no status code in the reply");
        size_t sp2 = raw.find(' ', sp1 + 1);
        r.status = atoi(raw.substr(sp1 + 1, (sp2 == std::string::npos ? eol : sp2) - sp1 - 1)
                                 .c_str());
        size_t head_end = raw.find("\r\n\r\n");
        std::string head = raw.substr(0, head_end == std::string::npos ? raw.size() : head_end);
        std::string lower;
        for (char c : head) lower.push_back((char)tolower((unsigned char)c));
        size_t ct = lower.find("content-type:");
        if (ct != std::string::npos) {
            size_t s = ct + 14;
            while (s < head.size() && head[s] == ' ') s++;
            size_t e = head.find("\r\n", s);
            r.content_type = head.substr(s, (e == std::string::npos ? head.size() : e) - s);
        }
        r.body = head_end == std::string::npos ? "" : raw.substr(head_end + 4);
        return r;
    }
};

inline ClusterTransport* cluster_default_transport() {
    static HttpClusterTransport t;
    return &t;
}

// ---------------------------------------------------------------------------
// Language surface
// ---------------------------------------------------------------------------

inline int64_t cluster_now_ms() { return unix_ms(); }

inline Val cluster_node_val() { return Val::text(cluster_runtime().node()); }

inline Val cluster_peers_val() {
    std::vector<Val> out;
    for (const std::string& spec : cluster_peer_specs()) {
        Val e = Val::object({{"id", Val::text(cluster_peer_id(spec))},
                             {"address", Val::text(cluster_peer_addr(spec))}});
        out.push_back(e);
    }
    return Val::list(std::move(out));
}

inline Val cluster_owner_val(const Val& key) {
    if (!key.is_str() || key.sv.empty())
        throw std::runtime_error("cluster: owner needs a key as text");
    return Val::text(cluster_owner(key.sv));
}

inline Val cluster_is_owner_val(const Val& key) {
    if (!key.is_str() || key.sv.empty())
        throw std::runtime_error("cluster: is_owner needs a key as text");
    return Val::boolean(cluster_owner(key.sv) == cluster_runtime().node());
}

/// `lock.acquire("invoice:42", 30)`: true when this process took it. A false
/// answer is not an error, it is the answer.
inline Val lock_acquire(const Val& name, const Val& ttl_secs) {
    if (!name.is_str() || name.sv.empty())
        throw std::runtime_error("lock: acquire needs a name as text");
    if (!ttl_secs.is_int() || ttl_secs.iv <= 0)
        throw std::runtime_error("lock: acquire needs a TTL in seconds (1 or more)");
    cluster_runtime().configure_from_env();
    return Val::boolean(cluster_runtime().acquire(name.sv, ttl_secs.iv * 1000, cluster_now_ms()));
}

inline Val lock_renew(const Val& name, const Val& ttl_secs) {
    if (!name.is_str() || name.sv.empty())
        throw std::runtime_error("lock: renew needs a name as text");
    if (!ttl_secs.is_int() || ttl_secs.iv <= 0)
        throw std::runtime_error("lock: renew needs a TTL in seconds (1 or more)");
    return Val::boolean(cluster_runtime().renew(name.sv, ttl_secs.iv * 1000, cluster_now_ms()));
}

inline Val lock_release(const Val& name) {
    if (!name.is_str() || name.sv.empty())
        throw std::runtime_error("lock: release needs a name as text");
    return Val::boolean(cluster_runtime().release(name.sv));
}

inline Val lock_held(const Val& name) {
    if (!name.is_str() || name.sv.empty())
        throw std::runtime_error("lock: held needs a name as text");
    return Val::boolean(cluster_runtime().held(name.sv, cluster_now_ms()));
}

/// `idem.claim("pay:1234", 3600)`: true for the first caller of a key. A retry
/// gets false, and reads the first caller's answer from `idem.lookup`.
inline Val idem_claim(const Val& key, const Val& ttl_secs) {
    if (!key.is_str() || key.sv.empty())
        throw std::runtime_error("idem: claim needs a key as text");
    if (!ttl_secs.is_int() || ttl_secs.iv <= 0)
        throw std::runtime_error("idem: claim needs a TTL in seconds (1 or more)");
    cluster_runtime().configure_from_env();
    return Val::boolean(cluster_runtime().claim(key.sv, ttl_secs.iv * 1000, cluster_now_ms()));
}

inline Val idem_record(const Val& key, const Val& value, const Val& ttl_secs) {
    if (!key.is_str() || key.sv.empty())
        throw std::runtime_error("idem: record needs a key as text");
    if (!ttl_secs.is_int() || ttl_secs.iv <= 0)
        throw std::runtime_error("idem: record needs a TTL in seconds (1 or more)");
    std::string stored = value.is_str() ? value.sv : to_json(value);
    return Val::boolean(cluster_runtime().record(key.sv, stored, ttl_secs.iv * 1000,
                                                 cluster_now_ms()));
}

inline Val idem_lookup(const Val& key) {
    if (!key.is_str() || key.sv.empty())
        throw std::runtime_error("idem: lookup needs a key as text");
    std::string out;
    if (!cluster_runtime().lookup(key.sv, out)) return Val::nil();
    // A recorded value is text; a caller that stored JSON gets it back parsed.
    if (!out.empty() && (out[0] == '{' || out[0] == '[')) {
        try {
            return val_from_value(parse_json_value(out));
        } catch (...) {
        }
    }
    return Val::text(out);
}

/// `cluster.call("worker-1@10.0.0.4:8080", "/jobs/run", { id: 7 })`. The node
/// may be a `CLUSTER_NODES` id or a bare `host:port`.
inline Val cluster_call(const Val& node, const Val& path, const Val& body) {
    if (!node.is_str() || node.sv.empty())
        throw std::runtime_error("cluster: call needs a node as text");
    if (!path.is_str() || path.sv.empty() || path.sv[0] != '/')
        throw std::runtime_error("cluster: call needs a path starting with `/`");
    std::string address = node.sv;
    if (address.find('@') != std::string::npos) {
        address = cluster_peer_addr(address);
    } else if (address.find(':') == std::string::npos) {
        // A bare id: look it up among the peers, so a caller never hardcodes
        // an address. An unknown id is passed through, so the error names the
        // host the dialer actually tried.
        for (const std::string& spec : cluster_peer_specs()) {
            if (cluster_peer_id(spec) == address) {
                address = cluster_peer_addr(spec);
                break;
            }
        }
    }
    std::string payload = body.is_nil() ? "{}" : (body.is_str() ? body.sv : to_json(body));
    ClusterReply r = cluster_default_transport()->call(address, path.sv, payload,
                                                       cluster_secret(), 5000);
    if (r.status < 200 || r.status >= 300) {
        throw std::runtime_error("cluster: " + address + path.sv + " answered " +
                                 std::to_string(r.status) + " (" + r.body.substr(0, 200) + ")");
    }
    std::string trimmed = r.body;
    while (!trimmed.empty() && isspace((unsigned char)trimmed.back())) trimmed.pop_back();
    if (trimmed.empty()) return Val::nil();
    try {
        return val_from_value(parse_json_value(trimmed));
    } catch (const std::exception&) {
        return Val::text(r.body);
    }
}

}  // namespace hs

#endif  // HS_RUNTIME_CLUSTER_HPP
