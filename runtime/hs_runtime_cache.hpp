// hs_runtime_cache.hpp -- native cache engine (M6.1)
//
// A TTL map from names to values: `cache users ttl 10m` declares how long the
// `users` entry lives, `cache.get("users")` reads it back, and ten minutes
// later it is gone. One entry per name; there are no keys inside a cache, so
// there is nothing to design around them.
//
// Concurrency is shard-striped: 64 mutex-guarded shards keyed by hash, so
// request threads share the cache without a global lock. Expiration is lazy
// on every access plus a timing wheel swept once a second: each set lands in
// exactly one one-second bucket, and the sweeper drops whatever is due. A
// version stamp per entry tells stale wheel marks from live ones.
//
// Values are owned copies (`hs::Val`): request arenas are per-thread and
// cannot be shared, while small values ride `Val`'s inline storage and copy
// without allocating on the way out.

#ifndef HS_RUNTIME_CACHE_HPP
#define HS_RUNTIME_CACHE_HPP

#include "hs_runtime_value.hpp"

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <mutex>
#include <thread>
#include <unordered_map>
#include <vector>

namespace hs {

// Milliseconds on the monotonic clock. Wall time would jump the wheel every
// time the system clock is stepped; monotonic time only moves forward.
//
// Tests override the clock through `cache_set_now_for_tests`: while set, time
// stands still until the test moves it, which makes expiration deterministic
// without sleeping. Production code never touches it.
inline std::atomic<int64_t>& cache_test_clock() {
    static std::atomic<int64_t> override_ms{-1};
    return override_ms;
}

inline int64_t cache_now_ms() {
    int64_t fake = cache_test_clock().load(std::memory_order_relaxed);
    if (fake >= 0) return fake;
    return (int64_t)std::chrono::duration_cast<std::chrono::milliseconds>(
               std::chrono::steady_clock::now().time_since_epoch())
        .count();
}

inline void cache_set_now_for_tests(int64_t ms) {
    cache_test_clock().store(ms, std::memory_order_relaxed);
}

inline void cache_clear_now_for_tests() {
    cache_test_clock().store(-1, std::memory_order_relaxed);
}

struct CacheEntry {
    Val value;
    /// Milliseconds at which the entry dies, or 0 to persist.
    int64_t expires_at_ms = 0;
    /// Bumped on every set and expire, so a wheel mark made before the bump
    /// is recognizable as stale.
    uint64_t version = 0;
};

struct CacheShard {
    std::mutex mu;
    std::unordered_map<std::string, CacheEntry> map;
};

/// One mark in the timing wheel: the entry that was live when the mark was
/// made, the version it had then, and when it dies (so a swept-but-not-due
/// mark goes back to the right slot).
struct CacheMark {
    uint32_t shard = 0;
    std::string key;
    uint64_t version = 0;
    int64_t expires_at_ms = 0;
};

class Cache {
  public:
    static constexpr uint32_t SHARDS = 64;
    /// One-second buckets; a TTL lands in exactly one.
    static constexpr uint32_t WHEEL = 3600;

    Cache() : wheel_(WHEEL) {}

    Cache(const Cache&) = delete;
    Cache& operator=(const Cache&) = delete;

    /// Declare a name's default TTL in seconds. Re-declaring updates it.
    /// Negative means persist.
    void declare(const std::string& name, int64_t ttl_secs) {
        std::lock_guard<std::mutex> lock(defaults_mu_);
        defaults_[name] = ttl_secs;
    }

    int64_t default_ttl(const std::string& name) const {
        std::lock_guard<std::mutex> lock(defaults_mu_);
        auto it = defaults_.find(name);
        return it == defaults_.end() ? -1 : it->second;
    }

    /// Read a name, or nil when it is missing or expired. An expired entry is
    /// dropped on the way out: lazy expiration means no reader ever sees a
    /// dead value, sweeper or not.
    Val get(const std::string& name) {
        CacheShard& s = shard_for(name);
        std::lock_guard<std::mutex> lock(s.mu);
        auto it = s.map.find(name);
        if (it == s.map.end()) {
            misses_.fetch_add(1, std::memory_order_relaxed);
            return Val::nil();
        }
        if (expired(it->second, cache_now_ms())) {
            s.map.erase(it);
            evictions_.fetch_add(1, std::memory_order_relaxed);
            misses_.fetch_add(1, std::memory_order_relaxed);
            return Val::nil();
        }
        hits_.fetch_add(1, std::memory_order_relaxed);
        return it->second.value;
    }

