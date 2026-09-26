#ifndef HS_QA_ORM_SUPPORT_HPP
#define HS_QA_ORM_SUPPORT_HPP
// Shared scaffolding for the ORM fixtures in this directory.
//
// The fixtures are self-asserting: each check either passes silently or the
// program exits non-zero with the file, line, and both texts. The runner only
// has to look at the exit status.
#include <cstdio>
#include <string>
#include <vector>

#include "hs_runtime_orm.hpp"

namespace qa {

inline int g_checks = 0;
inline int g_failed = 0;

inline void fail(const char* file, int line, const std::string& what, const std::string& got,
                 const std::string& want) {
    g_failed++;
    std::fprintf(stderr, "%s:%d: %s\n  got:  %s\n  want: %s\n", file, line, what.c_str(),
                 got.c_str(), want.c_str());
}

/// A check that reports and keeps going, so one run shows every breakage.
#define CHECK(cond, what)                                                 \
    do {                                                                  \
        qa::g_checks++;                                                   \
        if (!(cond)) qa::fail(__FILE__, __LINE__, (what), #cond, "true"); \
    } while (0)

#define CHECK_EQ(got, want, what)                                                     \
    do {                                                                             \
        qa::g_checks++;                                                               \
        auto g_ = (got);                                                              \
        auto w_ = (want);                                                             \
        if (!(g_ == w_)) qa::fail(__FILE__, __LINE__, (what), qa::show(g_), qa::show(w_)); \
    } while (0)

inline std::string show(const std::string& s) { return "\"" + s + "\""; }
inline std::string show(const char* s) { return std::string("\"") + s + "\""; }
inline std::string show(bool b) { return b ? "true" : "false"; }
inline std::string show(int64_t v) { return std::to_string(v); }
inline std::string show(size_t v) { return std::to_string(v); }
inline std::string show(int v) { return std::to_string(v); }
inline std::string show(const char* const&) = delete;

/// A backend that records what it was asked to do and returns canned rows.
///
/// The point of a fake here is that the query builder can be asserted on
/// exactly: what SQL it produced, in what order, with which bound values, and
/// with which placeholders.
class FakeBackend : public hs::DbBackend {
  public:
    hs::DbDialect d = hs::DbDialect::Sqlite;
    std::vector<std::string> sqls;
    std::vector<std::vector<hs::Val>> params;
    hs::DbResult canned;
    int begins = 0, commits = 0, rollbacks = 0;
    int depth = 0;
    bool throw_on_run = false;

    const char* name() const override { return "fake"; }
    hs::DbDialect dialect() const override { return d; }

    hs::DbResult run(const std::string& sql, const std::vector<hs::Val>& p) override {
        sqls.push_back(sql);
        params.push_back(p);
        if (throw_on_run) throw std::runtime_error("fake: run failed");
        return canned;
    }
    void begin() override {
        begins++;
        depth++;
    }
    void commit() override {
        commits++;
        if (depth > 0) depth--;
    }
    void rollback() override {
        rollbacks++;
        if (depth > 0) depth--;
    }
    int tx_depth() const override { return depth; }

    const std::string& last_sql() const {
        static const std::string none;
        return sqls.empty() ? none : sqls.back();
    }
    size_t calls() const { return sqls.size(); }
};

/// A backend that only reports which dialect it speaks, for pure-assembly
/// checks that must not need a connection.
class DialectOnly : public hs::DbBackend {
  public:
    hs::DbDialect d;
    explicit DialectOnly(hs::DbDialect dd) : d(dd) {}
    const char* name() const override { return "dialect-only"; }
    hs::DbDialect dialect() const override { return d; }
    hs::DbResult run(const std::string&, const std::vector<hs::Val>&) override { return {}; }
    void begin() override {}
    void commit() override {}
    void rollback() override {}
    int tx_depth() const override { return 0; }
};

/// The `User` table the fixtures query, matching the schema in the compiler
/// tests so a column name is spelled the same way on both sides.
inline const hs::OrmModel& user_model() {
    static hs::OrmModel m("User", "user", "id", {"id", "email", "name", "age"},
                           {hs::OrmKind::Int, hs::OrmKind::Text, hs::OrmKind::Text,
                            hs::OrmKind::Int});
    return m;
}

inline int report(const char* name) {
    if (g_failed) {
        std::fprintf(stderr, "%s: %d of %d checks failed\n", name, g_failed, g_checks);
        return 1;
    }
    std::printf("%s: %d checks passed\n", name, g_checks);
    return 0;
}

}  // namespace qa

#endif  // HS_QA_ORM_SUPPORT_HPP
