// hs_runtime_ratelimit.hpp -- rate limiter (M6.5)
//
// `limit 100 requests / minute` caps how often work happens: token bucket by
// default (bursts welcome), sliding window with `sliding`, keyed by client IP
// unless `key <expr>` says otherwise. Over the limit, the request ends with
// 429 and a Retry-After: shedding load before it reaches handlers is the
// whole point.
//
// Buckets live in 64 mutex-guarded shards keyed by limiter plus key, so
// threads share nothing global. Entries idle out: a shard past its cap sheds
// buckets that could not possibly deny (full token buckets, empty windows),
// which bounds memory without a sweeper thread. Counters feed metrics.

#ifndef HS_RUNTIME_RATELIMIT_HPP
#define HS_RUNTIME_RATELIMIT_HPP

#include "hs_runtime_http.hpp"
#include "hs_runtime_value.hpp"

#include <atomic>
#include <chrono>
#include <cstdint>
#include <deque>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <unordered_map>
#include <vector>

namespace hs {

inline std::atomic<int64_t>& limit_test_clock() {
    static std::atomic<int64_t> override_ms{-1};
    return override_ms;
}

inline int64_t limit_now_ms() {
    int64_t fake = limit_test_clock().load(std::memory_order_relaxed);
    if (fake >= 0) return fake;
    return (int64_t)std::chrono::duration_cast<std::chrono::milliseconds>(
               std::chrono::steady_clock::now().time_since_epoch())
        .count();
}

inline void limit_set_now_for_tests(int64_t ms) {
    limit_test_clock().store(ms, std::memory_order_relaxed);
}

inline void limit_clear_now_for_tests() {
    limit_test_clock().store(-1, std::memory_order_relaxed);
}

/// 0 is a token bucket, anything else a sliding window. One integer travels
/// from the desugared `limit` statement so the two never mix up.
constexpr int LIMIT_TOKEN_BUCKET = 0;
constexpr int LIMIT_SLIDING_WINDOW = 1;

/// A bucket both algorithms share: tokens for one, timestamps for the other.
/// Whichever the limiter runs, the other field sits empty.
struct LimitBucket {
    bool init = false;
    double tokens = 0;
    int64_t updated_ms = 0;
    std::deque<int64_t> hits;
};

struct LimitVerdict {
    bool allowed = false;
    /// Milliseconds until a retry could succeed. Zero when allowed.
    int64_t retry_after_ms = 0;
};

struct LimitShard {
    std::mutex mu;
    std::unordered_map<std::string, LimitBucket> buckets;
};

/// One limiter: an algorithm, a budget per window, and sharded buckets.
class RateLimiter {
  public:
    RateLimiter() = default;
    RateLimiter(int algo, int64_t limit, int64_t window_ms)
        : algo_(algo), limit_(limit), window_ms_(window_ms) {}

    LimitVerdict check(const std::string& key, int64_t now_ms) {
        LimitShard& s = shard_for(key);
        std::lock_guard<std::mutex> lock(s.mu);
        // Distinct keys accumulate forever otherwise: past the cap, shed
        // buckets that could not possibly deny (full token buckets, drained
        // windows). An attacker minting keys buys memory up to the cap, not
        // without bound.
        if (s.buckets.size() >= SHARD_CAP) sweep_shard(s, now_ms);
        LimitBucket& b = s.buckets[key];
        // First touch starts full: a fresh bucket has its whole budget, which
        // is what makes the very first request succeed.
        if (!b.init) {
            b.init = true;
            b.tokens = (double)limit_;
            b.updated_ms = now_ms;
        }
        LimitVerdict v;
        if (algo_ == LIMIT_TOKEN_BUCKET) {
            double rate = (double)limit_ / (double)window_ms_;
            b.tokens += (double)(now_ms - b.updated_ms) * rate;
            if (b.tokens > (double)limit_) b.tokens = (double)limit_;
            b.updated_ms = now_ms;
            if (b.tokens >= 1.0) {
                b.tokens -= 1.0;
                v.allowed = true;
                return v;
            }
            v.retry_after_ms = (int64_t)((1.0 - b.tokens) / rate);
            if (v.retry_after_ms < 0) v.retry_after_ms = 0;
            return v;
        }
        while (!b.hits.empty() && b.hits.front() <= now_ms - window_ms_) b.hits.pop_front();
        if ((int64_t)b.hits.size() < limit_) {
            b.hits.push_back(now_ms);
            v.allowed = true;
            return v;
        }
        v.retry_after_ms = b.hits.front() + window_ms_ - now_ms;
        if (v.retry_after_ms < 0) v.retry_after_ms = 0;
        return v;
    }

    void sweep_shard(LimitShard& s, int64_t now_ms) {
        for (auto it = s.buckets.begin(); it != s.buckets.end();) {
            bool idle = false;
            if (algo_ == LIMIT_TOKEN_BUCKET) {
                double rate = (double)limit_ / (double)window_ms_;
                double tokens = it->second.tokens + (double)(now_ms - it->second.updated_ms) * rate;
                idle = tokens >= (double)limit_;
            } else {
                while (!it->second.hits.empty() && it->second.hits.front() <= now_ms - window_ms_) {
                    it->second.hits.pop_front();
                }
                idle = it->second.hits.empty();
            }
            if (idle) {
                it = s.buckets.erase(it);
            } else {
                ++it;
            }
        }
    }

