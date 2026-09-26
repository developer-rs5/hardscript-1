// hs_runtime_metrics.hpp -- metrics and health endpoints (M6.7)
//
// Counters, gauges and latency histograms in-process, exposed three ways:
// Prometheus text at `GET /metrics`, a JSON snapshot for a program that wants
// to serve the numbers itself, and `/healthz` (is this process alive) plus
// `/readyz` (should a load balancer send it traffic).
//
// Nothing here is a background thread or a timer. Requests are timed in the
// dispatch wrapper that already exists for them, and the built-in series for
// the cache, queue, rate limiter and mailer are read from the counters those
// subsystems already keep -- and only once a program has actually touched
// them, so polling `/metrics` never wakes a subsystem the app does not use.

#ifndef HS_RUNTIME_METRICS_HPP
#define HS_RUNTIME_METRICS_HPP

#include "hs_runtime_cache.hpp"
#include "hs_runtime_email.hpp"
#include "hs_runtime_http.hpp"
#include "hs_runtime_io.hpp"
#include "hs_runtime_util.hpp"
#include "hs_runtime_queue.hpp"
#include "hs_runtime_ratelimit.hpp"
#include "hs_runtime_value.hpp"

#include <array>
#include <atomic>
#include <chrono>
#include <cmath>
#include <map>
#include <mutex>
#include <string>
#include <vector>

namespace hs {

// ---------------------------------------------------------------------------
// Series
// ---------------------------------------------------------------------------

enum class MetricKind { Counter, Gauge, Histogram };

/// Request latencies, in milliseconds. The bounds are the ones an operator
/// actually asks about: sub-millisecond, then each rough order of magnitude.
extern const double kHsLatencyBuckets[];
constexpr size_t kHsLatencyBucketCount = 14;

/// One series: a name, optional pre-rendered labels, and a kind. Values live
/// in atomics, so recording a request never takes the registry lock.
struct Metric {
    std::string name;
    std::string labels;  // already rendered: `{method="GET",path="/a"}`
    std::string help;
    MetricKind kind = MetricKind::Counter;

    std::atomic<uint64_t> counter{0};
    // Gauges keep millis as an integer so the value can be atomic: there is no
    // `std::atomic<double>` to lean on in C++17.
    std::atomic<int64_t> gauge_millis{0};
    // A fixed array, not a vector: `std::atomic` is neither copyable nor
    // movable, so a vector of them cannot even grow.
    std::array<std::atomic<uint64_t>, kHsLatencyBucketCount> buckets{};
    std::atomic<uint64_t> hist_count{0};
    std::atomic<int64_t> hist_sum_millis{0};

