// hs_runtime_schedule.hpp -- schedule engine (M6.3)
//
// `every 1h { cleanup() }` runs cleanup every hour; `every day at "03:00"`
// runs backup when the wall clock says so; `every monday at "09:00"` weekly;
// `every startup { ... }` once at startup. One scheduler thread owns all
// timers and runs handlers synchronously: a scheduled body must be fast, and
// slow work goes through the queue, which exists for exactly that.
//
// Time comes in two flavors. Intervals count seconds on the monotonic clock
// and never care what time it is. Cron forms (`day`, weekdays, `at`) read a
// wall clock in a zone: UTC by default, the system zone for `local` (DST via
// libc), or a fixed `+HH:MM` offset. The next-fire math is free functions, so
// tests drive it without threads or sleeping.
//
// State is last-run per schedule, in memory, or in SQLite when configured:
// a cron time that passed while nothing was running fires once on start
// (catch up, never backlog). Intervals never catch up; they measure from
// when the scheduler learns them.

#ifndef HS_RUNTIME_SCHEDULE_HPP
#define HS_RUNTIME_SCHEDULE_HPP

#include "hs_runtime_orm.hpp"

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <ctime>
#include <functional>
#include <map>
#include <mutex>
#include <queue>
#include <string>
#include <thread>
#include <vector>

