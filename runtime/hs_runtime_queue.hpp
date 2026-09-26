// hs_runtime_queue.hpp -- background job queue (M6.2)
//
// `job SendEmail(User user)` declares a job type; `queue SendEmail(user)`
// enqueues one; `worker SendEmail { ... }` handles them on background
// threads. A job is a payload plus a fate: run now or later (delay), important
// or not (priority), retried on failure with backoff up to a maximum number
// of attempts, and buried in the dead-letter queue past that.
//
// Storage is a virtual backend. Memory holds jobs in per-type heaps; SQL
// stores them in a `jobs` table through the ORM's `DbBackend` seam, so the
// same code serves SQLite today and PostgreSQL by passing a `PgDb`. Workers
// poll, run the registered handler, and complete, fail or bury. Everything
// stops and joins before static destruction, the way the cache sweeper does.

#ifndef HS_RUNTIME_QUEUE_HPP
#define HS_RUNTIME_QUEUE_HPP

#include "hs_runtime_orm.hpp"

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <functional>
#include <map>
#include <mutex>
#include <queue>
#include <set>
#include <thread>
#include <vector>

namespace hs {

// Milliseconds on the monotonic clock, overridable for tests, exactly like
// the cache clock: while set, time stands still until the test moves it.
inline std::atomic<int64_t>& queue_test_clock() {
    static std::atomic<int64_t> override_ms{-1};
    return override_ms;
}

inline int64_t queue_now_ms() {
    int64_t fake = queue_test_clock().load(std::memory_order_relaxed);
    if (fake >= 0) return fake;
    return (int64_t)std::chrono::duration_cast<std::chrono::milliseconds>(
               std::chrono::steady_clock::now().time_since_epoch())
        .count();
}

inline void queue_set_now_for_tests(int64_t ms) {
    queue_test_clock().store(ms, std::memory_order_relaxed);
}

inline void queue_clear_now_for_tests() {
    queue_test_clock().store(-1, std::memory_order_relaxed);
}

/// One unit of work. `attempts` counts failures so far; `run_after_ms` is the
/// earliest the job may run (delay and backoff share the field on purpose:
/// there is only one future for a job).
struct Job {
    int64_t id = 0;
    std::string type;
    Val payload;
    int attempts = 0;
    int max_attempts = 5;
    int priority = 0;
    int64_t run_after_ms = 0;
    int64_t created_ms = 0;
    std::string last_error;
};

/// How long to wait after failure number `attempts` (1-based) before the next
/// try: 1s, 2s, 4s, ... capped at a minute. Backoff, not punishment: a
/// failing downstream gets room to recover instead of a hammering.
inline int64_t queue_backoff_ms(int attempts) {
    int shift = attempts - 1;
    if (shift < 0) shift = 0;
    if (shift > 6) shift = 6;
    int64_t wait = (int64_t)1000 << shift;
    return wait > (int64_t)60000 ? (int64_t)60000 : wait;
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

/// Where jobs wait. Memory is a process restart away from forgetting them;
/// SQL is not. Both answer the same questions, which is what makes the
/// PostgreSQL hook real rather than a comment: it is this interface over a
/// `PgDb`, tested against the mock wire server.
struct JobBackend {
    virtual ~JobBackend() = default;
    /// Store a job, assigning its id.
    virtual int64_t push(Job job) = 0;
    /// Claim the oldest ready job of one of `types` (empty means any),
    /// removing it from the waiting set. False when nothing is ready.
    /// Memory claims in two steps (poll, then track for a later fail); a
    /// crash between them loses the job, which is the documented cost of the
    /// memory backend. SQL claims atomically and has no gap.
    virtual bool poll(const std::vector<std::string>& types, int64_t now_ms, Job& out) = 0;
    virtual void complete(int64_t id) = 0;
    /// Fail a job: `retry_at_ms` above zero requeues it for then, zero buries
    /// it in the dead-letter queue. Attempts and the error travel with it.
    virtual void fail(int64_t id, int attempts, const std::string& error, int64_t retry_at_ms) = 0;
    /// Waiting jobs, optionally of one type.
    virtual int64_t depth(const std::string& type) = 0;
    /// Buried jobs, newest failure last.
    virtual std::vector<Job> dlq(const std::string& type) = 0;
    virtual void clear() = 0;
    /// Remember a claimed job for a later fail(), for backends that separate
    /// claiming from reading. The default ignores it: backends that read
    /// atomically have nothing to remember.
    virtual void track(const Job&) {}
};

/// Ready means due, then important, then first in line. `std::priority_queue`
/// is a max-heap, so the comparison reads backwards.
struct JobOrder {
    bool operator()(const Job& a, const Job& b) const {
        if (a.run_after_ms != b.run_after_ms) return a.run_after_ms > b.run_after_ms;
        if (a.priority != b.priority) return a.priority < b.priority;
        return a.id > b.id;
    }
};

class MemoryBackend : public JobBackend {
  public:
    int64_t push(Job job) override {
        std::lock_guard<std::mutex> lock(mu_);
        job.id = ++seq_;
        if (job.created_ms == 0) job.created_ms = queue_now_ms();
        queues_[job.type].push(std::move(job));
        int64_t id = seq_;
        return id;
    }

    bool poll(const std::vector<std::string>& types, int64_t now_ms, Job& out) override {
        std::lock_guard<std::mutex> lock(mu_);
        Job* best = nullptr;
        std::string best_type;
        if (types.empty()) {
            for (auto& kv : queues_) {
                if (!kv.second.empty() && kv.second.top().run_after_ms <= now_ms &&
                    (!best || JobOrder()(*best, kv.second.top()))) {
                    best = const_cast<Job*>(&kv.second.top());
                    best_type = kv.first;
                }
            }
        } else {
            for (const auto& t : types) {
                auto it = queues_.find(t);
                if (it == queues_.end() || it->second.empty()) continue;
                if (it->second.top().run_after_ms > now_ms) continue;
                if (!best || JobOrder()(*best, it->second.top())) {
                    best = const_cast<Job*>(&it->second.top());
                    best_type = it->first;
                }
            }
        }
        if (!best) return false;
        out = *best;
        queues_[best_type].pop();
        return true;
    }

    void complete(int64_t /*id*/) override {
        completed_.fetch_add(1, std::memory_order_relaxed);
    }

    void fail(int64_t id, int attempts, const std::string& error, int64_t retry_at_ms) override {
        // The job left the heap on claim; failing finds it either requeued or
        // buried. The id is carried for the SQL backend, which updates by row.
        (void)id;
        if (retry_at_ms > 0) {
            std::lock_guard<std::mutex> lock(mu_);
            auto it = inflight_.find(id);
            if (it == inflight_.end()) return;
            Job job = it->second;
            inflight_.erase(it);
            job.attempts = attempts;
            job.last_error = error;
            job.run_after_ms = retry_at_ms;
            queues_[job.type].push(std::move(job));
            retried_.fetch_add(1, std::memory_order_relaxed);
            return;
        }
        std::lock_guard<std::mutex> lock(mu_);
        auto it = inflight_.find(id);
        if (it == inflight_.end()) return;
        it->second.attempts = attempts;
        it->second.last_error = error;
        buried_.push_back(it->second);
        inflight_.erase(it);
    }

    int64_t depth(const std::string& type) override {
        std::lock_guard<std::mutex> lock(mu_);
        if (type.empty()) {
            int64_t n = 0;
            for (auto& kv : queues_) n += (int64_t)kv.second.size();
            return n;
        }
        auto it = queues_.find(type);
        return it == queues_.end() ? 0 : (int64_t)it->second.size();
    }

    std::vector<Job> dlq(const std::string& type) override {
        std::lock_guard<std::mutex> lock(mu_);
        if (type.empty()) return buried_;
        std::vector<Job> out;
        for (auto& j : buried_)
            if (j.type == type) out.push_back(j);
        return out;
    }

    void clear() override {
        std::lock_guard<std::mutex> lock(mu_);
        queues_.clear();
        buried_.clear();
        inflight_.clear();
    }

    void track(const Job& job) override {
        std::lock_guard<std::mutex> lock(mu_);
        inflight_[job.id] = job;
    }

  private:
    std::mutex mu_;
    int64_t seq_ = 0;
    std::map<std::string, std::priority_queue<Job, std::vector<Job>, JobOrder>> queues_;
    std::map<int64_t, Job> inflight_;
    std::vector<Job> buried_;
    std::atomic<int64_t> completed_{0};
    std::atomic<int64_t> retried_{0};
};

/// Jobs in a table, through the ORM's backend seam. One implementation serves
/// SQLite directly and PostgreSQL through the same statements with `$n`
/// placeholders and `RETURNING`: the dialect is read off the connection, and
/// the only two statements that differ are picked per dialect.
///
/// The connection is dedicated to the queue: worker threads share it under
/// one mutex, but request threads must not use it concurrently.
class SqlJobBackend : public JobBackend {
  public:
    explicit SqlJobBackend(DbBackend* db) : db_(db) { ensure(); }

    void ensure() {
        std::lock_guard<std::mutex> lock(mu_);
        db_->run("CREATE TABLE IF NOT EXISTS \"hs_jobs\" (\"id\" INTEGER PRIMARY KEY AUTOINCREMENT,"
                 "\"type\" TEXT NOT NULL,\"payload\" TEXT NOT NULL,\"attempts\" INTEGER NOT NULL,"
                 "\"max_attempts\" INTEGER NOT NULL,\"priority\" INTEGER NOT NULL,"
                 "\"run_after\" INTEGER NOT NULL,\"status\" TEXT NOT NULL,\"last_error\" TEXT NOT NULL,"
                 "\"created\" INTEGER NOT NULL)",
                 {});
        db_->run("CREATE INDEX IF NOT EXISTS \"hs_jobs_ready\" ON \"hs_jobs\" (\"status\", \"run_after\")", {});
        // Missed-job recovery: a crash between claim and finish leaves rows
        // claimed, and no worker will ever look at them again. Requeue them
        // on open, when the alternative is silent loss.
        db_->run("UPDATE \"hs_jobs\" SET \"status\" = 'queued' WHERE \"status\" = 'claimed'", {});
    }

    int64_t push(Job job) override {
        std::lock_guard<std::mutex> lock(mu_);
        if (job.created_ms == 0) job.created_ms = queue_now_ms();
        std::string sql = "INSERT INTO \"hs_jobs\" (\"type\", \"payload\", \"attempts\", \"max_attempts\","
                          " \"priority\", \"run_after\", \"status\", \"last_error\", \"created\") VALUES (";
        for (int i = 1; i <= 9; i++) {
            if (i > 1) sql += ", ";
            sql += db_placeholder(db_->dialect(), i);
        }
        sql += ")";
        if (db_->dialect() == DbDialect::Postgres) sql += " RETURNING \"id\"";
        DbResult r = db_->run(sql, {Val::text(job.type), Val::text(job.payload.to_json()),
                                    Val::int_(job.attempts), Val::int_(job.max_attempts),
                                    Val::int_(job.priority), Val::int_(job.run_after_ms),
                                    Val::text("queued"), Val::text(""), Val::int_(job.created_ms)});
        if (!r.rows.empty() && !r.rows[0].empty() && r.rows[0][0].is_int()) return r.rows[0][0].iv;
        return r.last_id;
    }

    bool poll(const std::vector<std::string>& types, int64_t now_ms, Job& out) override {
        std::lock_guard<std::mutex> lock(mu_);
        if (db_->dialect() == DbDialect::Postgres) return poll_pg(types, now_ms, out);
        // SQLite has no SELECT..FOR UPDATE: the claim is BEGIN IMMEDIATE plus
        // an update, which holds the write lock across both.
        db_->begin();
        std::string sql = "SELECT \"id\", \"type\", \"payload\", \"attempts\", \"max_attempts\", \"priority\","
                          " \"run_after\", \"last_error\", \"created\" FROM \"hs_jobs\" WHERE \"status\" = 'queued'"
                          " AND \"run_after\" <= " +
                          db_placeholder(db_->dialect(), 1);
        std::vector<Val> params = {Val::int_(now_ms)};
        if (!types.empty()) {
            sql += " AND \"type\" IN (";
            for (size_t i = 0; i < types.size(); i++) {
                if (i) sql += ", ";
                sql += db_placeholder(db_->dialect(), (int)params.size() + 1);
                params.push_back(Val::text(types[i]));
            }
            sql += ")";
        }
        sql += " ORDER BY \"run_after\", \"priority\" DESC, \"id\" LIMIT 1";
        DbResult r = db_->run(sql, params);
        if (r.rows.empty()) {
            db_->rollback();
            return false;
        }
        read_row(r.rows[0], out);
        std::string up = "UPDATE \"hs_jobs\" SET \"status\" = 'claimed' WHERE \"id\" = " +
                         db_placeholder(db_->dialect(), 1);
        db_->run(up, {Val::int_(out.id)});
        db_->commit();
        return true;
    }

    void complete(int64_t id) override {
        std::lock_guard<std::mutex> lock(mu_);
        std::string sql =
            "DELETE FROM \"hs_jobs\" WHERE \"id\" = " + db_placeholder(db_->dialect(), 1);
        db_->run(sql, {Val::int_(id)});
    }

    void fail(int64_t id, int attempts, const std::string& error, int64_t retry_at_ms) override {
        std::lock_guard<std::mutex> lock(mu_);
        if (retry_at_ms > 0) {
            std::string sql = "UPDATE \"hs_jobs\" SET \"attempts\" = " + db_placeholder(db_->dialect(), 1) +
                              ", \"last_error\" = " + db_placeholder(db_->dialect(), 2) +
                              ", \"run_after\" = " + db_placeholder(db_->dialect(), 3) +
                              ", \"status\" = 'queued' WHERE \"id\" = " + db_placeholder(db_->dialect(), 4);
            db_->run(sql, {Val::int_(attempts), Val::text(error), Val::int_(retry_at_ms), Val::int_(id)});
            return;
        }
        std::string sql = "UPDATE \"hs_jobs\" SET \"attempts\" = " + db_placeholder(db_->dialect(), 1) +
                          ", \"last_error\" = " + db_placeholder(db_->dialect(), 2) +
                          ", \"status\" = 'dlq' WHERE \"id\" = " + db_placeholder(db_->dialect(), 3);
        db_->run(sql, {Val::int_(attempts), Val::text(error), Val::int_(id)});
    }

    int64_t depth(const std::string& type) override {
        std::lock_guard<std::mutex> lock(mu_);
        std::string sql = "SELECT COUNT(*) FROM \"hs_jobs\" WHERE \"status\" = 'queued'";
        std::vector<Val> params;
        if (!type.empty()) {
            sql += " AND \"type\" = " + db_placeholder(db_->dialect(), 1);
            params.push_back(Val::text(type));
        }
        DbResult r = db_->run(sql, params);
        if (!r.rows.empty() && !r.rows[0].empty() && r.rows[0][0].is_int()) return r.rows[0][0].iv;
        return 0;
    }

    std::vector<Job> dlq(const std::string& type) override {
        std::lock_guard<std::mutex> lock(mu_);
        std::string sql = "SELECT \"id\", \"type\", \"payload\", \"attempts\", \"max_attempts\", \"priority\","
                          " \"run_after\", \"last_error\", \"created\" FROM \"hs_jobs\" WHERE \"status\" = 'dlq'";
        std::vector<Val> params;
        if (!type.empty()) {
            sql += " AND \"type\" = " + db_placeholder(db_->dialect(), 1);
            params.push_back(Val::text(type));
        }
        sql += " ORDER BY \"id\"";
        DbResult r = db_->run(sql, params);
        std::vector<Job> out;
        for (const auto& row : r.rows) {
            Job j;
            read_row(row, j);
            out.push_back(std::move(j));
        }
        return out;
    }

    void clear() override {
        std::lock_guard<std::mutex> lock(mu_); db_->run("DELETE FROM \"hs_jobs\"", {}); }

  private:
    void read_row(const std::vector<Val>& row, Job& out) {
        // Column order is the SELECT's above, and every getter below is
        // guarded: a short row decodes to a zero job rather than a crash.
        auto iv = [&](size_t i) { return i < row.size() && row[i].is_int() ? row[i].iv : 0; };
        auto sv = [&](size_t i) -> std::string {
            return i < row.size() && row[i].is_str() ? row[i].sv : "";
        };
        out.id = iv(0);
        out.type = sv(1);
        try {
            out.payload = parse_json(sv(2));
        } catch (...) {
            out.payload = Val::object({});
        }
        out.attempts = (int)iv(3);
        out.max_attempts = (int)iv(4);
        out.priority = (int)iv(5);
        out.run_after_ms = iv(6);
        out.last_error = sv(7);
        out.created_ms = iv(8);
    }

    // One round trip: claim and read the oldest ready row, skipping rows
    // other workers hold. SQLite cannot do this in one statement, which is
    // why it takes the lock instead.
    bool poll_pg(const std::vector<std::string>& types, int64_t now_ms, Job& out) {
        std::string sql = "UPDATE \"hs_jobs\" SET \"status\" = 'claimed' WHERE \"id\" = (SELECT \"id\""
                          " FROM \"hs_jobs\" WHERE \"status\" = 'queued' AND \"run_after\" <= " +
                          db_placeholder(db_->dialect(), 1);
        std::vector<Val> params = {Val::int_(now_ms)};
        if (!types.empty()) {
            sql += " AND \"type\" IN (";
            for (size_t i = 0; i < types.size(); i++) {
                if (i) sql += ", ";
                sql += db_placeholder(db_->dialect(), (int)params.size() + 1);
                params.push_back(Val::text(types[i]));
            }
            sql += ")";
        }
        sql += " ORDER BY \"run_after\", \"priority\" DESC, \"id\" LIMIT 1 FOR UPDATE SKIP LOCKED)"
               " RETURNING \"id\", \"type\", \"payload\", \"attempts\", \"max_attempts\", \"priority\","
               " \"run_after\", \"last_error\", \"created\"";
        DbResult r = db_->run(sql, params);
        if (r.rows.empty()) return false;
        read_row(r.rows[0], out);
        return true;
    }

    // One connection serves every worker thread, and neither wire protocol
    // interleaves: every statement here holds this mutex. The database stays
    // the bottleneck, not the lock.
    std::mutex mu_;
    DbBackend* db_;
};

// ---------------------------------------------------------------------------
// Registry, workers, language surface
// ---------------------------------------------------------------------------

/// A declared job type: how many payload arguments it takes, and how many
/// tries one gets by default.
struct JobDecl {
    int nparams = 0;
    std::vector<std::string> params;
    int max_attempts = 5;
};

using JobHandler = std::function<Val(const Val& payload)>;

struct WorkerReg {
    JobHandler handler;
    int concurrency = 4;
};

/// The process-wide queue state. One static, stopped and joined before it
/// destroys, so worker threads never outlive the backends they poll.
class QueueRuntime {
  public:
    QueueRuntime() : backend_(new MemoryBackend()) {}

    ~QueueRuntime() {
        stop.store(true, std::memory_order_relaxed);
        wake.notify_all();
        for (auto& t : workers_) {
            if (t.joinable()) t.join();
        }
    }

    QueueRuntime(const QueueRuntime&) = delete;
    QueueRuntime& operator=(const QueueRuntime&) = delete;

    void declare_job(const std::string& name, const std::vector<std::string>& params, int max_attempts) {
        std::lock_guard<std::mutex> lock(mu_);
        JobDecl& d = decls_[name];
        d.params = params;
        d.nparams = (int)params.size();
        d.max_attempts = max_attempts;
    }

    int64_t enqueue(const std::string& type, const Val& args, int64_t delay_ms, int priority,
                    int max_attempts) {
        JobDecl decl;
        {
            std::lock_guard<std::mutex> lock(mu_);
            auto it = decls_.find(type);
            if (it == decls_.end())
                throw std::runtime_error("queue: unknown job `" + type + "`; declare it with `job " + type +
                                         "(...)` first");
            decl = it->second;
        }
        if (!args.is_arr() || (int)args.arr.size() != decl.nparams)
            throw std::runtime_error("queue: `" + type + "` takes " + std::to_string(decl.nparams) +
                                     " argument(s)");
        if (delay_ms < 0) throw std::runtime_error("queue: delay seconds must be 0 or more");
        // max_attempts arrives as 0 for "keep the declared default": the free
        // function validates user input, so by here only the sentinel remains.
        Val payload = Val::object({});
        for (size_t i = 0; i < decl.params.size() && i < args.arr.size(); i++) {
            payload.set(decl.params[i], args.arr[i]);
        }
        Job job;
        job.type = type;
        job.payload = payload;
        job.max_attempts = max_attempts > 0 ? max_attempts : decl.max_attempts;
        job.priority = priority;
        job.run_after_ms = queue_now_ms() + delay_ms;
        job.created_ms = queue_now_ms();
        int64_t id;
        {
            std::lock_guard<std::mutex> lock(mu_);
            id = backend_->push(std::move(job));
            enqueued_.fetch_add(1, std::memory_order_relaxed);
        }
        wake.notify_all();
        return id;
    }

    void register_worker(const std::string& type, int concurrency, JobHandler handler) {
        if (concurrency <= 0)
            throw std::runtime_error("queue: worker concurrency must be 1 or more");
        std::lock_guard<std::mutex> lock(mu_);
        WorkerReg& w = workers_reg_[type];
        w.handler = std::move(handler);
        w.concurrency = concurrency;
        // Threads start for newly registered types with queued work waiting;
        // a type registered twice keeps one pool per registration call is
        // wrong, so extra calls only replace the handler.
        if (threads_for_.insert(type).second) {
            for (int i = 0; i < concurrency; i++) {
                workers_.emplace_back([this, type] { work_loop(type); });
            }
        }
        wake.notify_all();
    }

    void set_concurrency(const std::string& type, int n) {
        if (n <= 0) throw std::runtime_error("queue: worker concurrency must be 1 or more");
        std::lock_guard<std::mutex> lock(mu_);
        workers_reg_[type].concurrency = n;
    }

    void use_backend(JobBackend* backend) {
        if (!backend) throw std::runtime_error("queue: backend must not be null");
        std::lock_guard<std::mutex> lock(mu_);
        backend_.reset(backend);
    }

    JobBackend* backend() {
        std::lock_guard<std::mutex> lock(mu_);
        return backend_.get();
    }

    int64_t depth(const std::string& type) {
        std::lock_guard<std::mutex> lock(mu_);
        return backend_->depth(type);
    }

    std::vector<Job> dlq(const std::string& type) {
        std::lock_guard<std::mutex> lock(mu_);
        return backend_->dlq(type);
    }

    void clear() {
        std::lock_guard<std::mutex> lock(mu_);
        backend_->clear();
    }

    uint64_t enqueued() const { return enqueued_.load(std::memory_order_relaxed); }
    uint64_t completed() const { return completed_.load(std::memory_order_relaxed); }
    uint64_t failed() const { return failed_.load(std::memory_order_relaxed); }

  private:
    void work_loop(const std::string& type) {
        for (;;) {
            Job job;
            JobHandler handler;
            bool claimed = false;
            {
                std::unique_lock<std::mutex> lock(mu_);
                if (stop.load(std::memory_order_relaxed)) return;
                auto it = workers_reg_.find(type);
                if (it == workers_reg_.end() || !it->second.handler) {
                    // Registered without a handler (or unregistered): wait
                    // for work or shutdown instead of spinning.
                    wake.wait_for(lock, std::chrono::milliseconds(10), [this] {
                        return stop.load(std::memory_order_relaxed);
                    });
                    if (stop.load(std::memory_order_relaxed)) return;
                    continue;
                }
                handler = it->second.handler;
                // A throwing backend must not kill the thread: back off a
                // beat and try again. Poison (a row that always throws) still
                // needs an operator, and crashing the worker is not it.
                try {
                    claimed = backend_->poll({type}, queue_now_ms(), job);
                } catch (...) {
                    wake.wait_for(lock, std::chrono::milliseconds(50), [this] {
                        return stop.load(std::memory_order_relaxed);
                    });
                    if (stop.load(std::memory_order_relaxed)) return;
                    continue;
                }
                if (!claimed) {
                    wake.wait_for(lock, std::chrono::milliseconds(10), [this] {
                        return stop.load(std::memory_order_relaxed);
                    });
                    if (stop.load(std::memory_order_relaxed)) return;
                    continue;
                }
                backend_->track(job);
            }
            // The handler runs unlocked: this is where the concurrency lives.
            // An exception is a failed job, never a dead thread.
            try {
                handler(job.payload);
            } catch (const std::exception& e) {
                on_error(job, e.what());
                continue;
            } catch (...) {
                on_error(job, "unknown error");
                continue;
            }
            {
                std::lock_guard<std::mutex> lock(mu_);
                backend_->complete(job.id);
                completed_.fetch_add(1, std::memory_order_relaxed);
            }
        }
    }

    void on_error(const Job& job, const std::string& what) {
        int attempts = job.attempts + 1;
        std::lock_guard<std::mutex> lock(mu_);
        if (attempts < job.max_attempts) {
            backend_->fail(job.id, attempts, what, queue_now_ms() + queue_backoff_ms(attempts));
        } else {
            backend_->fail(job.id, attempts, what, 0);
        }
        failed_.fetch_add(1, std::memory_order_relaxed);
    }

    mutable std::mutex mu_;
    std::condition_variable wake;
    std::atomic<bool> stop{false};
    std::vector<std::thread> workers_;
    std::map<std::string, JobDecl> decls_;
    std::map<std::string, WorkerReg> workers_reg_;
    std::set<std::string> threads_for_;
    std::unique_ptr<JobBackend> backend_;
    std::atomic<uint64_t> enqueued_{0};
    std::atomic<uint64_t> completed_{0};
    std::atomic<uint64_t> failed_{0};
};

inline QueueRuntime& queue_runtime() {
    static QueueRuntime r;
    return r;
}

// ---------------------------------------------------------------------------
// Language surface
// ---------------------------------------------------------------------------

/// `job Name(..)`: register the type, its parameter count and names, and the
/// default attempt budget. Re-declaring updates: declarations run at startup
/// and in tests, and neither may fail for running twice.
inline Val queue_declare_job(const Val& name, const Val& params, const Val& max_attempts) {
    if (!name.is_str() || name.sv.empty())
        throw std::runtime_error("queue: declare needs a job name as text");
    if (!params.is_arr())
        throw std::runtime_error("queue: declare needs a parameter name list");
    std::vector<std::string> names;
    for (const auto& p : params.arr) {
        if (!p.is_str() || p.sv.empty())
            throw std::runtime_error("queue: declare parameter names must be text");
        names.push_back(p.sv);
    }
    int maxa = 5;
    if (!max_attempts.is_nil()) {
        if (!max_attempts.is_int() || max_attempts.iv < 1)
            throw std::runtime_error("queue: declare max attempts must be 1 or more");
        maxa = (int)max_attempts.iv;
    }
    queue_runtime().declare_job(name.sv, names, maxa);
    return Val::boolean(true);
}

/// `queue Name(args, delay = s, priority = n, max_attempts = n)`: enqueue one
/// job, returning its id.
inline Val queue_enqueue(const Val& job, const Val& args, const Val& options) {
    if (!job.is_str() || job.sv.empty())
        throw std::runtime_error("queue: enqueue needs a job name as text");
    if (!args.is_arr()) throw std::runtime_error("queue: enqueue needs an argument list");
    int64_t delay_ms = 0;
    int priority = 0;
    int max_attempts = 0;  // 0 keeps the declared default
    if (!options.is_nil()) {
        if (!options.is_obj()) throw std::runtime_error("queue: enqueue options must be an object");
        for (const auto& kv : options.obj) {
            const Val* v = &kv.second;
            if (!v->is_int())
                throw std::runtime_error("queue: option `" + kv.first + "` must be an integer");
            if (kv.first == "delay") {
                if (v->iv < 0) throw std::runtime_error("queue: delay seconds must be 0 or more");
                delay_ms = v->iv * 1000;
            } else if (kv.first == "priority") {
                priority = (int)v->iv;
            } else if (kv.first == "max_attempts") {
                if (v->iv < 1) throw std::runtime_error("queue: max_attempts must be 1 or more");
                max_attempts = (int)v->iv;
            } else {
                throw std::runtime_error("queue: unknown option `" + kv.first +
                                         "` (delay, priority, max_attempts)");
            }
        }
    }
    return Val::int_(queue_runtime().enqueue(job.sv, args, delay_ms, priority, max_attempts));
}

inline Val queue_register_worker(const Val& name, const Val& concurrency, JobHandler handler) {
    if (!name.is_str() || name.sv.empty())
        throw std::runtime_error("queue: register_worker needs a job name as text");
    if (!concurrency.is_int() || concurrency.iv < 1)
        throw std::runtime_error("queue: worker concurrency must be 1 or more");
    if (!handler) throw std::runtime_error("queue: worker needs a handler");
    queue_runtime().register_worker(name.sv, (int)concurrency.iv, std::move(handler));
    return Val::boolean(true);
}

inline Val queue_depth(const Val& type) {
    std::string t;
    if (!type.is_nil()) {
        if (!type.is_str()) throw std::runtime_error("queue: depth needs a job name as text");
        t = type.sv;
    }
    return Val::int_(queue_runtime().depth(t));
}

inline Val queue_dlq_list(const Val& type) {
    std::string t;
    if (!type.is_nil()) {
        if (!type.is_str()) throw std::runtime_error("queue: dlq needs a job name as text");
        t = type.sv;
    }
    std::vector<Val> out;
    for (const auto& j : queue_runtime().dlq(t)) {
        out.push_back(Val::object({{"id", Val::int_(j.id)},
                                   {"type", Val::text(j.type)},
                                   {"attempts", Val::int_(j.attempts)},
                                   {"error", Val::text(j.last_error)},
                                   {"payload", j.payload}}));
    }
    return Val::list(std::move(out));
}

}  // namespace hs

#endif  // HS_RUNTIME_QUEUE_HPP