    void add(uint64_t n) { counter.fetch_add(n, std::memory_order_relaxed); }
    void set_millis(int64_t m) { gauge_millis.store(m, std::memory_order_relaxed); }
    void observe_millis(int64_t m) {
        hist_count.fetch_add(1, std::memory_order_relaxed);
        hist_sum_millis.fetch_add(m, std::memory_order_relaxed);
        for (size_t i = 0; i < buckets.size(); i++)
            if ((double)m <= kHsLatencyBuckets[i]) {
                buckets[i].fetch_add(1, std::memory_order_relaxed);
                break;
            }
    }
};

/// Escape a label value for the Prometheus text format: backslash, quote and
/// newline only, which is all the format defines.
inline std::string metric_escape(const std::string& v) {
    std::string out;
    for (char c : v) {
        if (c == '\\' || c == '"') out.push_back('\\');
        if (c == '\n') {
            out += "\\n";
            continue;
        }
        out.push_back(c);
    }
    return out;
}

/// A metric name may not carry Prometheus' label punctuation; a user metric
/// named `a.b:c` is fixed rather than rejected, so a typo still shows up.
inline std::string metric_sanitize(const std::string& v) {
    std::string out;
    for (char c : v) {
        if ((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_')
            out.push_back(c);
        else
            out.push_back('_');
    }
    if (out.empty() || (out[0] >= '0' && out[0] <= '9')) out.insert(out.begin(), '_');
    return out;
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Distinct label sets are capped: `/users/1`, `/users/2`, ... would otherwise
/// turn a public endpoint into a memory leak, and the folded series keeps the
/// cardinality visible instead of silent.
constexpr size_t kHsMaxSeries = 4096;

class MetricsRegistry {
  public:
    /// The counter/gauge/histogram for a series, creating it on first use.
    /// Returns null once the cap is hit, and the caller records the overflow
    /// instead: a metric that cannot be stored is still counted as overflow.
    std::shared_ptr<Metric> series(const std::string& name, const std::string& labels,
                                   MetricKind kind, const std::string& help = "") {
        std::lock_guard<std::mutex> lock(mu_);
        std::string key = name + "\x1f" + labels;
        auto it = index_.find(key);
        if (it != index_.end()) return by_ptr_[it->second];
        if (by_ptr_.size() >= kHsMaxSeries) {
            overflow_.fetch_add(1, std::memory_order_relaxed);
            return nullptr;
        }
        auto m = std::make_shared<Metric>();
        m->name = name;
        m->labels = labels;
        m->help = help;
        m->kind = kind;
        by_ptr_.push_back(m);
        order_.push_back(key);
        index_[key] = by_ptr_.size() - 1;
        return m;
    }

    void incr(const std::string& name, const std::string& labels, uint64_t by = 1) {
        if (auto m = series(name, labels, MetricKind::Counter)) m->add(by);
    }

    void set_millis(const std::string& name, const std::string& labels, double v) {
        if (auto m = series(name, labels, MetricKind::Gauge))
            m->set_millis((int64_t)llround(v * 1000.0));
    }

    void observe_millis(const std::string& name, const std::string& labels, double v) {
        if (auto m = series(name, labels, MetricKind::Histogram))
            m->observe_millis((int64_t)llround(v));
    }

    /// Current value of a user series: nil when it was never recorded, which
    /// is different from a recorded zero.
    bool value(const std::string& name, double& out) {
        std::lock_guard<std::mutex> lock(mu_);
        auto it = index_.find(name + "\x1f");
        if (it == index_.end()) return false;
        const Metric& m = *by_ptr_[it->second];
        switch (m.kind) {
            case MetricKind::Counter:
                out = (double)m.counter.load(std::memory_order_relaxed);
                return true;
            case MetricKind::Gauge:
                out = (double)m.gauge_millis.load(std::memory_order_relaxed) / 1000.0;
                return true;
            case MetricKind::Histogram:
                out = (double)m.hist_count.load(std::memory_order_relaxed);
                return true;
        }
        return false;
    }

    /// Every series in creation order, so two scrapes differ only in values.
    std::vector<std::shared_ptr<Metric>> snapshot() {
        std::lock_guard<std::mutex> lock(mu_);
        std::vector<std::shared_ptr<Metric>> out;
        out.reserve(by_ptr_.size());
        for (auto& m : by_ptr_) out.push_back(m);
        return out;
    }

    uint64_t overflow() const { return overflow_.load(std::memory_order_relaxed); }

  private:
    std::mutex mu_;
    std::vector<std::shared_ptr<Metric>> by_ptr_;
    std::vector<std::string> order_;
    std::map<std::string, size_t> index_;
    std::atomic<uint64_t> overflow_{0};
};

inline MetricsRegistry& metrics_registry() {
    static MetricsRegistry r;
    return r;
}

inline const double kHsLatencyBuckets[kHsLatencyBucketCount] = {0.5,   1,    2.5,   5,    10,
                                                                25,    50,   100,  250,  500,
                                                                1000,  2500, 5000, 10000};

// ---------------------------------------------------------------------------
// Request instrumentation
// ---------------------------------------------------------------------------

/// Set by `metrics_install`; the dispatch wrapper calls it once per request
/// with the finished response and its latency. Null when a program never used
/// metrics, and then dispatch skips its timing entirely.

/// Record one finished request. The dispatch wrapper measured it; all that is
/// left is to name the series, and to keep the name from growing without
/// bound: `/users/1`, `/users/2`, ... would otherwise be a memory leak served
/// from a public endpoint.
inline void metrics_request_end(const Request& req, const Response& res, double ms) {
    std::string path(req.path);
    if (path.size() > 200) path.resize(200);
    std::string method = metric_escape(req.method);
    std::string labels = "{method=\"" + method + "\",path=\"" + metric_escape(path) +
                         "\",status=\"" + std::to_string(res.status) + "\"}";
    metrics_registry().incr("hs_requests_total", labels);
    metrics_registry().observe_millis("hs_request_duration_ms",
                                      "{method=\"" + method + "\",path=\"" + metric_escape(path) +
                                          "\"}",
                                      ms);
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// A readiness gate a program sets itself: `health.set("redis", ok)`.
struct HealthGate {
    std::string name;
    bool ok = true;
    std::string detail;
    int64_t updated_ms = 0;
};

inline std::mutex& health_mu() {
    static std::mutex m;
    return m;
}

inline std::map<std::string, HealthGate>& health_gates() {
    static std::map<std::string, HealthGate> g;
    return g;
}

inline void health_set(const std::string& name, bool ok, const std::string& detail = "") {
    if (name.empty()) throw std::runtime_error("health: a check needs a name");
    std::lock_guard<std::mutex> lock(health_mu());
    HealthGate& g = health_gates()[name];
    g.name = name;
    g.ok = ok;
    g.detail = detail;
    g.updated_ms = unix_ms();
}

inline int64_t metrics_start_ms() {
    static int64_t t = unix_ms();
    return t;
}

inline double metrics_uptime_ms() { return (double)(unix_ms() - metrics_start_ms()); }

/// Liveness: the process is up and serving. It says nothing about the
/// database, because a request that cannot reach one still gets an answer.
inline Val metrics_health_payload() {
    Val out = Val::object({{"status", Val::text("ok")},
                            {"uptime_ms", Val::int_((int64_t)metrics_uptime_ms())},
                            {"pid", Val::text(pid())},
                            {"version", Val::text("0.7.0")}});
    return out;
}

/// Readiness: should traffic arrive? A process on its way down says no, so a
/// load balancer drains it before the listener closes. Every gate the program
/// set is reported, and any subsystem that is in use must still answer.
inline bool metrics_ready(Val& detail) {
    std::vector<std::pair<std::string, Val>> checks;
    bool ok = true;
    if (g_hs_stop.load()) {
        checks.emplace_back("shutting_down", Val::boolean(false));
        ok = false;
    } else {
        checks.emplace_back("shutting_down", Val::boolean(true));
    }
    // A subsystem in use has to still be answering: an unreachable SQL queue
    // means this process cannot do the work it accepted.
    if (queue_used()) {
        bool qok = true;
        std::string qdetail;
        try {
            queue_runtime().depth("");
        } catch (const std::exception& e) {
            qok = false;
            qdetail = e.what();
        }
        checks.emplace_back("queue", Val::object({{"ok", Val::boolean(qok)},
                                                  {"detail", Val::text(qdetail)}}));
        ok = ok && qok;
    }
    std::lock_guard<std::mutex> lock(health_mu());
    for (const auto& kv : health_gates()) {
        checks.emplace_back(kv.first, Val::object({{"ok", Val::boolean(kv.second.ok)},
                                                   {"detail", Val::text(kv.second.detail)},
                                                   {"age_ms", Val::int_(unix_ms() - kv.second.updated_ms)}}));
        ok = ok && kv.second.ok;
    }
    detail = Val::object({{"status", Val::text(ok ? "ready" : "not_ready")},
                          {"checks", Val::object(std::move(checks))}});
    return ok;
}

/// The readiness payload as a value, for a program that wants to answer
/// `/ready` itself. `health.ready()` is this.
inline Val metrics_ready_json() {
    Val detail;
    metrics_ready(detail);
    return detail;
}

// ---------------------------------------------------------------------------
// Exposition
// ---------------------------------------------------------------------------

inline std::string metric_number(double v) {
    if (v == (double)(int64_t)v && std::fabs(v) < 9.0e15) return std::to_string((int64_t)v);
    char b[40];
    std::snprintf(b, sizeof b, "%g", v);
    return b;
}

/// Prometheus text exposition, the format every scraper already speaks.
inline std::string metrics_prometheus_text() {
    std::string out;
    // `suffix` is what a scraper looks for: a histogram's buckets are
    // `<name>_bucket`, its total `<name>_count`.
    auto series_line = [&](const Metric& m, const char* suffix, const std::string& label_override,
                           double value) {
        out += m.name;
        out += suffix;
        out += label_override.empty() ? m.labels : label_override;
        out += " ";
        out += metric_number(value);
        out += "\n";
    };
    for (const auto& m : metrics_registry().snapshot()) {
        if (!m->help.empty()) {
            out += "# HELP " + m->name + " " + m->help + "\n";
        }
        out += std::string("# TYPE ") + m->name +
               (m->kind == MetricKind::Counter
                    ? " counter\n"
                    : (m->kind == MetricKind::Gauge ? " gauge\n" : " histogram\n"));
        switch (m->kind) {
            case MetricKind::Counter:
                series_line(*m, "", "", (double)m->counter.load(std::memory_order_relaxed));
                break;
            case MetricKind::Gauge:
                series_line(*m, "", "",
                            (double)m->gauge_millis.load(std::memory_order_relaxed) / 1000.0);
                break;
            case MetricKind::Histogram: {
                // The bucket bound is one more label on the series, and a
                // series with no labels still needs its opening brace.
                auto with_le = [](const std::string& base, const std::string& bound) {
                    if (base.empty()) return "{le=\"" + bound + "\"}";
                    std::string out = base.substr(0, base.size() - 1);
                    return out + ",le=\"" + bound + "\"}";
                };
                uint64_t cumulative = 0;
                for (size_t i = 0; i < m->buckets.size(); i++) {
                    cumulative += m->buckets[i].load(std::memory_order_relaxed);
                    series_line(*m, "_bucket", with_le(m->labels, metric_number(kHsLatencyBuckets[i])),
                                (double)cumulative);
                }
                uint64_t total = m->hist_count.load(std::memory_order_relaxed);
                series_line(*m, "_bucket", with_le(m->labels, "+Inf"), (double)total);
                out += m->name + "_sum" + m->labels + " " +
                       metric_number((double)m->hist_sum_millis.load(std::memory_order_relaxed)) +
                       "\n";
                out += m->name + "_count" + m->labels + " " + std::to_string(total) + "\n";
                break;
            }
        }
    }
    // Built-in runtime series, appended only for the subsystems this program
    // has actually used: a number nobody asked for is noise, and reading a
    // counter must not construct the subsystem behind it.
    if (cache_used()) {
        Cache& c = cache_default();
        out += "# HELP hs_cache_hits_total Cache reads served from an entry.\n";
        out += "# TYPE hs_cache_hits_total counter\n";
        out += "hs_cache_hits_total " + std::to_string(c.hits()) + "\n";
        out += "# HELP hs_cache_misses_total Cache reads that found nothing.\n";
        out += "# TYPE hs_cache_misses_total counter\n";
        out += "hs_cache_misses_total " + std::to_string(c.misses()) + "\n";
        out += "# HELP hs_cache_evictions_total Entries dropped for capacity.\n";
        out += "# TYPE hs_cache_evictions_total counter\n";
        out += "hs_cache_evictions_total " + std::to_string(c.evictions()) + "\n";
        out += "# HELP hs_cache_sets_total Writes that stored a value.\n";
        out += "# TYPE hs_cache_sets_total counter\n";
        out += "hs_cache_sets_total " + std::to_string(c.sets()) + "\n";
    }
    if (queue_used()) {
        QueueRuntime& q = queue_runtime();
        out += "# HELP hs_queue_enqueued_total Jobs accepted onto a queue.\n";
        out += "# TYPE hs_queue_enqueued_total counter\n";
        out += "hs_queue_enqueued_total " + std::to_string(q.enqueued()) + "\n";
        out += "# HELP hs_queue_completed_total Jobs that finished without error.\n";
        out += "# TYPE hs_queue_completed_total counter\n";
        out += "hs_queue_completed_total " + std::to_string(q.completed()) + "\n";
        out += "# HELP hs_queue_failed_total Job attempts that failed.\n";
        out += "# TYPE hs_queue_failed_total counter\n";
        out += "hs_queue_failed_total " + std::to_string(q.failed()) + "\n";
    }
    if (ratelimit_used()) {
        out += "# HELP hs_ratelimit_allowed_total Checks that let a request through.\n";
        out += "# TYPE hs_ratelimit_allowed_total counter\n";
        out += "hs_ratelimit_allowed_total " + std::to_string(ratelimit_allowed()) + "\n";
        out += "# HELP hs_ratelimit_denied_total Checks that shed a request with 429.\n";
        out += "# TYPE hs_ratelimit_denied_total counter\n";
        out += "hs_ratelimit_denied_total " + std::to_string(ratelimit_denied()) + "\n";
    }
    if (email_used()) {
        out += "# HELP hs_email_sent_total Messages handed to an SMTP server.\n";
        out += "# TYPE hs_email_sent_total counter\n";
        out += "hs_email_sent_total " + std::to_string(email_sent()) + "\n";
        out += "# HELP hs_email_failed_total Sends that raised, sync or queued.\n";
        out += "# TYPE hs_email_failed_total counter\n";
        out += "hs_email_failed_total " + std::to_string(email_failed()) + "\n";
    }
    out += "# HELP hs_process_uptime_ms How long this process has been serving.\n";
    out += "# TYPE hs_process_uptime_ms gauge\n";
    out += "hs_process_uptime_ms " + metric_number(metrics_uptime_ms()) + "\n";
    uint64_t over = metrics_registry().overflow();
    if (over) {
        out += "# HELP hs_metrics_series_dropped Series refused after the cardinality cap.\n";
        out += "# TYPE hs_metrics_series_dropped counter\n";
        out += "hs_metrics_series_dropped " + std::to_string(over) + "\n";
    }
    return out;
}

/// The same numbers as a value, for a program that would rather serve them
/// from its own route (or log them) than hand out Prometheus text.
inline Val metrics_snapshot_json() {
    Val series = Val::list({});
    for (const auto& m : metrics_registry().snapshot()) {
        Val entry = Val::object({{"name", Val::text(m->name)},
                                 {"labels", Val::text(m->labels)},
                                 {"value", Val::flt(0)}});
        double v = 0;
        switch (m->kind) {
            case MetricKind::Counter:
                v = (double)m->counter.load(std::memory_order_relaxed);
                break;
            case MetricKind::Gauge:
                v = (double)m->gauge_millis.load(std::memory_order_relaxed) / 1000.0;
                break;
            case MetricKind::Histogram:
                v = (double)m->hist_count.load(std::memory_order_relaxed);
                break;
        }
        for (auto& kv : entry.obj)
            if (kv.first == "value") kv.second = Val::flt(v);
        series.arr.push_back(entry);
    }
    return Val::object({{"uptime_ms", Val::int_((int64_t)metrics_uptime_ms())},
                        {"series", std::move(series)}});
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

/// Register `GET /metrics`, `/healthz` and `/readyz` on this server, and time
/// every request from here on. Called once per program, from `main`; calling
/// it twice on one server is harmless rather than clever -- the routes would
/// answer identically, and a process-wide "already installed" flag would leave
/// a second server with no endpoints at all.
inline void metrics_install(Server& app) {
    app.handle("GET", "/metrics", [](const Request&) {
        Response r = Response::text(metrics_prometheus_text());
        r.ctype = "text/plain; version=0.0.4; charset=utf-8";
        return r;
    });
    app.handle("GET", "/healthz", [](const Request&) { return Response::json(metrics_health_payload()); });
    app.handle("GET", "/readyz", [](const Request&) {
        Val detail;
        bool ok = metrics_ready(detail);
        Response r = Response::json(detail);
        if (!ok) r.status = 503;
        return r;
    });
    hs_request_done_hook = [](const Request& req, const Response& res, double ms) {
        metrics_request_end(req, res, ms);
    };
}

// ---------------------------------------------------------------------------
// Language surface
// ---------------------------------------------------------------------------

inline std::string metrics_require_name(const Val& v, const char* op) {
    if (!v.is_str() || v.sv.empty())
        throw std::runtime_error(std::string("metrics: ") + op + " needs a name as text");
    return metric_sanitize(v.sv);
}

inline Val metrics_inc(const Val& name, const Val& by) {
    std::string n = metrics_require_name(name, "incr");
    int64_t step = 1;
    if (!by.is_nil()) {
        if (!by.is_num()) throw std::runtime_error("metrics: incr needs a number to add");
        step = by.as_int();
    }
    metrics_registry().incr(n, "", step > 0 ? (uint64_t)step : 0);
    return Val::int_(step);
}

inline Val metrics_set(const Val& name, const Val& v) {
    std::string n = metrics_require_name(name, "set");
    if (!v.is_num()) throw std::runtime_error("metrics: set needs a number");
    metrics_registry().set_millis(n, "", v.num());
    return v;
}

inline Val metrics_observe(const Val& name, const Val& v) {
    std::string n = metrics_require_name(name, "observe");
    if (!v.is_num()) throw std::runtime_error("metrics: observe needs a number");
    metrics_registry().observe_millis(n, "", v.num());
    return v;
}

inline Val metrics_value(const Val& name) {
    std::string n = metrics_require_name(name, "value");
    double v = 0;
    if (!metrics_registry().value(n, v)) return Val::nil();
    return Val::flt(v);
}

/// `health.set("redis", false, "connection refused")`: the detail is what a
/// readiness probe shows the operator, so it is worth carrying.
inline Val health_set_val(const Val& name, const Val& ok, const Val& detail = Val::nil()) {
    if (!name.is_str() || name.sv.empty())
        throw std::runtime_error("health: set needs a check name as text");
    if (!ok.is_bool()) throw std::runtime_error("health: set needs true or false");
    std::string why;
    if (!detail.is_nil()) {
        if (!detail.is_str()) throw std::runtime_error("health: the detail must be text");
        why = detail.sv;
    }
    health_set(name.sv, ok.bv, why);
    return ok;
}

}  // namespace hs

#endif  // HS_RUNTIME_METRICS_HPP