    /// Store a name. `ttl_secs` below zero means the declared default, or
    /// persist when nothing was declared.
    void set(const std::string& name, const Val& value, int64_t ttl_secs = -2) {
        if (ttl_secs == -2) ttl_secs = default_ttl(name);
        if (ttl_secs < -1)
            throw std::runtime_error("cache: TTL seconds must be 0 or more (got " +
                                     std::to_string(ttl_secs) + ")");
        CacheShard& s = shard_for(name);
        int64_t now = cache_now_ms();
        std::lock_guard<std::mutex> lock(s.mu);
        CacheEntry& e = s.map[name];
        e.value = value;
        e.version++;
        e.expires_at_ms = ttl_secs < 0 ? 0 : now + ttl_secs * 1000;
        sets_.fetch_add(1, std::memory_order_relaxed);
        if (e.expires_at_ms > 0) {
            uint32_t slot = (uint32_t)((e.expires_at_ms / 1000) % WHEEL);
            std::lock_guard<std::mutex> wlock(wheel_mu_);
            wheel_[slot].push_back(
                CacheMark{(uint32_t)(shard_index(name)), name, e.version, e.expires_at_ms});
        }
    }

    /// Drop a name. True when something live was dropped.
    bool del(const std::string& name) {
        CacheShard& s = shard_for(name);
        std::lock_guard<std::mutex> lock(s.mu);
        auto it = s.map.find(name);
        if (it == s.map.end() || expired(it->second, cache_now_ms())) {
            if (it != s.map.end()) {
                s.map.erase(it);
                evictions_.fetch_add(1, std::memory_order_relaxed);
            }
            return false;
        }
        s.map.erase(it);
        return true;
    }

    bool exists(const std::string& name) {
        CacheShard& s = shard_for(name);
        std::lock_guard<std::mutex> lock(s.mu);
        auto it = s.map.find(name);
        if (it == s.map.end()) return false;
        if (expired(it->second, cache_now_ms())) {
            s.map.erase(it);
            evictions_.fetch_add(1, std::memory_order_relaxed);
            return false;
        }
        return true;
    }

    /// Add `step` to an integer entry and return the new value. A missing
    /// entry starts at zero, the way counters do. Anything else stored under
    /// the name is a type error, not a number.
    int64_t incr(const std::string& name, int64_t step = 1) {
        CacheShard& s = shard_for(name);
        std::lock_guard<std::mutex> lock(s.mu);
        auto it = s.map.find(name);
        if (it != s.map.end() && expired(it->second, cache_now_ms())) {
            s.map.erase(it);
            evictions_.fetch_add(1, std::memory_order_relaxed);
            it = s.map.end();
        }
        if (it == s.map.end()) {
            CacheEntry e;
            e.value = Val::int_(step);
            s.map[name] = e;
            return step;
        }
        if (!it->second.value.is_int())
            throw std::runtime_error("cache: \"" + name + "\" holds " + it->second.value.kind_name() +
                                     ", and only integers increment");
        it->second.value.iv += step;
        return it->second.value.iv;
    }

    int64_t decr(const std::string& name, int64_t step = 1) { return incr(name, -step); }

    /// Drop everything. Returns how many live entries went.
    int64_t clear() {
        int64_t n = 0;
        for (auto& s : shards_) {
            std::lock_guard<std::mutex> lock(s.mu);
            int64_t now = cache_now_ms();
            for (auto it = s.map.begin(); it != s.map.end();) {
                if (!expired(it->second, now)) n++;
                it = s.map.erase(it);
            }
        }
        return n;
    }