namespace hs {

// Fake clock for tests, mirroring the cache and queue seams: while set, time
// stands still until the test moves it.
inline std::atomic<int64_t>& sched_test_clock() {
    static std::atomic<int64_t> override_ms{-1};
    return override_ms;
}

inline int64_t sched_now_ms() {
    int64_t fake = sched_test_clock().load(std::memory_order_relaxed);
    if (fake >= 0) return fake;
    return (int64_t)std::chrono::duration_cast<std::chrono::milliseconds>(
               std::chrono::system_clock::now().time_since_epoch())
        .count();
}

inline void sched_set_now_for_tests(int64_t ms) {
    sched_test_clock().store(ms, std::memory_order_relaxed);
}

inline void sched_clear_now_for_tests() {
    sched_test_clock().store(-1, std::memory_order_relaxed);
}

/// A timezone as minutes east of UTC, or LOCAL_ZONE for the system zone.
constexpr int SCHED_LOCAL_ZONE = 2147483647;

/// Parse `UTC`, `Z`, `local`, or `+HH:MM` / `-HHMM`. Anything else is a
/// usage error naming what was offered: inventing zones silently would fire
/// jobs at the wrong hour, which is worse than refusing.
inline int sched_parse_zone(const std::string& tz) {
    std::string t;
    for (char c : tz) t += (char)tolower((unsigned char)c);
    if (t == "utc" || t == "z") return 0;
    if (t == "local") return SCHED_LOCAL_ZONE;
    if (t.size() < 3 || (t[0] != '+' && t[0] != '-')) {
        throw std::runtime_error("schedule: unknown timezone `" + tz +
                                 "` (UTC, local, or +HH:MM)");
    }
    int sign = t[0] == '+' ? 1 : -1;
    std::string digits;
    for (size_t i = 1; i < t.size(); i++) {
        if (t[i] == ':') continue;
        if (!isdigit((unsigned char)t[i]))
            throw std::runtime_error("schedule: unknown timezone `" + tz +
                                     "` (UTC, local, or +HH:MM)");
        digits += t[i];
    }
    if (digits.size() != 2 && digits.size() != 4)
        throw std::runtime_error("schedule: unknown timezone `" + tz +
                                 "` (UTC, local, or +HH:MM)");
    int hh = std::stoi(digits.substr(0, 2));
    int mm = digits.size() == 4 ? std::stoi(digits.substr(2, 2)) : 0;
    if (hh > 14 || mm > 59)
        throw std::runtime_error("schedule: unknown timezone `" + tz +
                                 "` (UTC, local, or +HH:MM)");
    return sign * (hh * 60 + mm);
}

/// Weekday number for a name, Monday-first like cron: monday 0 .. sunday 6.
/// Full names and three-letter abbreviations, any case.
inline int sched_parse_weekday(const std::string& day) {
    std::string t;
    for (char c : day) t += (char)tolower((unsigned char)c);
    const char* names[] = {"monday", "tuesday", "wednesday", "thursday",
                           "friday", "saturday", "sunday"};
    const char* abbrev[] = {"mon", "tue", "wed", "thu", "fri", "sat", "sun"};
    for (int i = 0; i < 7; i++) {
        if (t == names[i] || t == abbrev[i]) return i;
    }
    if (t == "day") return -1;  // every day
    throw std::runtime_error("schedule: unknown day `" + day +
                             "` (a weekday, or `day`)");
}

/// Split "HH:MM" or "HH:MM:SS" into parts, range-checked. Seconds default to
/// zero; anything else-shaped is refused before it becomes a silent midnight.
inline void sched_parse_time(const std::string& at, int& h, int& m, int& s) {
    std::vector<std::string> parts;
    std::string cur;
    for (char c : at) {
        if (c == ':') {
            parts.push_back(cur);
            cur.clear();
        } else if (!isdigit((unsigned char)c)) {
            throw std::runtime_error("schedule: bad time `" + at + "` (HH:MM or HH:MM:SS)");
        } else {
            cur += c;
        }
    }
    parts.push_back(cur);
    if (parts.size() < 2 || parts.size() > 3)
        throw std::runtime_error("schedule: bad time `" + at + "` (HH:MM or HH:MM:SS)");
    h = std::stoi(parts[0]);
    m = std::stoi(parts[1]);
    s = parts.size() == 3 ? std::stoi(parts[2]) : 0;
    if (h > 23 || m > 59 || s > 59)
        throw std::runtime_error("schedule: bad time `" + at + "` (HH:MM or HH:MM:SS)");
}

/// Break epoch milliseconds into civil time in a fixed zone (minutes east of
/// UTC), or the system zone for SCHED_LOCAL_ZONE. Fixed zones are pure
/// arithmetic; the system zone goes through libc, which owns DST.
inline void sched_civil(int64_t epoch_ms, int zone_min, int& year, int& mon, int& mday, int& wday,
                        int& hour, int& min, int& sec) {
    if (zone_min == SCHED_LOCAL_ZONE) {
        std::time_t t = (std::time_t)(epoch_ms / 1000);
        std::tm out;
        localtime_r(&t, &out);
        year = out.tm_year + 1900;
        mon = out.tm_mon + 1;
        mday = out.tm_mday;
        wday = (out.tm_wday + 6) % 7;  // Monday-first
        hour = out.tm_hour;
        min = out.tm_min;
        sec = out.tm_sec;
        return;
    }
    int64_t shifted = epoch_ms / 1000 + (int64_t)zone_min * 60;
    int64_t days = shifted >= 0 ? shifted / 86400 : (shifted - 86399) / 86400;
    int64_t sod = shifted - days * 86400;
    // Howard Hinnant's civil_from_days, Monday-first weekday included.
    int64_t z = days + 719468;
    int64_t era = (z >= 0 ? z : z - 146096) / 146097;
    int64_t doe = z - era * 146097;
    int64_t yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    int64_t y = yoe + era * 400;
    int64_t doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    int64_t mp = (5 * doy + 2) / 153;
    int64_t d = doy - (153 * mp + 2) / 5 + 1;
    int64_t mo = mp + (mp < 10 ? 3 : -9);
    y += (mo <= 2);
    year = (int)y;
    mon = (int)mo;
    mday = (int)d;
    wday = (int)(((days % 7) + 7 + 3) % 7);  // 1970-01-01 was a Thursday (3)
    hour = (int)(sod / 3600);
    min = (int)((sod % 3600) / 60);
    sec = (int)(sod % 60);
}

/// Epoch milliseconds for civil time in a fixed zone, or -1 asking libc for
/// the system zone (mktime normalizes DST folds and gaps the way the zone
/// defines them, which no hand-rolled table should second-guess).
inline int64_t sched_epoch(int year, int mon, int mday, int hour, int min, int sec, int zone_min) {
    if (zone_min == SCHED_LOCAL_ZONE) {
        std::tm t{};
        t.tm_year = year - 1900;
        t.tm_mon = mon - 1;
        t.tm_mday = mday;
        t.tm_hour = hour;
        t.tm_min = min;
        t.tm_sec = sec;
        t.tm_isdst = -1;
        std::time_t out = mktime(&t);
        if (out == (std::time_t)-1) return -1;
        return (int64_t)out * 1000;
    }
    // days_from_civil, inverted above.
    int64_t y = year;
    int64_t m = mon;
    y -= m <= 2;
    int64_t era = (y >= 0 ? y : y - 399) / 400;
    int64_t yoe = y - era * 400;
    int64_t mp = (m + 9) % 12;
    int64_t doy = (153 * mp + 2) / 5 + mday - 1;
    int64_t doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    int64_t days = era * 146097 + doe - 719468;
    return (days * 86400 + hour * 3600 + min * 60 + sec) * 1000 - (int64_t)zone_min * 60 * 1000;
}

/// Next interval fire strictly after `after_ms`: intervals measure from when
/// the scheduler learns them, never catch up, and the first fire is one full
/// period out.
inline int64_t sched_next_interval_ms(int64_t after_ms, int64_t every_secs) {
    return after_ms + every_secs * 1000;
}

/// Next daily fire strictly after `after_ms` at h:m:s in `zone_min`.
inline int64_t sched_next_daily_ms(int64_t after_ms, int h, int m, int s, int zone_min) {
    int year, mon, mday, wday, hh, mm, ss;
    sched_civil(after_ms, zone_min, year, mon, mday, wday, hh, mm, ss);
    int64_t cand = sched_epoch(year, mon, mday, h, m, s, zone_min);
    // A system zone can refuse a wall time (spring-forward gap): mktime says
    // -1, and the honest answer is the next day, not a guess.
    if (cand < 0) cand = sched_epoch(year, mon, mday, h, m, s, 0);
    if (cand <= after_ms) {
        // Tomorrow: add a day in civil terms, because 23/25-hour DST days
        // are not 86400 seconds long.
        int64_t next_day = sched_epoch(year, mon, mday, 12, 0, 0, zone_min) + 86400 * 1000;
        int y2, mo2, d2, w2, h2, m2, s2;
        sched_civil(next_day, zone_min, y2, mo2, d2, w2, h2, m2, s2);
        cand = sched_epoch(y2, mo2, d2, h, m, s, zone_min);
        if (cand < 0) cand = sched_epoch(y2, mo2, d2, h, m, s, 0);
    }
    return cand;
}

/// Next weekly fire strictly after `after_ms` on `weekday` (Monday-first) at
/// h:m:s in `zone_min`.
inline int64_t sched_next_weekly_ms(int64_t after_ms, int weekday, int h, int m, int s, int zone_min) {
    int year, mon, mday, wday, hh, mm, ss;
    sched_civil(after_ms, zone_min, year, mon, mday, wday, hh, mm, ss);
    int delta = (weekday - wday + 7) % 7;
    int64_t noon = sched_epoch(year, mon, mday, 12, 0, 0, zone_min);
    int64_t target_noon = noon + (int64_t)delta * 86400 * 1000;
    int y2, mo2, d2, w2, h2, m2, s2;
    sched_civil(target_noon, zone_min, y2, mo2, d2, w2, h2, m2, s2);
    int64_t cand = sched_epoch(y2, mo2, d2, h, m, s, zone_min);
    if (cand < 0) cand = sched_epoch(y2, mo2, d2, h, m, s, 0);
    if (cand <= after_ms) {
        int64_t week_out = sched_epoch(y2, mo2, d2, 12, 0, 0, zone_min) + 7 * 86400 * 1000;
        int y3, mo3, d3, w3, h3, m3, s3;
        sched_civil(week_out, zone_min, y3, mo3, d3, w3, h3, m3, s3);
        cand = sched_epoch(y3, mo3, d3, h, m, s, zone_min);
        if (cand < 0) cand = sched_epoch(y3, mo3, d3, h, m, s, 0);
    }
    return cand;
}

/// Most recent daily fire at or before `now_ms`, or -1 when the schedule never
/// fired (a fresh install has nothing to catch up on). Computed in civil
/// terms so DST days do not shift it: today at h:m:s when that already
/// passed, else yesterday at h:m:s.
inline int64_t sched_prev_daily_ms(int64_t now_ms, int h, int m, int s, int zone_min) {
    int year, mon, mday, wday, hh, mm, ss;
    sched_civil(now_ms, zone_min, year, mon, mday, wday, hh, mm, ss);
    int64_t today = sched_epoch(year, mon, mday, h, m, s, zone_min);
    if (today >= 0 && today <= now_ms) return today;
    // Yesterday: step back a day from noon, because noon exists on every
    // civil day even across DST transitions and midnight sometimes does not.
    int64_t noon = sched_epoch(year, mon, mday, 12, 0, 0, zone_min);
    if (noon < 0) noon = now_ms - (now_ms % (86400 * 1000)) + 12 * 3600 * 1000;
    int y2, mo2, d2, w2, h2, m2, s2;
    sched_civil(noon - 86400 * 1000, zone_min, y2, mo2, d2, w2, h2, m2, s2);
    int64_t prev = sched_epoch(y2, mo2, d2, h, m, s, zone_min);
    if (prev < 0 || prev > now_ms) return -1;
    return prev;
}

/// Most recent weekly fire at or before `now_ms` on `weekday`, walking back
/// at most seven days. The first matching day whose fire already passed is
/// the answer; a week ago always qualifies, so there always is one.
inline int64_t sched_prev_weekly_ms(int64_t now_ms, int weekday, int h, int m, int s, int zone_min) {
    int year, mon, mday, wday, hh, mm, ss;
    sched_civil(now_ms, zone_min, year, mon, mday, wday, hh, mm, ss);
    int64_t noon = sched_epoch(year, mon, mday, 12, 0, 0, zone_min);
    if (noon < 0) noon = now_ms - (now_ms % (86400 * 1000)) + 12 * 3600 * 1000;
    for (int d = 0; d < 8; d++) {
        int y2, mo2, d2, w2, h2, m2, s2;
        sched_civil(noon - (int64_t)d * 86400 * 1000, zone_min, y2, mo2, d2, w2, h2, m2, s2);
        if (w2 != weekday) continue;
        int64_t cand = sched_epoch(y2, mo2, d2, h, m, s, zone_min);
        if (cand >= 0 && cand <= now_ms) return cand;
    }
    return -1;
}

// ---------------------------------------------------------------------------
// Schedules, state, threads
// ---------------------------------------------------------------------------

enum class SchedKind : uint8_t { Interval, Daily, Weekly, Startup };

struct Schedule {
    std::string name;
    SchedKind kind = SchedKind::Interval;
    int64_t every_secs = 0;  // Interval
    int weekday = -1;        // Weekly, Monday-first; -1 with Daily means day
    int hour = 0, min = 0, sec = 0;
    int zone_min = 0;

