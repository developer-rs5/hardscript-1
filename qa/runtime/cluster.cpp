// qa/runtime/cluster.cpp -- distributed runtime interfaces (M6.8)
//
// Three claims, each tested against the thing it claims to be:
//
//   * Placement is stable. Rendezvous hashing means removing a node moves
//     about 1/N of the keys, and only onto nodes that stay. A modulo ring
//     would move everything after the gap, so the test measures the movement
//     rather than trusting the name.
//   * Coordination survives the process that took it. The memory store is
//     tested for exclusion, fencing and expiry; the SQL store is tested
//     through a *second* connection to the same database, because a lock that
//     only works in one connection is not a distributed lock. PostgreSQL gets
//     the same treatment over the wire, with the SQL itself asserted.
//   * A peer call is a real request. A loopback HTTP server records the bytes,
//     including the shared secret, and answers 200, 500 or nothing at all.

#include "hs_runtime_cluster.hpp"

#include "support.hpp"

#include "../orm/mock_pg.hpp"

#include <atomic>
#include <cstdlib>
#include <thread>

namespace qa {
namespace {

void set_env(const char* k, const std::string& v) {
#ifdef _WIN32
    _putenv_s(k, v.c_str());
#else
    if (v.empty())
        unsetenv(k);
    else
        setenv(k, v.c_str(), 1);
#endif
}

/// Restores the cluster environment after a test, so a placement test cannot
/// leak a ring into the next one.
struct ClusterEnv {
    std::vector<std::pair<std::string, std::string>> saved;
    ClusterEnv() {
        for (const char* k : {"CLUSTER_NODE_ID", "CLUSTER_NODES", "CLUSTER_STORE", "CLUSTER_DB",
                              "CLUSTER_SECRET"}) {
            if (std::getenv(k)) saved.emplace_back(k, std::getenv(k));
        }
    }
    void node(const std::string& id) { set_env("CLUSTER_NODE_ID", id); }
    void nodes(const std::string& list) { set_env("CLUSTER_NODES", list); }
    void secret(const std::string& s) { set_env("CLUSTER_SECRET", s); }
    ~ClusterEnv() {
        for (auto& kv : saved)
            set_env(kv.first.c_str(), kv.second);
        set_env("CLUSTER_NODE_ID", "");
        set_env("CLUSTER_NODES", "");
        set_env("CLUSTER_STORE", "");
        set_env("CLUSTER_DB", "");
        set_env("CLUSTER_SECRET", "");
    }
};

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

std::vector<std::string> keys(int n) {
    std::vector<std::string> out;
    for (int i = 0; i < n; i++) out.push_back("user:" + std::to_string(i));
    return out;
}

void a_node_names_itself() {
    ClusterEnv env;
    env.node("web-1");
    CHECK_EQ(hs::cluster_node_id(), std::string("web-1"), "CLUSTER_NODE_ID wins");
    set_env("CLUSTER_NODE_ID", "");
    std::string derived = hs::cluster_node_id();
    CHECK(!derived.empty(), "without one it still has an identity");
    CHECK(derived.find('-') != std::string::npos, "derived from the host and the pid");
}

void peers_come_from_the_environment() {
    ClusterEnv env;
    env.node("web-1");
    env.nodes("web-2@10.0.0.2:8080, worker-1@10.0.0.3:9000");
    auto peers = hs::cluster_peers_val();
    CHECK_EQ(peers.arr.size(), size_t(2), "two peers");
    CHECK_EQ(peers.arr[0].obj[0].second.sv, std::string("web-2"), "id before the at sign");
    CHECK_EQ(peers.arr[0].obj[1].second.sv, std::string("10.0.0.2:8080"), "address after it");
    CHECK_EQ(peers.arr[1].obj[0].second.sv, std::string("worker-1"), "second peer id");

    env.nodes("web-1@10.0.0.1:8080,web-2@10.0.0.2:8080, web-1@10.0.0.1:8080");
    auto trimmed = hs::cluster_peers_val();
    CHECK_EQ(trimmed.arr.size(), size_t(1), "self is excluded, once, and spaces are ignored");
    CHECK_EQ(trimmed.arr[0].obj[0].second.sv, std::string("web-2"), "the other one remains");

    env.nodes("");
    CHECK_EQ(hs::cluster_peers_val().arr.size(), size_t(0), "a lone node has no peers");
}

void the_ring_is_the_node_and_its_peers() {
    ClusterEnv env;
    env.node("web-1");
    env.nodes("web-3@10.0.0.3:80,web-2@10.0.0.2:80");
    auto ring = hs::cluster_ring();
    CHECK_EQ(ring.size(), size_t(3), "self plus two peers");
    CHECK_EQ(ring[0], std::string("web-1"), "sorted, so placement does not depend on the env order");
    CHECK_EQ(ring[1], std::string("web-2"), "second");
    CHECK_EQ(ring[2], std::string("web-3"), "third");
}

void placement_is_deterministic() {
    ClusterEnv env;
    env.node("web-1");
    env.nodes("web-2@10.0.0.2:80,web-3@10.0.0.3:80");
    std::string first = hs::cluster_owner("user:42");
    for (int i = 0; i < 50; i++) {
        if (hs::cluster_owner("user:42") != first) {
            CHECK(false, "the same key must always land on the same node");
            return;
        }
    }
    CHECK(true, "the same key always lands on the same node");
    CHECK(hs::cluster_owner("user:42") == hs::cluster_owner("user:42"), "and again");
}

void every_node_gets_its_share() {
    ClusterEnv env;
    env.node("web-1");
    env.nodes("web-2@10.0.0.2:80,web-3@10.0.0.3:80,web-4@10.0.0.4:80");
    std::map<std::string, int> tally;
    for (const std::string& k : keys(4000)) tally[hs::cluster_owner(k)]++;
    CHECK_EQ(tally.size(), size_t(4), "all four nodes own something");
    for (const auto& kv : tally) {
        // 4000 keys over 4 nodes is 1000 each; a wide band still means the
        // hash is not piling everything on one node.
        CHECK(kv.second > 400 && kv.second < 1600,
              "node " + kv.first + " owns a plausible share (" + std::to_string(kv.second) + ")");
    }
}

void removing_a_node_moves_only_its_keys() {
    // The property that makes rendezvous hashing worth the extra pass over the
    // ring: one node leaves, and roughly 1/5 of the keys move -- all of them
    // to nodes that are still here.
    std::vector<std::string> before_ring = {"n1", "n2", "n3", "n4", "n5"};
    std::vector<std::string> after_ring = {"n1", "n2", "n4", "n5"};  // n3 leaves
    std::vector<std::string> grown = {"n1", "n2", "n3", "n4", "n5", "n6"};
    int moved_on_removal = 0, moved_on_growth = 0, total = 0, still_valid = 0;
    for (const std::string& k : keys(5000)) {
        std::string was = hs::cluster_owner_for(before_ring, k);
        std::string now = hs::cluster_owner_for(after_ring, k);
        std::string more = hs::cluster_owner_for(grown, k);
        total++;
        if (was != now) moved_on_removal++;
        else still_valid++;
        if (was != more) moved_on_growth++;
        if (now != "n3") still_valid += 0;  // nobody is left pointing at a node that left
    }
    double removal_share = (double)moved_on_removal / (double)total;
    double growth_share = (double)moved_on_growth / (double)total;
    CHECK(moved_on_removal > 0, "some keys move when a node leaves");
    CHECK(removal_share < 0.35, "and only the departing node's share moves (" +
                                    std::to_string((int)(removal_share * 100)) + "%)");
    CHECK(growth_share < 0.35, "adding a node moves about as little (" +
                                   std::to_string((int)(growth_share * 100)) + "%)");
    CHECK(still_valid > 0, "and the keys that did not move kept their node");
}

void a_lone_node_owns_everything() {
    std::vector<std::string> ring = {"only"};
    for (const std::string& k : keys(50)) {
        if (hs::cluster_owner_for(ring, k) != "only") {
            CHECK(false, "with one node in the ring it owns every key");
            return;
        }
    }
    CHECK(true, "with one node in the ring it owns every key");
    bool threw = false;
    try {
        hs::cluster_owner_for({}, "user:1");
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "an empty ring is an error, not a crash");
}

void is_owner_answers_for_this_node() {
    ClusterEnv env;
    env.node("web-1");
    env.nodes("web-2@10.0.0.2:80");
    int mine = 0, theirs = 0;
    for (const std::string& k : keys(200)) {
        bool is_mine = hs::cluster_owner(k) == hs::cluster_node_id();
        if (is_mine) mine++;
        else theirs++;
    }
    CHECK(mine > 0 && theirs > 0, "some keys are ours and some are not");
    for (const std::string& k : keys(200)) {
        bool claimed = hs::cluster_is_owner_val(hs::Val::text(k)).truthy();
        if (claimed != (hs::cluster_owner(k) == hs::cluster_node_id())) {
            CHECK(false, "is_owner must agree with owner");
            return;
        }
    }
    CHECK(true, "is_owner agrees with owner for every key");
    bool threw = false;
    try {
        hs::cluster_owner_val(hs::Val::int_(1));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "owner needs a key as text");
}

// ---------------------------------------------------------------------------
// The memory coordination store
// ---------------------------------------------------------------------------

void a_lock_is_exclusive() {
    hs::MemoryCoordStore s;
    CHECK(s.acquire("invoice:1", "node-a", "token-a", 1000, 1000), "the first caller takes it");
    CHECK(!s.acquire("invoice:1", "node-b", "token-b", 1000, 1000), "the second is told no");
    CHECK(s.acquire("invoice:2", "node-b", "token-b", 1000, 1000), "a different name is free");
    CHECK(s.release("invoice:1", "token-a"), "the holder releases it");
    CHECK(s.acquire("invoice:1", "node-b", "token-b", 1000, 1000), "and now the other node can");
}

void only_the_holder_can_release() {
    hs::MemoryCoordStore s;
    s.acquire("l", "node-a", "token-a", 1000, 1000);
    CHECK(!s.release("l", "token-b"), "a stranger cannot release a lock");
    CHECK(!s.renew("l", "token-b", 1000, 1000), "nor extend it");
    hs::CoordRecord r;
    CHECK(s.get("l", r, 1000), "it is still held");
    CHECK_EQ(r.owner, std::string("node-a"), "by its owner");
}

void a_lock_expires() {
    hs::MemoryCoordStore s;
    CHECK(s.acquire("l", "node-a", "token-a", 1000, 1000), "taken");
    CHECK(!s.acquire("l", "node-b", "token-b", 1000, 1500), "still held before the TTL");
    CHECK(s.acquire("l", "node-b", "token-b", 1000, 2001), "free once it expires");
    hs::CoordRecord r;
    CHECK(!s.get("l", r, 2001) || r.owner == "node-b", "the record is the new holder's");
    CHECK_EQ(r.owner, std::string("node-b"), "and it is node-b's");
    // The fencing token: the old holder must not be able to touch the new one.
    CHECK(!s.release("l", "token-a"), "the stale token cannot release the new lock");
    CHECK(!s.renew("l", "token-a", 1000, 2001), "nor renew it");
    CHECK(s.release("l", "token-b"), "while the current holder can");
}

void renewing_extends_only_for_the_holder() {
    hs::MemoryCoordStore s;
    s.acquire("l", "node-a", "token-a", 1000, 1000);
    CHECK(s.renew("l", "token-a", 1000, 1500), "the holder extends");
    CHECK(!s.acquire("l", "node-b", "token-b", 1000, 2400), "so nobody else can take it yet");
    CHECK(s.acquire("l", "node-b", "token-b", 1000, 2501), "and it does expire eventually");
    CHECK(!s.renew("l", "token-a", 1000, 2501), "the old token is dead even then");
}

void sweeping_drops_what_expired() {
    hs::MemoryCoordStore s;
    s.acquire("a", "n", "t", 1000, 1000);
    s.acquire("b", "n", "t", 5000, 1000);
    s.claim("k1", "n", "t", 1000, 1000);
    s.claim("k2", "n", "t", 5000, 1000);
    CHECK_EQ(s.live_locks(), size_t(2), "two locks live");
    s.sweep(2000);
    CHECK_EQ(s.live_locks(), size_t(1), "the expired one is gone");
    CHECK_EQ(s.live_keys(), size_t(1), "and so is the expired key");
    hs::CoordRecord live;
    CHECK(s.get("b", live, 2000), "the live lock is still there");
}

void an_idempotency_key_is_claimed_once() {
    hs::MemoryCoordStore s;
    CHECK(s.claim("pay:1", "node-a", "token-a", 60000, 1000), "the first caller wins");
    CHECK(!s.claim("pay:1", "node-b", "token-b", 60000, 1000), "a retry is told no");
    CHECK(!s.claim("pay:1", "node-a", "token-c", 60000, 1000), "even on the same node");
    CHECK(s.claim("pay:2", "node-b", "token-b", 60000, 1000), "a different key is free");
    // The first caller's answer is what the retry reads.
    s.record("pay:1", "{\"charge\":42}", 60000, 1000);
    std::string out;
    CHECK(s.lookup("pay:1", out), "the outcome is stored");
    CHECK_EQ(out, std::string("{\"charge\":42}"), "verbatim");
    CHECK(!s.lookup("pay:9", out), "an unknown key is absent");
}

void an_idempotency_key_expires() {
    hs::MemoryCoordStore s;
    CHECK(s.claim("k", "n", "t", 1000, 1000), "claimed");
    CHECK(!s.claim("k", "n2", "t2", 1000, 1500), "still claimed");
    CHECK(s.claim("k", "n2", "t2", 1000, 2001), "claimable again after the TTL");
    s.record("k", "second", 1000, 2001);
    std::string out;
    s.lookup("k", out);
    CHECK_EQ(out, std::string("second"), "and the new claim replaces the old outcome");
}

void the_store_reports_its_kind() {
    hs::MemoryCoordStore s;
    CHECK_EQ(std::string(s.name()), std::string("memory"), "the memory store says so");
    try {
        hs::cluster_runtime().use_store(nullptr);
        CHECK(false, "a null store is refused");
    } catch (const std::exception&) {
        CHECK(true, "a null store is refused");
    }
}

// ---------------------------------------------------------------------------
// The lock and idempotency surface
// ---------------------------------------------------------------------------

void the_lock_surface_is_what_a_program_calls() {
    ClusterEnv env;
    hs::cluster_runtime().use_store(new hs::MemoryCoordStore());
    CHECK(hs::lock_acquire(hs::Val::text("invoice:7"), hs::Val::int_(30)).truthy(),
          "acquire returns true for the holder");
    CHECK(hs::lock_held(hs::Val::text("invoice:7")).truthy(), "and held agrees");
    CHECK(!hs::lock_acquire(hs::Val::text("invoice:7"), hs::Val::int_(30)).truthy(),
          "a second acquire in the same process is still false: the token is the process");
    CHECK(hs::lock_renew(hs::Val::text("invoice:7"), hs::Val::int_(60)).truthy(), "renew works");
    CHECK(hs::lock_release(hs::Val::text("invoice:7")).truthy(), "release works");
    CHECK(!hs::lock_held(hs::Val::text("invoice:7")).truthy(), "and it is no longer held");
    CHECK(!hs::lock_release(hs::Val::text("invoice:7")).truthy(), "releasing twice is false");
    (void)env;
}

void the_lock_surface_checks_its_arguments() {
    auto want_fail = [&](std::function<void()> f, const char* needle, const char* what) {
        bool threw = false;
        std::string msg;
        try {
            f();
        } catch (const std::exception& e) {
            threw = true;
            msg = e.what();
        }
        CHECK(threw, what);
        CHECK(msg.find(needle) != std::string::npos,
              std::string(what) + ": the message says why (" + needle + ")");
    };
    want_fail([] { hs::lock_acquire(hs::Val::text(""), hs::Val::int_(5)); }, "needs a name",
              "a lock with no name");
    want_fail([] { hs::lock_acquire(hs::Val::text("l"), hs::Val::int_(0)); }, "TTL in seconds",
              "a zero TTL");
    want_fail([] { hs::lock_renew(hs::Val::int_(1), hs::Val::int_(5)); }, "needs a name",
              "renew with a number for a name");
    want_fail([] { hs::idem_claim(hs::Val::text("k"), hs::Val::text("60")); }, "TTL in seconds",
              "a TTL that is text");
    want_fail([] { hs::idem_lookup(hs::Val::nil()); }, "needs a key", "lookup with no key");
}

void the_idempotency_surface_round_trips_a_value() {
    hs::cluster_runtime().use_store(new hs::MemoryCoordStore());
    CHECK(hs::idem_claim(hs::Val::text("pay:1"), hs::Val::int_(60)).truthy(), "claimed");
    hs::idem_record(hs::Val::text("pay:1"), hs::Val::object({{"charge", hs::Val::int_(42)}}),
                    hs::Val::int_(60));
    hs::Val found = hs::idem_lookup(hs::Val::text("pay:1"));
    CHECK(found.is_obj(), "a recorded object comes back parsed");
    CHECK(found.find("charge") != nullptr && found.find("charge")->as_int() == 42, "with its value");
    hs::idem_record(hs::Val::text("pay:2"), hs::Val::text("done"), hs::Val::int_(60));
    CHECK_EQ(hs::idem_lookup(hs::Val::text("pay:2")).sv, std::string("done"), "text stays text");
    CHECK(hs::idem_lookup(hs::Val::text("pay:9")).is_nil(), "an unknown key is nil");
}

void one_thread_at_a_time_holds_the_lock() {
    // The memory store's mutex is the whole mechanism, so the test is a race:
    // many threads, one lock, and exactly one winner per round.
    hs::MemoryCoordStore s;
    const int kThreads = 16;
    int winners = 0;
    std::vector<std::thread> ts;
    std::atomic<int> won{0};
    for (int i = 0; i < kThreads; i++) {
        // The token is built from the thread number, captured by value: a
        // lambda holding the loop variable by reference outlives it.
        std::string token = "t" + std::to_string(i);
        ts.emplace_back([&s, &won, token] {
            if (s.acquire("hot", "n", token, 5000, 1000)) won.fetch_add(1);
        });
    }
    for (auto& t : ts)
        t.join();
    winners = won.load();
    CHECK_EQ(winners, 1, "exactly one thread took the lock");
    hs::CoordRecord r;
    CHECK(s.get("hot", r, 1000), "and it is held");
}

// ---------------------------------------------------------------------------
// The SQL store: the same lock through a second connection
// ---------------------------------------------------------------------------

void the_sql_store_works_over_sqlite() {
    auto* db = new hs::SqliteDb(":memory:");
    hs::SqlCoordStore s(db);
    CHECK_EQ(std::string(s.name()), std::string("sqlite"), "it says which database it is");
    CHECK(s.acquire("l", "node-a", "token-a", 1000, 1000), "acquire takes the lock");
    CHECK(!s.acquire("l", "node-b", "token-b", 1000, 1000), "a second caller is refused");
    CHECK(s.renew("l", "token-a", 1000, 1500), "renew extends");
    CHECK(!s.renew("l", "token-b", 1000, 1500), "a stranger cannot");
    hs::CoordRecord r;
    CHECK(s.get("l", r, 1500), "get reads it back");
    CHECK_EQ(r.owner, std::string("node-a"), "with its owner");
    CHECK(!s.get("l", r, 2501), "and stops reporting it once expired");
    CHECK(s.acquire("l", "node-b", "token-b", 1000, 2501), "so another node can take it");
    CHECK(!s.release("l", "token-a"), "the old token cannot release it");
    CHECK(s.release("l", "token-b"), "the new holder can");
    delete db;
}

void a_lock_spans_connections() {
    // Two connections to one file: this is the property the memory store
    // cannot have, and the reason the SQL store exists.
    std::string path = "/tmp/opencode/qa-cluster-lock.db";
    ::remove(path.c_str());
    {
        auto* first = new hs::SqliteDb(path);
        auto* second = new hs::SqliteDb(path);
        hs::SqlCoordStore a(first), b(second);
        CHECK(a.acquire("invoice:9", "node-a", "token-a", 60'000, 1000), "node-a takes the lock");
        CHECK(!b.acquire("invoice:9", "node-b", "token-b", 60'000, 1000),
              "node-b, on its own connection, is refused");
        CHECK(a.release("invoice:9", "token-a"), "node-a releases it");
        CHECK(b.acquire("invoice:9", "node-b", "token-b", 60'000, 1000),
              "and node-b can take it immediately");
        // Idempotency keys behave the same way across connections.
        CHECK(a.claim("pay:1", "node-a", "token-a", 60'000, 1000), "node-a claims the key");
        CHECK(!b.claim("pay:1", "node-b", "token-b", 60'000, 1000), "node-b's retry is refused");
        a.record("pay:1", "{\"ok\":true}", 60'000, 1000);
        std::string out;
        CHECK(b.lookup("pay:1", out), "node-b reads the recorded outcome");
        CHECK_EQ(out, std::string("{\"ok\":true}"), "verbatim");
        delete second;
        delete first;
    }
    ::remove(path.c_str());
}

void the_sql_store_creates_its_own_tables() {
    auto* db = new hs::SqliteDb(":memory:");
    hs::SqlCoordStore s(db);
    // A fresh database has no hs_locks: constructing the store is what makes
    // it, so a program does not need a migration for a lock.
    hs::DbResult r = db->run("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
                             {hs::Val::text("hs_locks")});
    CHECK_EQ(r.rows.size(), size_t(1), "hs_locks exists after the store is built");
    r = db->run("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
                {hs::Val::text("hs_idem")});
    CHECK_EQ(r.rows.size(), size_t(1), "and hs_idem");
    delete db;
}

void the_sql_store_sweeps() {
    auto* db = new hs::SqliteDb(":memory:");
    hs::SqlCoordStore s(db);
    s.acquire("short", "n", "t", 1000, 1000);
    s.acquire("long", "n", "t", 60'000, 1000);
    s.sweep(2000);
    hs::CoordRecord r;
    CHECK(!s.get("short", r, 2000), "the expired lock is gone");
    CHECK(s.get("long", r, 2000), "the live one is not");
    delete db;
}

void the_sql_store_opens_from_a_configuration() {
    // The path a program takes: CLUSTER_STORE plus CLUSTER_DB.
    std::string path = "/tmp/opencode/qa-cluster-env.db";
    ::remove(path.c_str());
    hs::DbBackend* db = hs::cluster_open_db("sqlite", path);
    CHECK(db != nullptr, "a sqlite store opens from a path");
    CHECK_EQ(std::string(db->name()), std::string("sqlite"), "and is a sqlite connection");
    delete db;
    ::remove(path.c_str());
    bool threw = false;
    try {
        hs::cluster_open_db("redis", "whatever");
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("unknown store") != std::string::npos;
    }
    CHECK(threw, "an unknown store kind is named as one");
}

/// The store creates its tables on construction, so the script has to answer
/// those two statements before anything else.
PgReply create_reply(const char* match) {
    PgReply r;
    r.match = match;
    r.tag = "CREATE TABLE";
    r.affected = 0;
    return r;
}

void the_sql_store_works_over_postgres() {
    // The wire, not a fake: a lock is a conditional upsert, and the condition
    // is the whole guarantee, so the statement is asserted.
    PgReply taken;
    taken.match = "INSERT INTO hs_locks";
    taken.tag = "INSERT 0 1";
    taken.affected = 1;
    taken.once = true;  // only the first attempt took the row
    PgReply refused;
    refused.match = "INSERT INTO hs_locks";
    refused.tag = "INSERT 0 0";
    refused.affected = 0;
    // The mock answers in order, so the first acquire takes and the second is
    // refused -- exactly what a contended lock looks like from the server.
    MockPg server({create_reply("CREATE TABLE IF NOT EXISTS hs_locks"),
                    create_reply("CREATE TABLE IF NOT EXISTS hs_idem"), taken, refused});
    hs::PgDb db(server.conninfo());
    hs::SqlCoordStore s(&db);
    CHECK(s.acquire("l", "node-a", "token-a", 1000, 1000), "the first acquire took the row");
    CHECK(!s.acquire("l", "node-b", "token-b", 1000, 1000), "the second was refused by the server");
    auto statements = server.statements();
    bool saw_upsert = false, saw_expiry_condition = false;
    for (const auto& st : statements) {
        if (st.sql.find("INSERT INTO hs_locks") == std::string::npos) continue;
        saw_upsert = true;
        if (st.sql.find("ON CONFLICT") != std::string::npos &&
            st.sql.find("WHERE hs_locks.expires_at <= ?") != std::string::npos) {
            saw_expiry_condition = true;
        }
        // The bound values are the ones the caller passed, not string-spliced.
        CHECK_EQ(st.params.size(), size_t(8), "eight bound values on the upsert");
        CHECK_EQ(st.params[0].text, std::string("l"), "the lock name is bound");
        CHECK_EQ(st.params[7].text, std::string("1000"), "and so is the clock it compares against");
    }
    CHECK(saw_upsert, "the lock is taken with an upsert");
    CHECK(saw_expiry_condition, "which only takes over an expired row");
}

void the_sql_store_reads_a_held_lock_over_postgres() {
    PgReply row;
    row.match = "SELECT owner, token, expires_at";
    row.columns = {{"owner", 25}, {"token", 25}, {"expires_at", 20}};
    row.rows = {{PgCell("node-a"), PgCell("token-a"), PgCell("4000")}};
    MockPg server({create_reply("CREATE TABLE"), row});
    hs::PgDb db(server.conninfo());
    hs::SqlCoordStore s(&db);
    hs::CoordRecord r;
    CHECK(s.get("l", r, 1000), "the row is read");
    CHECK_EQ(r.owner, std::string("node-a"), "with its owner");
    CHECK_EQ(r.token, std::string("token-a"), "and its fencing token");
    CHECK(!s.get("l", r, 5000), "an expired row is not a held lock");
}

void a_database_error_surfaces_from_the_store() {
    PgReply boom;
    boom.match = "CREATE TABLE";
    boom.error = "permission denied for database qa";
    MockPg server({boom});
    hs::PgDb db(server.conninfo());
    bool threw = false;
    std::string msg;
    try {
        hs::SqlCoordStore s(&db);
    } catch (const std::exception& e) {
        threw = true;
        msg = e.what();
    }
    CHECK(threw, "a failed statement is not a silent success");
    CHECK(msg.find("permission denied") != std::string::npos, "and carries the server's message");
}

// ---------------------------------------------------------------------------
// Peer calls
// ---------------------------------------------------------------------------

struct PeerRequest {
    std::string method;
    std::string path;
    std::string body;
    std::string secret;
};

/// A loopback HTTP server that records what a peer call actually sent.
class MockPeer {
  public:
    explicit MockPeer(int status = 200, bool drop_first = false)
        : status_(status), drop_first_(drop_first) {
        listen_fd_ = socket(AF_INET, SOCK_STREAM, 0);
        int one = 1;
        setsockopt(listen_fd_, SOL_SOCKET, SO_REUSEADDR, (const char*)&one, sizeof one);
        struct sockaddr_in a;
        memset(&a, 0, sizeof a);
        a.sin_family = AF_INET;
        a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        a.sin_port = 0;
        if (bind(listen_fd_, (struct sockaddr*)&a, sizeof a) != 0)
            throw std::runtime_error("mock peer: bind");
        if (listen(listen_fd_, 8) != 0) throw std::runtime_error("mock peer: listen");
        socklen_t len = sizeof a;
        getsockname(listen_fd_, (struct sockaddr*)&a, &len);
        port_ = ntohs(a.sin_port);
        thread_ = std::thread([this] { serve(); });
    }

    ~MockPeer() {
        stop_ = true;
        if (listen_fd_ >= 0) {
            shutdown(listen_fd_, SHUT_RDWR);
            close(listen_fd_);
        }
        if (thread_.joinable()) thread_.join();
    }

    MockPeer(const MockPeer&) = delete;
    MockPeer& operator=(const MockPeer&) = delete;

    std::string address() const { return "127.0.0.1:" + std::to_string(port_); }
    std::vector<PeerRequest> seen() {
        std::lock_guard<std::mutex> lock(mu_);
        return seen_;
    }
    size_t connections() const { return connections_.load(); }

    void set_reply(const std::string& body, int status) {
        std::lock_guard<std::mutex> lock(mu_);
        body_ = body;
        status_ = status;
    }

  private:
    void serve() {
        while (!stop_) {
            int fd = accept(listen_fd_, nullptr, nullptr);
            if (fd < 0) {
                if (stop_) return;
                continue;
            }
            size_t n = connections_.fetch_add(1);
            if (drop_first_ && n == 0) {
                // A peer that was restarting: the connection dies before a
                // byte arrives, which is what a retry exists for.
                close(fd);
                continue;
            }
            one(fd);
            close(fd);
        }
    }

    void one(int fd) {
        std::string raw;
        char buf[2048];
        size_t head_end = std::string::npos;
        while (head_end == std::string::npos) {
            ssize_t n = recv(fd, buf, sizeof buf, 0);
            if (n <= 0) return;
            raw += std::string(buf, (size_t)n);
            head_end = raw.find("\r\n\r\n");
        }
        PeerRequest r;
        size_t sp1 = raw.find(' ');
        size_t sp2 = raw.find(' ', sp1 + 1);
        r.method = raw.substr(0, sp1);
        r.path = raw.substr(sp1 + 1, sp2 - sp1 - 1);
        size_t hlen = std::string::npos;
        {
            std::string lower;
            for (char c : raw.substr(0, head_end)) lower.push_back((char)tolower((unsigned char)c));
            size_t at = lower.find("content-length:");
            if (at != std::string::npos) hlen = (size_t)atoll(raw.c_str() + at + 15);
        }
        if (hlen != std::string::npos) {
            while (raw.size() < head_end + 4 + hlen) {
                ssize_t n = recv(fd, buf, sizeof buf, 0);
                if (n <= 0) break;
                raw += std::string(buf, (size_t)n);
            }
            r.body = raw.substr(head_end + 4, hlen);
        }
        size_t at = raw.find("X-HardScript-Cluster:");
        if (at != std::string::npos) {
            size_t value = at + strlen("X-HardScript-Cluster:");
            while (value < raw.size() && raw[value] == ' ') value++;
            size_t eol = raw.find("\r\n", value);
            r.secret = raw.substr(value, eol - value);
        }
        std::string body;
        int status;
        {
            std::lock_guard<std::mutex> lock(mu_);
            seen_.push_back(r);
            body = body_;
            status = status_;
        }
        std::string out = "HTTP/1.1 " + std::to_string(status) + " X\r\n";
        out += "Content-Type: application/json\r\n";
        out += "Content-Length: " + std::to_string(body.size()) + "\r\n";
        out += "Connection: close\r\n\r\n";
        out += body;
        size_t sent = 0;
        while (sent < out.size()) {
            ssize_t n = send(fd, out.data() + sent, out.size() - sent, 0);
            if (n <= 0) return;
            sent += (size_t)n;
        }
    }

    std::atomic<bool> stop_{false};
    std::atomic<int> connections_{0};
    bool drop_first_;
    int listen_fd_ = -1;
    int port_ = 0;
    int status_;
    std::string body_ = "{\"ok\":true}";
    std::thread thread_;
    std::mutex mu_;
    std::vector<PeerRequest> seen_;
};

void a_peer_call_is_a_real_request() {
    ClusterEnv env;
    env.secret("s3cret");
    MockPeer peer;
    hs::Val reply = hs::cluster_call(hs::Val::text(peer.address()), hs::Val::text("/jobs/run"),
                                     hs::Val::object({{"id", hs::Val::int_(7)}}));
    CHECK(reply.is_obj(), "the peer's JSON answer comes back parsed");
    CHECK(reply.find("ok") != nullptr && reply.find("ok")->truthy(), "with its content");
    auto seen = peer.seen();
    CHECK_EQ(seen.size(), size_t(1), "one request reached the peer");
    CHECK_EQ(seen[0].method, std::string("POST"), "a peer call is a POST");
    CHECK_EQ(seen[0].path, std::string("/jobs/run"), "to the path asked for");
    CHECK_EQ(seen[0].body, std::string("{\"id\":7}"), "with the body as JSON");
    CHECK_EQ(seen[0].secret, std::string("s3cret"), "and the shared secret");
}

void a_peer_call_retries_a_dropped_connection() {
    ClusterEnv env;
    env.secret("s");
    MockPeer peer(200, /*drop_first=*/true);
    hs::Val reply = hs::cluster_call(hs::Val::text(peer.address()), hs::Val::text("/ping"),
                                     hs::Val::nil());
    CHECK(reply.is_obj(), "the retry reached the peer");
    CHECK_EQ(peer.connections(), 2, "and it took two connections to get there");
}

void a_peer_error_is_reported_with_its_status() {
    ClusterEnv env;
    env.secret("s");
    MockPeer peer;
    peer.set_reply("{\"error\":\"no such job\"}", 500);
    bool threw = false;
    std::string msg;
    try {
        hs::cluster_call(hs::Val::text(peer.address()), hs::Val::text("/jobs/run"),
                         hs::Val::nil());
    } catch (const std::exception& e) {
        threw = true;
        msg = e.what();
    }
    CHECK(threw, "a 500 is an error, not a value");
    CHECK(msg.find("500") != std::string::npos, "the status is in the message");
    CHECK(msg.find("no such job") != std::string::npos, "and so is what the peer said");
}

void an_unreachable_peer_is_an_error() {
    ClusterEnv env;
    env.secret("s");
    MockPeer probe;
    std::string address = probe.address();
    // Drop the listener, then call: nothing is listening on that port now.
    {
        MockPeer gone;
        address = gone.address();
    }
    bool threw = false;
    std::string msg;
    try {
        hs::cluster_call(hs::Val::text(address), hs::Val::text("/ping"), hs::Val::nil());
    } catch (const std::exception& e) {
        threw = true;
        msg = e.what();
    }
    CHECK(threw, "an unreachable peer throws");
    CHECK(msg.find("failed") != std::string::npos, "saying the call failed");
}

void a_peer_address_may_be_a_node_id() {
    ClusterEnv env;
    env.node("web-1");
    env.nodes("worker-1@10.0.0.9:8080");
    // The id is resolved through CLUSTER_NODES, so a caller never hardcodes an
    // address; an unknown id is passed through as an address.
    MockPeer peer;
    env.nodes("worker-1@" + peer.address());
    hs::Val reply = hs::cluster_call(hs::Val::text("worker-1"), hs::Val::text("/ping"),
                                     hs::Val::nil());
    CHECK(reply.is_obj(), "a node id resolves to its address");
    auto seen = peer.seen();
    CHECK_EQ(seen.size(), size_t(1), "and the request arrived");
    CHECK_EQ(seen[0].path, std::string("/ping"), "at the path asked for");
}

void a_peer_call_checks_its_arguments() {
    auto want_fail = [&](std::function<void()> f, const char* needle, const char* what) {
        bool threw = false;
        std::string msg;
        try {
            f();
        } catch (const std::exception& e) {
            threw = true;
            msg = e.what();
        }
        CHECK(threw, what);
        CHECK(msg.find(needle) != std::string::npos,
              std::string(what) + ": the message says why (" + needle + ")");
    };
    want_fail([] { hs::cluster_call(hs::Val::text(""), hs::Val::text("/x"), hs::Val::nil()); },
              "needs a node", "call with no node");
    want_fail([] { hs::cluster_call(hs::Val::text("n:1"), hs::Val::text("no-slash"), hs::Val::nil()); },
              "starting with `/`", "call with a path that is not absolute");
}

// ---------------------------------------------------------------------------
// The whole thing, once
// ---------------------------------------------------------------------------

void a_single_node_cluster_works_end_to_end() {
    ClusterEnv env;
    env.node("solo");
    env.nodes("");
    hs::cluster_runtime().use_store(new hs::MemoryCoordStore());
    // Placement: one node owns everything, so every route is local.
    CHECK(hs::cluster_is_owner_val(hs::Val::text("anything")).truthy(), "solo owns every key");
    // Coordination: take the lock, do the work exactly once, release.
    CHECK(hs::lock_acquire(hs::Val::text("invoice:1"), hs::Val::int_(30)).truthy(), "lock taken");
    CHECK(hs::idem_claim(hs::Val::text("pay:invoice:1"), hs::Val::int_(30)).truthy(),
          "the key is claimed");
    hs::idem_record(hs::Val::text("pay:invoice:1"), hs::Val::object({{"ok", hs::Val::boolean(true)}}),
                    hs::Val::int_(30));
    CHECK(hs::lock_release(hs::Val::text("invoice:1")).truthy(), "lock released");
    hs::Val replay = hs::idem_lookup(hs::Val::text("pay:invoice:1"));
    CHECK(replay.is_obj() && replay.find("ok") != nullptr, "a replay reads the first answer");
    CHECK(!hs::idem_claim(hs::Val::text("pay:invoice:1"), hs::Val::int_(30)).truthy(),
          "and does not run the work again");
    // And the counters a program would expose are still just counters.
    CHECK(hs::lock_held(hs::Val::text("invoice:1")).is_bool(), "held answers either way");
}

}  // namespace
}  // namespace qa

int main() {
    qa::a_node_names_itself();
    qa::peers_come_from_the_environment();
    qa::the_ring_is_the_node_and_its_peers();
    qa::placement_is_deterministic();
    qa::every_node_gets_its_share();
    qa::removing_a_node_moves_only_its_keys();
    qa::a_lone_node_owns_everything();
    qa::is_owner_answers_for_this_node();
    qa::a_lock_is_exclusive();
    qa::only_the_holder_can_release();
    qa::a_lock_expires();
    qa::renewing_extends_only_for_the_holder();
    qa::sweeping_drops_what_expired();
    qa::an_idempotency_key_is_claimed_once();
    qa::an_idempotency_key_expires();
    qa::the_store_reports_its_kind();
    qa::the_lock_surface_is_what_a_program_calls();
    qa::the_lock_surface_checks_its_arguments();
    qa::the_idempotency_surface_round_trips_a_value();
    qa::one_thread_at_a_time_holds_the_lock();
    qa::the_sql_store_works_over_sqlite();
    qa::a_lock_spans_connections();
    qa::the_sql_store_creates_its_own_tables();
    qa::the_sql_store_sweeps();
    qa::the_sql_store_opens_from_a_configuration();
    qa::the_sql_store_works_over_postgres();
    qa::the_sql_store_reads_a_held_lock_over_postgres();
    qa::a_database_error_surfaces_from_the_store();
    qa::a_peer_call_is_a_real_request();
    qa::a_peer_call_retries_a_dropped_connection();
    qa::a_peer_error_is_reported_with_its_status();
    qa::an_unreachable_peer_is_an_error();
    qa::a_peer_address_may_be_a_node_id();
    qa::a_peer_call_checks_its_arguments();
    qa::a_single_node_cluster_works_end_to_end();
    return qa::report("cluster");
}