    /// Every live name, sorted. Expiry is checked, not assumed: an entry that
    /// died since the sweeper's last pass is dropped here instead of listed.
    std::vector<std::string> keys() {
        std::vector<std::string> out;
        for (auto& s : shards_) {
            std::lock_guard<std::mutex> lock(s.mu);
            int64_t now = cache_now_ms();
            for (auto it = s.map.begin(); it != s.map.end();) {
                if (expired(it->second, now)) {
                    it = s.map.erase(it);
                    evictions_.fetch_add(1, std::memory_order_relaxed);
                    continue;
                }
                out.push_back(it->first);
                ++it;
            }
        }
        std::sort(out.begin(), out.end());
        return out;
    }

    /// Seconds left on a name: -2 when nothing is stored, -1 when it
    /// persists, otherwise the time to live.
    int64_t ttl(const std::string& name) {
        CacheShard& s = shard_for(name);
        std::lock_guard<std::mutex> lock(s.mu);
        auto it = s.map.find(name);
        if (it == s.map.end()) return -2;
        if (expired(it->second, cache_now_ms())) {
            s.map.erase(it);
            evictions_.fetch_add(1, std::memory_order_relaxed);
            return -2;
        }
        if (it->second.expires_at_ms == 0) return -1;
        int64_t left = (it->second.expires_at_ms - cache_now_ms() + 999) / 1000;
        return left < 0 ? 0 : left;
    }

    /// Give a live name a new TTL in seconds, refreshing an existing one.
    /// Zero expires it now. False when nothing is stored.
    bool expire(const std::string& name, int64_t ttl_secs) {
        if (ttl_secs < 0)
            throw std::runtime_error("cache: TTL seconds must be 0 or more (got " +
                                     std::to_string(ttl_secs) + ")");
        CacheShard& s = shard_for(name);
        int64_t now = cache_now_ms();
        std::lock_guard<std::mutex> lock(s.mu);
        auto it = s.map.find(name);
        if (it == s.map.end() || expired(it->second, now)) {
            if (it != s.map.end()) {
                s.map.erase(it);
                evictions_.fetch_add(1, std::memory_order_relaxed);
            }
            return false;
        }
        it->second.version++;
        it->second.expires_at_ms = ttl_secs == 0 ? now : now + ttl_secs * 1000;
        if (ttl_secs > 0) {
            uint32_t slot = (uint32_t)((it->second.expires_at_ms / 1000) % WHEEL);
            std::lock_guard<std::mutex> wlock(wheel_mu_);
            wheel_[slot].push_back(CacheMark{(uint32_t)(shard_index(name)), name, it->second.version,
                                            it->second.expires_at_ms});
        }
        return true;
    }

    /// Drop every entry due at `now_ms`. The background sweeper calls this
    /// with the real clock; tests call it with a fake one, which is what
    /// makes expiration deterministic without sleeping.
    ///
    /// Every slot from behind up to now is swept, not just now's: a stalled
    /// sweeper must catch up rather than let skipped slots linger, and jumped
    /// clocks (tests, NTP steps) land whole seconds ahead. Marks that are not
    /// due go back where they were found; stale marks are dropped.
    int64_t expire_due(int64_t now_ms) {
        int64_t cur = now_ms / 1000;
        int64_t lo;
        {
            std::lock_guard<std::mutex> wlock(wheel_mu_);
            lo = last_swept_sec_ < cur ? std::max(last_swept_sec_, cur - (int64_t)WHEEL) : cur - (int64_t)WHEEL;
            last_swept_sec_ = cur;
        }
        // Collect first, process after: the wheel lock is never held while a
        // shard lock is taken (set() takes them in the other order).
        std::vector<CacheMark> marks;
        {
            std::lock_guard<std::mutex> wlock(wheel_mu_);
            for (int64_t s = lo + 1; s <= cur; s++) {
                uint32_t slot = (uint32_t)(((s % (int64_t)WHEEL) + (int64_t)WHEEL) % (int64_t)WHEEL);
                auto& bucket = wheel_[slot];
                marks.insert(marks.end(), bucket.begin(), bucket.end());
                bucket.clear();
            }
        }
        std::vector<CacheMark> future;
        int64_t n = 0;
        for (const auto& m : marks) {
            CacheShard& s = shards_[m.shard % SHARDS];
            std::lock_guard<std::mutex> lock(s.mu);
            auto it = s.map.find(m.key);
            // A mark is stale when the entry is gone or was rewritten (the
            // version moved on); either way it is garbage, not work.
            if (it == s.map.end() || it->second.version != m.version) continue;
            if (it->second.expires_at_ms == 0 || it->second.expires_at_ms > now_ms) {
                future.push_back(m);
                continue;
            }
            s.map.erase(it);
            evictions_.fetch_add(1, std::memory_order_relaxed);
            n++;
        }
        if (!future.empty()) {
            std::lock_guard<std::mutex> wlock(wheel_mu_);
            for (const auto& m : future) {
                uint32_t slot = (uint32_t)(((m.expires_at_ms / 1000) % (int64_t)WHEEL + (int64_t)WHEEL) %
                                           (int64_t)WHEEL);
                wheel_[slot].push_back(m);
            }
        }
        return n;
    }