    bool same_spec(const Schedule& o) const {
        if (kind != o.kind || every_secs != o.every_secs || weekday != o.weekday || hour != o.hour ||
            min != o.min || sec != o.sec || zone_min != o.zone_min)
            return false;
        return true;
    }
};

using SchedHandler = std::function<Val()>;

struct SchedTimer {
    int64_t next_ms = 0;
    uint64_t seq = 0;
    std::string name;
};

struct SchedTimerOrder {
    bool operator()(const SchedTimer& a, const SchedTimer& b) const {
        if (a.next_ms != b.next_ms) return a.next_ms > b.next_ms;
        return a.seq > b.seq;
    }
};

/// Last-run persistence. Memory forgets restarts; SQLite remembers them,
/// which is what makes missed-job recovery real across them.
struct SchedState {
    virtual ~SchedState() = default;
    virtual int64_t last_run(const std::string& name) = 0;
    virtual void mark_run(const std::string& name, int64_t at_ms) = 0;
};

class SchedMemory : public SchedState {
  public:
    int64_t last_run(const std::string& name) override {
        std::lock_guard<std::mutex> lock(mu_);
        auto it = runs_.find(name);
        return it == runs_.end() ? -1 : it->second;
    }
    void mark_run(const std::string& name, int64_t at_ms) override {
        std::lock_guard<std::mutex> lock(mu_);
        runs_[name] = at_ms;
    }