    int algo() const { return algo_; }
    int64_t limit() const { return limit_; }
    int64_t window_ms() const { return window_ms_; }

    /// Live buckets across all shards. For tests and metrics: memory follows
    /// this number, so boundedness is observable rather than hoped for.
    size_t bucket_count() {
        size_t n = 0;
        for (uint32_t s = 0; s < SHARDS; s++) {
            std::lock_guard<std::mutex> lock(shards_[s].mu);
            n += shards_[s].buckets.size();
        }
        return n;
    }

    static constexpr uint32_t SHARDS = 64;

  private:
    static constexpr size_t SHARD_CAP = 4096;
    int algo_ = LIMIT_TOKEN_BUCKET;
    int64_t limit_ = 0;
    int64_t window_ms_ = 60000;

    LimitShard& shard_for(const std::string& key) {
        // Each limiter owns its shards, so keys alone address buckets.
        return shards_[std::hash<std::string>{}(key) % SHARDS];
    }

    LimitShard shards_[SHARDS];
};

/// The process-wide limiter registry, keyed by limiter identity. Specs update
/// in place: generated code is the source of truth, and a rebuild may change
/// budgets without restarting the process in tests.
class LimitRegistry {
  public:
    RateLimiter& get(const std::string& id, int algo, int64_t limit, int64_t window_ms) {
        std::lock_guard<std::mutex> lock(mu_);
        auto it = limiters_.find(id);
        if (it == limiters_.end()) {
            auto r = std::unique_ptr<RateLimiter>(new RateLimiter(algo, limit, window_ms));
            it = limiters_.emplace(id, std::move(r)).first;
        } else if (it->second->algo() != algo || it->second->limit() != limit ||
                   it->second->window_ms() != window_ms) {
            it->second.reset(new RateLimiter(algo, limit, window_ms));
        }
        return *it->second;
    }

    uint64_t allowed() const { return allowed_.load(std::memory_order_relaxed); }
    uint64_t denied() const { return denied_.load(std::memory_order_relaxed); }
    void count(bool ok) {
        if (ok) {
            allowed_.fetch_add(1, std::memory_order_relaxed);
        } else {
            denied_.fetch_add(1, std::memory_order_relaxed);
        }
    }

  private:
    mutable std::mutex mu_;
    std::map<std::string, std::unique_ptr<RateLimiter>> limiters_;
    std::atomic<uint64_t> allowed_{0};
    std::atomic<uint64_t> denied_{0};
};

inline LimitRegistry& limit_registry() {
    static LimitRegistry r;
    return r;
}

/// Set by the first check, so `/metrics` reports rate limits without building
/// a registry a program never used.
inline std::atomic<bool>& ratelimit_used_flag() {
    static std::atomic<bool> f{false};
    return f;
}
inline bool ratelimit_used() { return ratelimit_used_flag().load(std::memory_order_relaxed); }
inline uint64_t ratelimit_allowed() { return limit_registry().allowed(); }
inline uint64_t ratelimit_denied() { return limit_registry().denied(); }

/// The key when none was given: the client address, or one shared bucket for
/// traffic without a socket (in-process calls, tests).
inline std::string limit_key_or_ip(const Val& key, const std::string& ip) {
    if (key.is_str() && !key.sv.empty()) return key.sv;
    if (!ip.empty()) return ip;
    return "local";
}

/// Check and consume: true when the request may proceed. Pure logic over the
/// registry; the abort lives one layer up so tests assert verdicts without
/// throwing HTTP around.
inline LimitVerdict limit_check(const std::string& id, int algo, int64_t limit, int64_t window_ms,
                                const std::string& key) {
    if (limit <= 0) throw std::runtime_error("limit: budget must be 1 or more");
    if (window_ms <= 0) throw std::runtime_error("limit: window must be positive");
    if (algo != LIMIT_TOKEN_BUCKET && algo != LIMIT_SLIDING_WINDOW)
        throw std::runtime_error("limit: unknown algorithm");
    ratelimit_used_flag().store(true, std::memory_order_relaxed);
    RateLimiter& lim = limit_registry().get(id, algo, limit, window_ms);
    LimitVerdict v = lim.check(key, limit_now_ms());
    limit_registry().count(v.allowed);
    return v;
}

/// Enforce a limit from generated code: allow, or end the request with 429
/// and a Retry-After in seconds (rounded up, minimum one).
[[noreturn]] inline void limit_deny(int64_t retry_after_ms) {
    Response r = Response::error(429, "rate limit exceeded");
    int64_t secs = (retry_after_ms + 999) / 1000;
    if (secs < 1) secs = 1;
    r.headers.push_back({"Retry-After", std::to_string(secs)});
    throw HttpAbort(std::move(r));
}

inline void limit_check_or_abort(const std::string& id, int algo, int64_t limit, int64_t window_ms,
                                 const std::string& key) {
    LimitVerdict v = limit_check(id, algo, limit, window_ms, key);
    if (!v.allowed) limit_deny(v.retry_after_ms);
}

}  // namespace hs

#endif  // HS_RUNTIME_RATELIMIT_HPP