    // -- counters, for metrics and tests --
    uint64_t hits() const { return hits_.load(std::memory_order_relaxed); }
    uint64_t misses() const { return misses_.load(std::memory_order_relaxed); }
    uint64_t evictions() const { return evictions_.load(std::memory_order_relaxed); }
    uint64_t sets() const { return sets_.load(std::memory_order_relaxed); }
    size_t shard_count(const std::string& name) {
        CacheShard& s = shard_for(name);
        std::lock_guard<std::mutex> lock(s.mu);
        return s.map.size();
    }
    /// Every live entry, expired ones included until something reads or sweeps
    /// them: what a box has to hold right now.
    size_t live_entries() {
        size_t n = 0;
        for (uint32_t i = 0; i < SHARDS; i++) {
            std::lock_guard<std::mutex> lock(shards_[i].mu);
            n += shards_[i].map.size();
        }
        return n;
    }

  private:
    static bool expired(const CacheEntry& e, int64_t now_ms) {
        return e.expires_at_ms > 0 && e.expires_at_ms <= now_ms;
    }
    static uint32_t shard_index(const std::string& name) {
        return (uint32_t)(std::hash<std::string>{}(name) % SHARDS);
    }
    CacheShard& shard_for(const std::string& name) { return shards_[shard_index(name)]; }

    CacheShard shards_[SHARDS];
    mutable std::mutex defaults_mu_;
    std::unordered_map<std::string, int64_t> defaults_;
    std::mutex wheel_mu_;
    std::vector<std::vector<CacheMark>> wheel_;
    /// Last second swept, so jumps sweep the range instead of one slot.
    int64_t last_swept_sec_ = -1;
    std::atomic<uint64_t> hits_{0};
    std::atomic<uint64_t> misses_{0};
    std::atomic<uint64_t> evictions_{0};
    std::atomic<uint64_t> sets_{0};
};

/// The ambient default cache, shared by every thread.
inline Cache& cache_default();

/// Owns the sweeper thread next to the cache it sweeps, so destruction stops
/// the thread before the memory goes away. A detached immortal thread races
/// static destruction at exit (ThreadSanitizer caught it sweeping a freed
/// wheel); a joined shutdown cannot.
struct CacheSweeper {
    std::atomic<bool> stop{false};
    std::mutex wake_mu;
    std::condition_variable wake;
    std::thread worker;
    std::once_flag started;

    ~CacheSweeper() {
        stop.store(true, std::memory_order_relaxed);
        wake.notify_all();
        if (worker.joinable()) worker.join();
    }