  private:
    std::mutex mu_;
    std::map<std::string, int64_t> runs_;
};

/// Persistent last-run state in an `hs_sched_state` table, through the ORM's
/// backend seam: SQLite directly, PostgreSQL by passing a `PgDb`. The
/// connection is dedicated to the scheduler.
class SchedSqlState : public SchedState {
  public:
    explicit SchedSqlState(DbBackend* db) : db_(db) {
        std::lock_guard<std::mutex> lock(mu_);
        db_->run("CREATE TABLE IF NOT EXISTS \"hs_sched_state\" (\"name\" TEXT PRIMARY KEY,"
                 "\"last_run\" INTEGER NOT NULL)",
                 {});
    }

    int64_t last_run(const std::string& name) override {
        std::lock_guard<std::mutex> lock(mu_);
        auto it = runs_.find(name);
        if (it != runs_.end()) return it->second;
        std::string sql = "SELECT \"last_run\" FROM \"hs_sched_state\" WHERE \"name\" = " +
                          db_placeholder(db_->dialect(), 1);
        DbResult r = db_->run(sql, {Val::text(name)});
        int64_t v = (!r.rows.empty() && !r.rows[0].empty() && r.rows[0][0].is_int()) ? r.rows[0][0].iv : -1;
        runs_[name] = v;
        return v;
    }

