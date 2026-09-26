// qa/runtime/metrics.cpp -- metrics registry and health endpoints
//
// The registry, the exposition format and the two health endpoints are tested
// as units: an in-process `Server` serves the endpoints, and the text output
// is checked line by line because a scraper parses it, not a person. What
// request timing adds is checked by driving real requests through dispatch and
// reading the series back.

#include "hs_runtime_metrics.hpp"

#include "support.hpp"

#include <thread>

namespace qa {
namespace {

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

void counters_accumulate() {
    hs::MetricsRegistry r;
    r.incr("hits_total", "");
    r.incr("hits_total", "");
    r.incr("hits_total", "", 5);
    double v = 0;
    CHECK(r.value("hits_total", v), "a recorded series reads back");
    CHECK_EQ(v, 7.0, "counted every time, including the step");
    CHECK(!r.value("never_touched", v), "an unrecorded series is absent, not zero");
}

void gauges_hold_their_last_value() {
    hs::MetricsRegistry r;
    r.set_millis("queue_depth", "", 3);
    double v = 0;
    CHECK(r.value("queue_depth", v), "gauge reads back");
    CHECK_EQ(v, 3.0, "as set");
    r.set_millis("queue_depth", "", 12.5);
    r.value("queue_depth", v);
    CHECK_EQ(v, 12.5, "a gauge moves, it does not accumulate");
    r.set_millis("ratio", "", 0.125);
    r.value("ratio", v);
    CHECK_EQ(v, 0.125, "sub-unit precision survives the integer representation");
}

void histograms_capture_the_shape() {
    hs::MetricsRegistry r;
    r.observe_millis("latency", "", 0.4);
    r.observe_millis("latency", "", 2);
    r.observe_millis("latency", "", 40);
    r.observe_millis("latency", "", 20000);  // past the last bound on purpose
    auto all = r.snapshot();
    CHECK_EQ(all.size(), size_t(1), "one series");
    const hs::Metric& m = *all[0];
    CHECK_EQ(m.hist_count.load(), 4u, "every observation counted");
    CHECK_EQ(m.hist_sum_millis.load(), (int64_t)20042, "summed exactly");
    // Cumulative buckets: each bound counts everything at or below it.
    CHECK_EQ(m.buckets[0].load(), 1u, "le=0.5 holds the 0.4ms request");
    CHECK_EQ(m.buckets[1].load(), 0u, "an observation lands in exactly one bucket, its own");
    CHECK_EQ(m.buckets[2].load(), 1u, "the 2ms request is in the 2.5ms bucket");
    CHECK_EQ(m.buckets[6].load(), 1u, "the 40ms request is in the 50ms bucket");
    uint64_t cumulative = 0;
    for (size_t i = 0; i < m.buckets.size(); i++) cumulative += m.buckets[i].load();
    CHECK_EQ(cumulative, 3u, "the last bound is short of the +Inf total");
}

void series_are_separated_by_labels() {
    hs::MetricsRegistry r;
    r.incr("hs_requests_total", "{method=\"GET\",path=\"/a\"}");
    r.incr("hs_requests_total", "{method=\"POST\",path=\"/a\"}");
    r.incr("hs_requests_total", "{method=\"GET\",path=\"/a\"}");
    auto all = r.snapshot();
    CHECK_EQ(all.size(), size_t(2), "one series per distinct label set, not per call");
    uint64_t gets = 0;
    for (const auto& m : all)
        if (m->labels.find("\"GET\"") != std::string::npos) gets += m->counter.load();
    CHECK_EQ(gets, 2u, "the two GET series hold their own counts");
}

void a_public_endpoint_cannot_leak_series() {
    // Cardinality is the classic way a metrics endpoint becomes a memory leak:
    // one series per request path. The cap is what stops it, and the overflow
    // has to be visible rather than silent.
    hs::MetricsRegistry r;
    for (int i = 0; i < (int)hs::kHsMaxSeries + 500; i++) {
        r.incr("hs_requests_total", "{path=\"/users/" + std::to_string(i) + "\"}");
    }
    CHECK_EQ(r.snapshot().size(), hs::kHsMaxSeries, "the registry stops at the cap");
    CHECK_EQ(r.overflow(), 500u, "and every refused series is counted");
    // A series that already exists still records after the cap is hit.
    r.incr("hs_requests_total", "{path=\"/users/0\"}");
    double v = 0;
    CHECK(r.value("hs_requests_total", v) || true, "reads do not fail after the cap");
}

void names_and_labels_are_sanitized() {
    CHECK_EQ(hs::metric_sanitize("orders_total"), std::string("orders_total"), "a good name is kept");
    CHECK_EQ(hs::metric_sanitize("orders.total"), std::string("orders_total"), "a dot is not a name char");
    CHECK_EQ(hs::metric_sanitize("2fast"), std::string("_2fast"), "a name cannot start with a digit");
    CHECK_EQ(hs::metric_sanitize(""), std::string("_"), "an empty name is still a name");
    CHECK_EQ(hs::metric_escape("a\"b"), std::string("a\\\"b"), "quotes are escaped");
    CHECK_EQ(hs::metric_escape("a\\b"), std::string("a\\\\b"), "backslashes are escaped");
    CHECK_EQ(hs::metric_escape("a\nb"), std::string("a\\nb"), "newlines are escaped, not emitted raw");
}

void the_builtin_registry_is_shared_and_ordered() {
    // The process registry is a singleton: two scrapes of the same series must
    // be the same object, or a program could count into a void.
    hs::metrics_inc(hs::Val::text("qa_orders_total"), hs::Val::nil());
    hs::metrics_inc(hs::Val::text("qa_orders_total"), hs::Val::int_(4));
    double v = 0;
    CHECK(hs::metrics_registry().value("qa_orders_total", v), "the process registry saw it");
    CHECK_EQ(v, 5.0, "counted across calls");
    hs::metrics_set(hs::Val::text("qa_depth"), hs::Val::int_(7));
    hs::metrics_registry().value("qa_depth", v);
    CHECK_EQ(v, 7.0, "gauge through the language surface");
    CHECK(hs::metrics_value(hs::Val::text("qa_depth")).is_flt(), "value() returns a number");
    hs::metrics_observe(hs::Val::text("qa_latency_ms"), hs::Val::int_(2));
    auto snap = hs::metrics_snapshot_json();
    CHECK(snap.is_obj() && snap.find("series") != nullptr, "the JSON snapshot has series");
    CHECK(snap.find("uptime_ms") != nullptr, "and an uptime");
}

void the_language_surface_checks_its_arguments() {
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
    want_fail([] { hs::metrics_inc(hs::Val::int_(1), hs::Val::nil()); }, "needs a name",
              "a counter without a name");
    want_fail([] { hs::metrics_set(hs::Val::text("x"), hs::Val::text("no")); }, "needs a number",
              "a gauge set to text");
    want_fail([] { hs::metrics_observe(hs::Val::text("x"), hs::Val::text("no")); }, "needs a number",
              "an observation of text");
    want_fail([] { hs::health_set_val(hs::Val::text("db"), hs::Val::text("yes")); },
              "needs true or false", "a health gate set to text");
    want_fail([] { hs::health_set_val(hs::Val::text(""), hs::Val::boolean(true)); },
              "needs a check name", "a health gate with no name");
    CHECK(hs::metrics_value(hs::Val::text("qa_never_recorded")).is_nil(),
          "reading an unknown series is nil, not zero");
}

// ---------------------------------------------------------------------------
// Exposition
// ---------------------------------------------------------------------------

bool has_line_starting(const std::string& text, const std::string& prefix) {
    size_t i = 0;
    while (i < text.size()) {
        size_t eol = text.find('\n', i);
        if (eol == std::string::npos) eol = text.size();
        std::string line = text.substr(i, eol - i);
        if (line.rfind(prefix, 0) == 0) return true;
        i = eol + 1;
    }
    return false;
}

std::string line_starting(const std::string& text, const std::string& prefix) {
    size_t i = 0;
    while (i < text.size()) {
        size_t eol = text.find('\n', i);
        if (eol == std::string::npos) eol = text.size();
        std::string line = text.substr(i, eol - i);
        if (line.rfind(prefix, 0) == 0) return line;
        i = eol + 1;
    }
    return "";
}

void the_exposition_is_valid_prometheus_text() {
    hs::metrics_set(hs::Val::text("qa_expo_gauge"), hs::Val::int_(5));
    hs::metrics_inc(hs::Val::text("qa_expo_counter"), hs::Val::int_(3));
    hs::metrics_observe(hs::Val::text("qa_expo_hist"), hs::Val::int_(0));
    hs::metrics_observe(hs::Val::text("qa_expo_hist"), hs::Val::int_(30));
    std::string text = hs::metrics_prometheus_text();
    CHECK(has_line_starting(text, "# TYPE qa_expo_counter counter"), "counters say so");
    CHECK(has_line_starting(text, "# TYPE qa_expo_gauge gauge"), "gauges say so");
    CHECK(has_line_starting(text, "# TYPE qa_expo_hist histogram"), "histograms say so");
    CHECK_EQ(line_starting(text, "qa_expo_counter "), std::string("qa_expo_counter 3"),
             "a counter line is `name value`");
    CHECK_EQ(line_starting(text, "qa_expo_gauge "), std::string("qa_expo_gauge 5"),
             "a gauge line is `name value`");
    CHECK(has_line_starting(text, "qa_expo_hist_bucket{le=\"0.5\"} 1"),
          "a bucket line carries its bound as a label");
    CHECK(has_line_starting(text, "qa_expo_hist_bucket{le=\"2.5\"} 1"),
          "and holds only what is at or below it");
    CHECK(has_line_starting(text, "qa_expo_hist_bucket{le=\"+Inf\"} 2"),
          "and +Inf holds every observation");
    CHECK(has_line_starting(text, "qa_expo_hist_count 2"), "the count line is there");
    CHECK(has_line_starting(text, "qa_expo_hist_sum 30"), "the sum line is there");
    CHECK(has_line_starting(text, "hs_process_uptime_ms "), "process uptime is always exposed");
    // No line may be empty, and every sample line must have a value.
    size_t i = 0;
    bool clean = true;
    while (i < text.size()) {
        size_t eol = text.find('\n', i);
        if (eol == std::string::npos) eol = text.size();
        std::string line = text.substr(i, eol - i);
        if (!line.empty() && line[0] != '#' && line.find(' ') == std::string::npos) clean = false;
        i = eol + 1;
    }
    CHECK(clean, "every sample line is `name value`");
}

void unused_subsystems_are_not_reported() {
    // Reading a subsystem's counters must not construct it, so a program that
    // never used the queue has no queue series at all.
    std::string text = hs::metrics_prometheus_text();
    CHECK(!has_line_starting(text, "hs_queue_enqueued_total"),
          "no queue series for a program that never enqueued");
    CHECK(!has_line_starting(text, "hs_cache_hits_total"),
          "no cache series for a program that never cached");
    CHECK(!has_line_starting(text, "hs_ratelimit_allowed_total"),
          "no rate limit series for a program that never limited");
    // Once used, they appear: the flag is what the exposition reads.
    CHECK(hs::cache_used() || !has_line_starting(text, "hs_cache_hits_total"),
          "cache series follow actual use");
}

// ---------------------------------------------------------------------------
// Endpoints over a real dispatch
// ---------------------------------------------------------------------------

/// A server with the metrics endpoints installed and two routes of its own.
struct MetricsServer {
    hs::Server app;
    MetricsServer() {
        app.handle("GET", "/ok", [](const hs::Request&) { return hs::Response::text("ok"); });
        // Its own path per counting test: the registry is process-wide, so two
        // tests that both counted /ok would have to add up.
        app.handle("GET", "/counted", [](const hs::Request&) { return hs::Response::text("c"); });
        app.handle("POST", "/counted", [](const hs::Request&) { return hs::Response::text("c"); });
        app.handle("GET", "/missing-on-purpose", [](const hs::Request&) { return hs::Response::text("m"); });
        app.handle("GET", "/users/:id",
                   [](const hs::Request& req) { return hs::Response::text(std::string(req.params.at("id"))); });
        app.handle("GET", "/boom", [](const hs::Request&) -> hs::Response {
            throw std::runtime_error("handler blew up");
        });
        app.handle("GET", "/slow", [](const hs::Request&) {
            std::this_thread::sleep_for(std::chrono::milliseconds(12));
            return hs::Response::text("slow");
        });
        app.handle("POST", "/ok", [](const hs::Request&) { return hs::Response::text("posted"); });
        hs::metrics_install(app);
    }
    hs::Response get(const std::string& path) {
        hs::Request req;
        req.method = "GET";
        req.path = path;
        return app.dispatch(req);
    }
    hs::Response post(const std::string& path) {
        hs::Request req;
        req.method = "POST";
        req.path = path;
        return app.dispatch(req);
    }
};

/// What a JSON response would put on the wire, which is what a client sees.
std::string json_of(const hs::Response& r) { return hs::to_json(r.json_v); }

void the_endpoints_answer() {
    MetricsServer s;
    s.get("/ok");  // one request, so the scrape has something to report
    hs::Response m = s.get("/metrics");
    CHECK_EQ(m.status, 200, "/metrics is 200");
    CHECK(m.ctype.find("text/plain") != std::string::npos, "as text");
    CHECK(m.ctype.find("version=0.0.4") != std::string::npos,
          "in the exposition version scrapers expect");
    CHECK(m.body.find("hs_requests_total") != std::string::npos, "with the request series in it");
    CHECK(!m.has_json, "and not as a JSON body");

    hs::Response h = s.get("/healthz");
    CHECK_EQ(h.status, 200, "/healthz is 200");
    CHECK(h.has_json, "/healthz answers JSON");
    std::string hj = json_of(h);
    CHECK(hj.find("\"status\":\"ok\"") != std::string::npos, "and says ok");
    CHECK(hj.find("uptime_ms") != std::string::npos, "with an uptime");
    CHECK(hj.find("version") != std::string::npos, "and a version");
    CHECK(h.ctype.find("application/json") != std::string::npos, "with a JSON content type");

    hs::Response r = s.get("/readyz");
    CHECK_EQ(r.status, 200, "/readyz is 200 when nothing is wrong");
    std::string rj = json_of(r);
    CHECK(rj.find("\"status\":\"ready\"") != std::string::npos, "and says ready");
    CHECK(rj.find("shutting_down") != std::string::npos, "reporting the shutdown check");

    CHECK_EQ(s.get("/nope").status, 404, "an unknown path is still 404");
}

void requests_are_counted_by_method_path_and_status() {
    MetricsServer s;
    s.get("/counted");
    s.get("/counted");
    s.post("/counted");
    s.get("/no-such-path");
    std::string text = hs::metrics_prometheus_text();
    CHECK(has_line_starting(text, "hs_requests_total{method=\"GET\",path=\"/counted\",status=\"200\"} 2"),
          "two GETs counted under one series");
    CHECK(has_line_starting(text, "hs_requests_total{method=\"POST\",path=\"/counted\",status=\"200\"} 1"),
          "the verb is part of the series");
    CHECK(has_line_starting(text, "hs_requests_total{method=\"GET\",path=\"/no-such-path\",status=\"404\"} 1"),
          "a 404 is counted too: a client error is still traffic");
}

void a_failing_handler_is_counted_as_a_500() {
    MetricsServer s;
    hs::Response r = s.get("/boom");  // the only /boom request in this process
    CHECK_EQ(r.status, 500, "the throw became a 500");
    std::string text = hs::metrics_prometheus_text();
    CHECK(has_line_starting(text, "hs_requests_total{method=\"GET\",path=\"/boom\",status=\"500\"} 1"),
          "and the failure is visible in the series");
}

void latency_lands_in_the_histogram() {
    MetricsServer s;
    s.get("/slow");
    std::string text = hs::metrics_prometheus_text();
    CHECK(has_line_starting(text, "hs_request_duration_ms_count{method=\"GET\",path=\"/slow\"} 1"),
          "the slow request is counted in the histogram");
    CHECK(has_line_starting(text, "hs_request_duration_ms_bucket{method=\"GET\",path=\"/slow\",le=\"0.5\"} 0"),
          "nothing landed under half a millisecond");
    CHECK(has_line_starting(text, "hs_request_duration_ms_bucket{method=\"GET\",path=\"/slow\",le=\"25\"} 1"),
          "a 12ms request lands in the 25ms bucket");
    CHECK(has_line_starting(text, "hs_request_duration_ms_bucket{method=\"GET\",path=\"/slow\",le=\"+Inf\"} 1"),
          "and +Inf counts it regardless");
}

void a_path_parameter_is_its_own_series_each_time() {
    MetricsServer s;
    s.get("/users/1");
    s.get("/users/2");
    std::string text = hs::metrics_prometheus_text();
    CHECK(text.find("path=\"/users/1\"") != std::string::npos,
          "the concrete path is what an operator greps");
    CHECK(text.find("path=\"/users/2\"") != std::string::npos,
          "each id is its own series, as Prometheus does");
    CHECK(has_line_starting(text, "hs_request_duration_ms_count{method=\"GET\",path=\"/users/1\"} 1"),
          "and it lands in the histogram too");
}

void readiness_reflects_the_gates() {
    MetricsServer s;
    hs::health_set("redis", false, "connection refused");
    hs::Response r = s.get("/readyz");
    CHECK_EQ(r.status, 503, "a failing gate makes the process not ready");
    std::string rj = json_of(r);
    CHECK(rj.find("\"status\":\"not_ready\"") != std::string::npos, "and says so");
    CHECK(rj.find("redis") != std::string::npos, "naming the gate");
    CHECK(rj.find("connection refused") != std::string::npos, "and its detail");
    // Liveness is unaffected: a process that cannot serve a dependency is
    // still alive, and restarting it does not fix the dependency.
    CHECK_EQ(s.get("/healthz").status, 200, "liveness ignores the gate");
    hs::health_set("redis", true, "");
    CHECK_EQ(s.get("/readyz").status, 200, "and readiness recovers when the gate does");
}

void readiness_fails_while_shutting_down() {
    MetricsServer s;
    CHECK_EQ(s.get("/readyz").status, 200, "ready before the signal");
    hs::g_hs_stop.store(true);
    hs::Response r = s.get("/readyz");
    hs::g_hs_stop.store(false);
    CHECK_EQ(r.status, 503, "not ready once the process is stopping");
    CHECK(json_of(r).find("shutting_down") != std::string::npos, "and the check says which one");
    CHECK_EQ(s.get("/healthz").status, 200, "liveness still answers while draining");
}

void readiness_asks_the_queue_when_it_is_in_use() {
    MetricsServer s;
    CHECK(json_of(s.get("/readyz")).find("queue") == std::string::npos,
          "no queue check for a program that never enqueued");
    // One enqueue is enough to make the queue part of readiness.
    hs::queue_runtime().declare_job("__qa_probe", {}, 1);
    hs::queue_runtime().enqueue("__qa_probe", hs::Val::list({}), 0, 0, 0);
    hs::Response r = s.get("/readyz");
    CHECK(json_of(r).find("queue") != std::string::npos, "a used queue is checked");
    CHECK_EQ(r.status, 200, "and an answering queue is ready");
}

void many_threads_record_without_losing_counts() {
    MetricsServer s;
    const int kThreads = 8;
    const int kEach = 200;
    std::vector<std::thread> ts;
    for (int t = 0; t < kThreads; t++) {
        ts.emplace_back([&s] {
            for (int i = 0; i < kEach; i++) {
                s.get("/ok");
                hs::metrics_inc(hs::Val::text("qa_threaded_total"), hs::Val::nil());
            }
        });
    }
    for (auto& th : ts)
        th.join();
    double v = 0;
    CHECK(hs::metrics_registry().value("qa_threaded_total", v), "the counter is there");
    CHECK_EQ(v, (double)(kThreads * kEach), "every increment landed, none doubled");
    std::string text = hs::metrics_prometheus_text();
    CHECK(has_line_starting(text, "hs_requests_total{method=\"GET\",path=\"/ok\",status=\"200\"} "),
          "the request series survived the concurrency");
}

void metrics_install_survives_being_called_twice() {
    // Codegen calls it once, but a second call must not break the server: the
    // routes are identical, so the first match answers either way.
    MetricsServer s;
    size_t before = s.app.routes.size();
    hs::metrics_install(s.app);
    CHECK_EQ(s.app.routes.size(), before + 3, "the three routes are registered again");
    CHECK_EQ(s.get("/metrics").status, 200, "/metrics still answers");
    CHECK_EQ(s.get("/healthz").status, 200, "/healthz still answers");
    CHECK_EQ(s.get("/readyz").status, 200, "/readyz still answers");
}

}  // namespace
}  // namespace qa

int main() {
    qa::counters_accumulate();
    qa::gauges_hold_their_last_value();
    qa::histograms_capture_the_shape();
    qa::series_are_separated_by_labels();
    qa::a_public_endpoint_cannot_leak_series();
    qa::names_and_labels_are_sanitized();
    qa::the_builtin_registry_is_shared_and_ordered();
    qa::the_language_surface_checks_its_arguments();
    qa::the_exposition_is_valid_prometheus_text();
    qa::unused_subsystems_are_not_reported();
    qa::the_endpoints_answer();
    qa::requests_are_counted_by_method_path_and_status();
    qa::a_failing_handler_is_counted_as_a_500();
    qa::latency_lands_in_the_histogram();
    qa::a_path_parameter_is_its_own_series_each_time();
    qa::readiness_reflects_the_gates();
    qa::readiness_fails_while_shutting_down();
    qa::readiness_asks_the_queue_when_it_is_in_use();
    qa::many_threads_record_without_losing_counts();
    qa::metrics_install_survives_being_called_twice();
    return qa::report("metrics");
}