    void start() {
        std::call_once(started, [this] {
            worker = std::thread([this] {
                for (;;) {
                    std::unique_lock<std::mutex> lock(wake_mu);
                    // A second between sweeps, or no wait at all when the
                    // process is going away.
                    if (wake.wait_for(lock, std::chrono::seconds(1), [this] {
                            return stop.load(std::memory_order_relaxed);
                        }))
                        return;
                    lock.unlock();
                    cache_default().expire_due(cache_now_ms());
                }
            });
        });
    }
};

/// The ambient runtime: the default cache and its sweeper, in an order that
/// destroys safely. One static instead of two, because two statics destroy in
/// whichever order they happened to construct in, and the wrong order is a
/// thread sweeping freed memory.
struct CacheRuntime {
    Cache cache;
    CacheSweeper sweeper;
};

inline CacheRuntime& cache_runtime() {
    static CacheRuntime r;
    return r;
}

inline Cache& cache_default() {
    return cache_runtime().cache;
}

/// Set by every entry point below, so `/metrics` can report cache numbers
/// without constructing the cache a program never used.
inline std::atomic<bool>& cache_used_flag() {
    static std::atomic<bool> f{false};
    return f;
}
inline bool cache_used() { return cache_used_flag().load(std::memory_order_relaxed); }

/// Start the background sweeper once per process: every second it drops
/// whatever the wheel says is due. Shutdown joins it before the cache it
/// sweeps is destroyed.
inline void cache_start_sweeper() {
    cache_used_flag().store(true, std::memory_order_relaxed);
    cache_runtime().sweeper.start();
}

// ---------------------------------------------------------------------------
// Language surface: `cache.declare`, `cache.get`, ...
//
// Every function takes plain values and returns `Val`, so generated code is
// one call per operation with no glue. Names arrive as text; anything else
// is a usage error, named as one.
// ---------------------------------------------------------------------------

inline std::string cache_require_name(const Val& k, const char* op) {
    if (!k.is_str() || k.sv.empty())
        throw std::runtime_error(std::string("cache: ") + op + " needs a name as text");
    return k.sv;
}

inline Val cache_declare(const Val& name, const Val& ttl_secs) {
    std::string n = cache_require_name(name, "declare");
    if (!ttl_secs.is_int() || ttl_secs.iv < -1)
        throw std::runtime_error("cache: declare needs TTL seconds (-1 persists)");
    cache_start_sweeper();
    cache_default().declare(n, ttl_secs.iv);
    return Val::boolean(true);
}

inline Val cache_get(const Val& k) {
    cache_start_sweeper();
    return cache_default().get(cache_require_name(k, "get"));
}

inline Val cache_set(const Val& k, const Val& v, const Val& ttl_secs) {
    std::string n = cache_require_name(k, "set");
    int64_t ttl = -2;
    if (!ttl_secs.is_nil()) {
        if (!ttl_secs.is_int() || ttl_secs.iv < 0)
            throw std::runtime_error("cache: set TTL seconds must be 0 or more");
        ttl = ttl_secs.iv;
    }
    cache_start_sweeper();
    cache_default().set(n, v, ttl);
    return Val::boolean(true);
}

/// Two-argument form: the declared default TTL applies, or persist.
inline Val cache_set(const Val& k, const Val& v) {
    return cache_set(k, v, Val::nil());
}

inline Val cache_delete(const Val& k) {
    return Val::boolean(cache_default().del(cache_require_name(k, "delete")));
}

inline Val cache_exists(const Val& k) {
    return Val::boolean(cache_default().exists(cache_require_name(k, "exists")));
}

inline Val cache_increment(const Val& k, const Val& step) {
    std::string n = cache_require_name(k, "increment");
    if (!step.is_int())
        throw std::runtime_error("cache: increment step must be an integer");
    return Val::int_(cache_default().incr(n, step.iv));
}

inline Val cache_decrement(const Val& k, const Val& step) {
    std::string n = cache_require_name(k, "decrement");
    if (!step.is_int())
        throw std::runtime_error("cache: decrement step must be an integer");
    return Val::int_(cache_default().decr(n, step.iv));
}

inline Val cache_clear() {
    return Val::int_((int64_t)cache_default().clear());
}

inline Val cache_keys() {
    std::vector<Val> out;
    for (const auto& k : cache_default().keys()) out.push_back(Val::text(k));
    return Val::list(std::move(out));
}

inline Val cache_ttl(const Val& k) {
    return Val::int_(cache_default().ttl(cache_require_name(k, "ttl")));
}

inline Val cache_expire(const Val& k, const Val& ttl_secs) {
    std::string n = cache_require_name(k, "expire");
    if (!ttl_secs.is_int() || ttl_secs.iv < 0)
        throw std::runtime_error("cache: expire TTL seconds must be 0 or more");
    return Val::boolean(cache_default().expire(n, ttl_secs.iv));
}

}  // namespace hs

#endif  // HS_RUNTIME_CACHE_HPP