    void mark_run(const std::string& name, int64_t at_ms) override {
        std::lock_guard<std::mutex> lock(mu_);
        runs_[name] = at_ms;
        if (db_->dialect() == DbDialect::Postgres) {
            std::string sql = "INSERT INTO \"hs_sched_state\" (\"name\", \"last_run\") VALUES (" +
                              db_placeholder(db_->dialect(), 1) + ", " +
                              db_placeholder(db_->dialect(), 2) +
                              ") ON CONFLICT (\"name\") DO UPDATE SET \"last_run\" = " +
                              db_placeholder(db_->dialect(), 3);
            db_->run(sql, {Val::text(name), Val::int_(at_ms), Val::int_(at_ms)});
            return;
        }
        std::string sql = "INSERT OR REPLACE INTO \"hs_sched_state\" (\"name\", \"last_run\") VALUES (" +
                          db_placeholder(db_->dialect(), 1) + ", " + db_placeholder(db_->dialect(), 2) + ")";
        db_->run(sql, {Val::text(name), Val::int_(at_ms)});
    }

  private:
    std::mutex mu_;
    std::map<std::string, int64_t> runs_;
    DbBackend* db_;
};

/// The process-wide scheduler. One thread owns every timer and runs every
/// handler synchronously: a scheduled body must be fast, and slow work goes
/// through the queue. Stops and joins before it destroys, like every other
/// background thread in this runtime.
class SchedRuntime {
  public:
    SchedRuntime() = default;
    ~SchedRuntime() {
        stop.store(true, std::memory_order_relaxed);
        wake.notify_all();
        if (worker.joinable()) worker.join();
    }
    SchedRuntime(const SchedRuntime&) = delete;
    SchedRuntime& operator=(const SchedRuntime&) = delete;

    void register_schedule(Schedule s, SchedHandler handler) {
        if (s.name.empty()) throw std::runtime_error("schedule: a schedule needs a name");
        if (!handler) throw std::runtime_error("schedule: `" + s.name + "` needs a handler");
        std::lock_guard<std::mutex> lock(mu_);
        auto dup = schedules_.find(s.name);
        if (dup != schedules_.end()) {
            // Same spec twice is startup code running twice: keep the first,
            // which is already ticking. A different spec under one name is a
            // conflict worth failing loudly.
            if (dup->second.spec.same_spec(s)) return;
            throw std::runtime_error("schedule: `" + s.name + "` is already registered");
        }
        int64_t now = sched_now_ms();
        SchedEntry e;
        e.spec = std::move(s);
        e.handler = std::move(handler);
        int64_t next_ms;
        if (e.spec.kind == SchedKind::Startup) {
            next_ms = now;
        } else if (e.spec.kind == SchedKind::Interval) {
            if (e.spec.every_secs <= 0)
                throw std::runtime_error("schedule: `" + e.spec.name + "` interval must be positive");
            next_ms = sched_next_interval_ms(now, e.spec.every_secs);
        } else {
            next_ms = next_cron_ms(e.spec, now);
            // Missed-job recovery: the persisted last run predates the most
            // recent scheduled time, so the world already missed one. Fire
            // immediately, once; the timer below resumes the rhythm. A fresh
            // install (no last run) is not a miss.
            int64_t last = state_locked()->last_run(e.spec.name);
            int64_t prev = prev_cron_ms(e.spec, now);
            if (last >= 0 && prev >= 0 && last < prev) {
                next_ms = now;
                e.catch_up = true;
            }
        }
        timers_.push(SchedTimer{next_ms, seq_++, e.spec.name});
        schedules_[e.spec.name] = std::move(e);
        ensure_worker_locked();
        wake.notify_all();
    }

    void use_state(SchedState* state) {
        if (!state) throw std::runtime_error("schedule: state must not be null");
        std::lock_guard<std::mutex> lock(mu_);
        state_.reset(state);
    }

    int64_t last_run(const std::string& name) {
        std::lock_guard<std::mutex> lock(mu_);
        return state_locked()->last_run(name);
    }

    size_t schedule_count() {
        std::lock_guard<std::mutex> lock(mu_);
        return schedules_.size();
    }

    bool has_schedule(const std::string& name) {
        std::lock_guard<std::mutex> lock(mu_);
        return schedules_.count(name) > 0;
    }

    uint64_t fires() const { return fires_.load(std::memory_order_relaxed); }
    uint64_t missed() const { return missed_.load(std::memory_order_relaxed); }

  private:
    struct SchedEntry {
        Schedule spec;
        SchedHandler handler;
        bool catch_up = false;
    };

    static int64_t next_cron_ms(const Schedule& s, int64_t now) {
        if (s.kind == SchedKind::Daily) return sched_next_daily_ms(now, s.hour, s.min, s.sec, s.zone_min);
        return sched_next_weekly_ms(now, s.weekday, s.hour, s.min, s.sec, s.zone_min);
    }

    static int64_t prev_cron_ms(const Schedule& s, int64_t now) {
        if (s.kind == SchedKind::Daily) return sched_prev_daily_ms(now, s.hour, s.min, s.sec, s.zone_min);
        if (s.kind == SchedKind::Weekly)
            return sched_prev_weekly_ms(now, s.weekday, s.hour, s.min, s.sec, s.zone_min);
        return -1;
    }

    SchedState* state_locked() {
        if (!state_) state_.reset(new SchedMemory());
        return state_.get();
    }

    void ensure_worker_locked() {
        if (worker.joinable()) return;
        worker = std::thread([this] { loop(); });
    }

    void loop() {
        for (;;) {
            SchedTimer due;
            SchedHandler handler;
            bool catch_up = false;
            {
                std::unique_lock<std::mutex> lock(mu_);
                for (;;) {
                    if (stop.load(std::memory_order_relaxed)) return;
                    if (timers_.empty()) {
                        wake.wait(lock, [this] { return stop.load(std::memory_order_relaxed); });
                        if (stop.load(std::memory_order_relaxed)) return;
                        continue;
                    }
                    int64_t wait = timers_.top().next_ms - sched_now_ms();
                    if (wait <= 0) break;
                    // Short slices, not one sleep till the deadline: clocks
                    // jump (tests move fake time; NTP steps real time), and a
                    // thread asleep till Tuesday never notices Monday changed.
                    // Recomputing every 100ms costs nothing measurable.
                    int64_t slice = wait > 100 ? 100 : wait;
                    wake.wait_for(lock, std::chrono::milliseconds(slice), [this] {
                        return stop.load(std::memory_order_relaxed);
                    });
                }
                due = timers_.top();
                timers_.pop();
                auto it = schedules_.find(due.name);
                if (it == schedules_.end()) continue;  // unregistered mid-flight; drop it
                handler = it->second.handler;
                catch_up = it->second.catch_up;
                it->second.catch_up = false;
            }
            // Handlers run unlocked: this is the only thread, and a slow body
            // delays every schedule, which is why bodies must be fast.
            int64_t fired_at = sched_now_ms();
            try {
                handler();
            } catch (...) {
                // A throwing schedule must not kill time itself: count it and
                // keep the rhythm. The error is swallowed the way a log line
                // swallows it, because there is nowhere better to put it.
            }
            {
                std::lock_guard<std::mutex> lock(mu_);
                fires_.fetch_add(1, std::memory_order_relaxed);
                if (catch_up) missed_.fetch_add(1, std::memory_order_relaxed);
                auto it = schedules_.find(due.name);
                if (it == schedules_.end()) continue;
                state_locked()->mark_run(due.name, fired_at);
                if (it->second.spec.kind == SchedKind::Startup) {
                    schedules_.erase(it);
                    continue;
                }
                int64_t next;
                if (it->second.spec.kind == SchedKind::Interval) {
                    // From now, not from the deadline: a slow fire does not
                    // debt the next one, and intervals never backlog.
                    next = sched_next_interval_ms(sched_now_ms(), it->second.spec.every_secs);
                } else {
                    next = next_cron_ms(it->second.spec, sched_now_ms());
                }
                timers_.push(SchedTimer{next, seq_++, due.name});
            }
        }
    }

    mutable std::mutex mu_;
    std::condition_variable wake;
    std::atomic<bool> stop{false};
    std::thread worker;
    std::map<std::string, SchedEntry> schedules_;
    std::priority_queue<SchedTimer, std::vector<SchedTimer>, SchedTimerOrder> timers_;
    uint64_t seq_ = 0;
    std::unique_ptr<SchedState> state_;
    std::atomic<uint64_t> fires_{0};
    std::atomic<uint64_t> missed_{0};
};

inline SchedRuntime& sched_runtime() {
    static SchedRuntime r;
    return r;
}

// ---------------------------------------------------------------------------
// Language surface
// ---------------------------------------------------------------------------

inline Val schedule_register_interval(const Val& name, const Val& secs, SchedHandler handler) {
    if (!name.is_str() || name.sv.empty())
        throw std::runtime_error("schedule: register needs a name as text");
    if (!secs.is_int() || secs.iv <= 0)
        throw std::runtime_error("schedule: interval seconds must be positive");
    Schedule s;
    s.name = name.sv;
    s.kind = SchedKind::Interval;
    s.every_secs = secs.iv;
    sched_runtime().register_schedule(std::move(s), std::move(handler));
    return Val::boolean(true);
}

inline Val schedule_register_daily(const Val& name, const Val& h, const Val& m, const Val& s,
                                   const Val& tz, SchedHandler handler) {
    if (!name.is_str() || name.sv.empty())
        throw std::runtime_error("schedule: register needs a name as text");
    for (const Val* v : {&h, &m, &s}) {
        if (!v->is_int()) throw std::runtime_error("schedule: time parts must be integers");
    }
    if (h.iv < 0 || h.iv > 23 || m.iv < 0 || m.iv > 59 || s.iv < 0 || s.iv > 59)
        throw std::runtime_error("schedule: bad time (HH:MM or HH:MM:SS)");
    int zone_min = 0;  // default zone: UTC
    if (!tz.is_nil()) {
        if (!tz.is_str()) throw std::runtime_error("schedule: timezone must be text");
        zone_min = sched_parse_zone(tz.sv);
    }
    Schedule spec;
    spec.name = name.sv;
    spec.kind = SchedKind::Daily;
    spec.hour = (int)h.iv;
    spec.min = (int)m.iv;
    spec.sec = (int)s.iv;
    spec.zone_min = zone_min;
    sched_runtime().register_schedule(std::move(spec), std::move(handler));
    return Val::boolean(true);
}

inline Val schedule_register_weekly(const Val& name, const Val& weekday, const Val& h, const Val& m,
                                    const Val& s, const Val& tz, SchedHandler handler) {
    if (!name.is_str() || name.sv.empty())
        throw std::runtime_error("schedule: register needs a name as text");
    if (!weekday.is_int() || weekday.iv < 0 || weekday.iv > 6)
        throw std::runtime_error("schedule: weekday must be 0 (Monday) to 6 (Sunday)");
    for (const Val* v : {&h, &m, &s}) {
        if (!v->is_int()) throw std::runtime_error("schedule: time parts must be integers");
    }
    if (h.iv < 0 || h.iv > 23 || m.iv < 0 || m.iv > 59 || s.iv < 0 || s.iv > 59)
        throw std::runtime_error("schedule: bad time (HH:MM or HH:MM:SS)");
    int zone_min = 0;  // default zone: UTC
    if (!tz.is_nil()) {
        if (!tz.is_str()) throw std::runtime_error("schedule: timezone must be text");
        zone_min = sched_parse_zone(tz.sv);
    }
    Schedule spec;
    spec.name = name.sv;
    spec.kind = SchedKind::Weekly;
    spec.weekday = (int)weekday.iv;
    spec.hour = (int)h.iv;
    spec.min = (int)m.iv;
    spec.sec = (int)s.iv;
    spec.zone_min = zone_min;
    sched_runtime().register_schedule(std::move(spec), std::move(handler));
    return Val::boolean(true);
}

inline Val schedule_register_startup(const Val& name, SchedHandler handler) {
    if (!name.is_str() || name.sv.empty())
        throw std::runtime_error("schedule: register needs a name as text");
    Schedule spec;
    spec.name = name.sv;
    spec.kind = SchedKind::Startup;
    sched_runtime().register_schedule(std::move(spec), std::move(handler));
    return Val::boolean(true);
}

/// Persist schedule state through a state store (memory by default). Takes
/// ownership, like the queue backend setter.
inline Val schedule_use_state(SchedState* state) {
    sched_runtime().use_state(state);
    return Val::boolean(true);
}

}  // namespace hs

#endif  // HS_RUNTIME_SCHEDULE_HPP
