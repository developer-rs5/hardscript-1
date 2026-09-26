#ifndef HS_RUNTIME_SQLITE_HPP
#define HS_RUNTIME_SQLITE_HPP
// SQLite (native, loaded at run time)
//
// The library is opened with `dlopen` rather than linked, and the handful of
// entry points used here are declared by hand. Two reasons, and the second is
// the one that matters:
//
//  1. A program that never opens a database does not need libsqlite3 at build
//     time or run time. A web service that only reads config, or a CI job that
//     only builds, works on a machine with no SQLite installed at all.
//
//  2. `<sqlite3.h>` is a versioned, occasionally breaking ABI, and a build
//     machine's header is not the library the program will load. Declaring the
//     six functions actually called pins the contract to what this file uses,
//     and `SQLITE_VERSION_NUMBER` is checked at run time so a mismatched
//     library is reported rather than crashing.
//
// Everything the ORM needs goes through prepared statements with bound
// parameters. There is no path here that builds SQL text from a value.
#include "hs_runtime_orm.hpp"
#include <dlfcn.h>

namespace hs {

// The subset of the C API this backend uses. The signatures are SQLite's, not
// HardScript's, so they read as the C header declares them.
struct SqliteApi {
    void* lib = nullptr;
    int (*open_v2)(const char* filename, void** db, int flags, const char* vfs) = nullptr;
    int (*close_v2)(void* db) = nullptr;
    int (*prepare_v2)(void* db, const char* sql, int nbyte, void** stmt, const char** tail) = nullptr;
    int (*step)(void* stmt) = nullptr;
    int (*finalize)(void* stmt) = nullptr;
    int (*reset)(void* stmt) = nullptr;
    const char* (*errmsg)(void* db) = nullptr;
    int (*errcode)(void* db) = nullptr;
    int (*extended_errcode)(void* db) = nullptr;
    int (*bind_null)(void* stmt, int i) = nullptr;
    int (*bind_int64)(void* stmt, int i, int64_t v) = nullptr;
    int (*bind_double)(void* stmt, int i, double v) = nullptr;
    int (*bind_text)(void* stmt, int i, const char* v, int n, void (*destroy)(void*)) = nullptr;
    int (*bind_parameter_count)(void* stmt) = nullptr;
    int (*column_count)(void* stmt) = nullptr;
    const char* (*column_name)(void* stmt, int i) = nullptr;
    int (*column_type)(void* stmt, int i) = nullptr;
    int64_t (*column_int64)(void* stmt, int i) = nullptr;
    double (*column_double)(void* stmt, int i) = nullptr;
    const unsigned char* (*column_text)(void* stmt, int i) = nullptr;
    int (*column_bytes)(void* stmt, int i) = nullptr;
    int64_t (*changes)(void* db) = nullptr;
    int64_t (*last_insert_rowid)(void* db) = nullptr;
    int (*busy_timeout)(void* db, int ms) = nullptr;
    int (*libversion_number)(void) = nullptr;
};

// Constants from <sqlite3.h>. Spelled here so the header is not needed.
static constexpr int SQLITE_OK = 0;
static constexpr int SQLITE_ROW = 100;
static constexpr int SQLITE_DONE = 101;
static constexpr int SQLITE_BUSY = 5;
static constexpr int SQLITE_LOCKED = 6;
static constexpr int SQLITE_OPEN_READWRITE = 0x00000002;
static constexpr int SQLITE_OPEN_CREATE = 0x00000004;
static constexpr int SQLITE_OPEN_FULLMUTEX = 0x00010000;
static constexpr int SQLITE_OPEN_READONLY = 0x00000001;
static constexpr int SQLITE_NULL = 5;
static constexpr int SQLITE_BLOB = 4;
static constexpr int SQLITE_FLOAT = 2;
static constexpr int SQLITE_INTEGER = 1;
static constexpr int SQLITE_TEXT = 3;
/// The oldest SQLite whose ABI this file was written against. 3.7.14 is
/// sqlite3_close_v2, which is the first version where closing a connection with
/// unfinalized statements is legal rather than undefined.
static constexpr int SQLITE_MIN_VERSION = 3007014;
/// SQLITE_TRANSIENT tells SQLite to copy the bytes, which is what makes binding
/// a `std::string`'s buffer safe.
///
/// It is the *sentinel* `(void*)-1`, not a destructor to call: SQLite compares
/// the pointer against it and copies when it matches. Passing a real function
/// instead makes SQLite hand the buffer to `free`, and a `std::string`'s
/// internals are not a heap block -- the row comes back as whatever the freed
/// bytes have since become.
#define HS_SQLITE_TRANSIENT (reinterpret_cast<void (*)(void*)>(-1))

/// The loaded library, opened once per process.
///
/// `HS_SQLITE_LIB` names a library to use instead of probing, so a program can
/// be pinned to one build and a test can prove the override works.
inline SqliteApi& sqlite_api() {
    static SqliteApi api = [] {
        SqliteApi a;
        std::vector<const char*> names;
        if (const char* override_name = getenv("HS_SQLITE_LIB")) names.push_back(override_name);
        names.push_back("libsqlite3.so.0");
        names.push_back("libsqlite3.so");
        names.push_back("libsqlite3.dylib");
        for (const char* n : names) {
            a.lib = dlopen(n, RTLD_LAZY | RTLD_LOCAL);
            if (a.lib) break;
        }
        if (!a.lib) {
            throw std::runtime_error(
                "sqlite: libsqlite3 not found. Install it, or set HS_SQLITE_LIB to the library to load.");
        }
#define HS_SQLITE_SYM(field, name)                                                    \
    a.field = reinterpret_cast<decltype(a.field)>(dlsym(a.lib, name));                \
    if (!a.field) {                                                                   \
        dlclose(a.lib);                                                               \
        throw std::runtime_error(std::string("sqlite: ") + name + " is missing from the loaded library."); \
    }
        HS_SQLITE_SYM(open_v2, "sqlite3_open_v2")
        HS_SQLITE_SYM(close_v2, "sqlite3_close_v2")
        HS_SQLITE_SYM(prepare_v2, "sqlite3_prepare_v2")
        HS_SQLITE_SYM(step, "sqlite3_step")
        HS_SQLITE_SYM(finalize, "sqlite3_finalize")
        HS_SQLITE_SYM(reset, "sqlite3_reset")
        HS_SQLITE_SYM(errmsg, "sqlite3_errmsg")
        HS_SQLITE_SYM(errcode, "sqlite3_errcode")
        HS_SQLITE_SYM(extended_errcode, "sqlite3_extended_errcode")
        HS_SQLITE_SYM(bind_null, "sqlite3_bind_null")
        HS_SQLITE_SYM(bind_int64, "sqlite3_bind_int64")
        HS_SQLITE_SYM(bind_double, "sqlite3_bind_double")
        HS_SQLITE_SYM(bind_text, "sqlite3_bind_text")
        HS_SQLITE_SYM(bind_parameter_count, "sqlite3_bind_parameter_count")
        HS_SQLITE_SYM(column_count, "sqlite3_column_count")
        HS_SQLITE_SYM(column_name, "sqlite3_column_name")
        HS_SQLITE_SYM(column_type, "sqlite3_column_type")
        HS_SQLITE_SYM(column_int64, "sqlite3_column_int64")
        HS_SQLITE_SYM(column_double, "sqlite3_column_double")
        HS_SQLITE_SYM(column_text, "sqlite3_column_text")
        HS_SQLITE_SYM(column_bytes, "sqlite3_column_bytes")
        HS_SQLITE_SYM(changes, "sqlite3_changes")
        HS_SQLITE_SYM(last_insert_rowid, "sqlite3_last_insert_rowid")
        HS_SQLITE_SYM(busy_timeout, "sqlite3_busy_timeout")
        HS_SQLITE_SYM(libversion_number, "sqlite3_libversion_number")
#undef HS_SQLITE_SYM
        if (a.libversion_number() < SQLITE_MIN_VERSION) {
            dlclose(a.lib);
            a.lib = nullptr;
            throw std::runtime_error("sqlite: the loaded library is older than this runtime supports.");
        }
        return a;
    }();
    return api;
}

/// Whether a usable SQLite is present, without throwing. A program can branch
/// on this instead of wrapping every open in a try.
inline bool sqlite_available() {
    try {
        sqlite_api();
        return true;
    } catch (...) {
        return false;
    }
}

struct SqliteDb : DbBackend {
    void* db = nullptr;
    int depth = 0;
    /// Whether the connection enforces `REFERENCES`. True unless the build says
    /// otherwise, and readable so a program can tell rather than assume.
    bool foreign_keys = true;
    std::string path;
    /// Set for `:memory:`, which is per-connection and so per-object.
    bool memory = false;
    /// Open savepoints, oldest first. A name may appear more than once: like
    /// the database itself, a second mark with the same name stacks on top of
    /// the first, and rolling back or releasing names the most recent one.
    std::vector<std::string> sps;
    int sp_next = 0;

    explicit SqliteDb(const std::string& filename, bool readonly = false) : path(filename) {
        SqliteApi& a = sqlite_api();
        memory = filename == ":memory:" || filename.rfind("file:", 0) == 0;
        int flags = readonly ? SQLITE_OPEN_READONLY
                             : (SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX);
        int rc = a.open_v2(filename.c_str(), &db, flags, nullptr);
        if (rc != SQLITE_OK) {
            // A failed open still hands back a handle, to read the reason from.
            std::string why = db ? a.errmsg(db) : "cannot open the database";
            if (db) a.close_v2(db);
            db = nullptr;
            throw std::runtime_error("sqlite: " + filename + ": " + why);
        }
        // A write that waits is better than a write that fails: a request that
        // collides with another waits for it instead of reporting an error the
        // programmer cannot act on.
        a.busy_timeout(db, 5000);
        // SQLite leaves foreign keys *off* for compatibility, and the pragma is
        // per-connection. Left off, every `REFERENCES` clause the schema
        // generates is a comment: an orphan row goes in and the constraint
        // that was written down never fires. A declared constraint that is
        // silently not enforced is worse than none, so it is on.
        // Set, then read back: a pragma that returned nothing says nothing, and
        // a build that ignored it should be visible rather than assumed.
        exec("PRAGMA foreign_keys = ON");
        foreign_keys = pragma("PRAGMA foreign_keys") == 1;
    }

    ~SqliteDb() override {
        if (!db) return;
        rollback_all();
        sqlite_api().close_v2(db);
    }

    SqliteDb(const SqliteDb&) = delete;
    SqliteDb& operator=(const SqliteDb&) = delete;

    const char* name() const override { return "sqlite"; }
    DbDialect dialect() const override { return DbDialect::Sqlite; }
    int tx_depth() const override { return depth; }

    /// Run a statement for its effect and ignore the result. A pragma that
    /// *sets* something returns no rows, which is why setting and reading are
    /// two calls.
    void exec(const std::string& sql) { run(sql, {}); }

    /// One pragma's value. SQLite has no API for pragmas, so this is a
    /// statement like any other; one that does not answer gives 0.
    int pragma(const std::string& sql) {
        try {
            DbResult r = run(sql, {});
            if (r.rows.empty() || r.rows[0].empty()) return 0;
            const Val& v = r.rows[0][0];
            if (v.is_int()) return (int)v.iv;
            if (v.is_str()) return atoi(v.sv.c_str());
            return 0;
        } catch (...) {
            return 0;
        }
    }

    int64_t last_id() { return sqlite_api().last_insert_rowid(db); }
    int64_t affected() { return sqlite_api().changes(db); }

    /// Run one statement, binding `params` in order. A statement that returns
    /// rows yields them; one that does not yields its effect counts.
    DbResult run(const std::string& sql, const std::vector<Val>& params) override {
        SqliteApi& a = sqlite_api();
        DbResult out;
        void* stmt = nullptr;
        const char* tail = nullptr;
        int rc = a.prepare_v2(db, sql.c_str(), (int)sql.size(), &stmt, &tail);
        if (rc != SQLITE_OK || !stmt) {
            out.last_id = 0;
            std::string why = a.errmsg(db);
            if (stmt) a.finalize(stmt);
            throw std::runtime_error("sqlite: " + why + " -- while preparing: " + sql);
        }
        int want = a.bind_parameter_count(stmt);
        if (want != (int)params.size()) {
            // Catching it here names the real problem: a statement with the
            // wrong number of placeholders is a compiler bug, and the database
            // would only say "index out of range".
            a.finalize(stmt);
            throw std::runtime_error("sqlite: statement wants " + std::to_string(want) + " values but " +
                                     std::to_string(params.size()) + " were given -- " + sql);
        }
        for (size_t i = 0; i < params.size(); i++) bind(a, stmt, (int)i + 1, params[i]);

        int cols = a.column_count(stmt);
        for (int i = 0; i < cols; i++) out.columns.push_back(a.column_name(stmt, i) ? a.column_name(stmt, i) : "");

        while (true) {
            rc = a.step(stmt);
            if (rc == SQLITE_ROW) {
                std::vector<Val> row;
                row.reserve((size_t)cols);
                for (int i = 0; i < cols; i++) row.push_back(read(a, stmt, i));
                out.rows.push_back(std::move(row));
                continue;
            }
            break;
        }
        // The reason is read before the statement is finalized: finalizing a
        // failed statement can reset the connection's error state.
        if (rc != SQLITE_DONE) {
            std::string why = a.errmsg(db);
            a.finalize(stmt);
            throw std::runtime_error("sqlite: " + why + " -- while running: " + sql);
        }
        a.finalize(stmt);
        // `changes` and `last_insert_rowid` are connection state that any
        // statement can change, so they are read after the last `step`.
        out.affected = a.changes(db);
        out.last_id = a.last_insert_rowid(db);
        return out;
    }

    void begin() override {
        if (depth == 0) run("BEGIN", {});
        // SQLite has no nested transactions, so an inner one is a marker. That
        // is the honest mapping: the outermost `begin` owns the transaction and
        // the outermost `commit` or `rollback` ends it.
        depth++;
    }

    void commit() override {
        if (depth == 0) return;
        depth--;
        if (depth == 0) {
            sps.clear();
            run("COMMIT", {});
        }
    }

    std::string savepoint(const std::string& name) override {
        // A savepoint outside a transaction starts one implicitly on SQLite,
        // which would commit-bookkeep nobody owns. The language requires the
        // block, and `db_savepoint` enforces it; reaching here with none open
        // means an unchecked pipeline, and refusing beats a silent implicit
        // transaction.
        if (depth == 0) throw std::runtime_error("sqlite: savepoint needs an open transaction");
        std::string sp = name;
        if (sp.empty()) sp = "hs_sp_" + std::to_string(++sp_next);
        if (!db_valid_savepoint_name(sp))
            throw std::runtime_error("sqlite: savepoint name must use letters, digits and underscores");
        run("SAVEPOINT \"" + sp + "\"", {});
        sps.push_back(sp);
        return sp;
    }

    void release_savepoint(const std::string& name) override {
        // Like the database, releasing a mark forgets everything from it
        // forward: a savepoint made after it cannot outlive it.
        bool found = false;
        while (!sps.empty()) {
            std::string top = sps.back();
            sps.pop_back();
            if (top == name) {
                found = true;
                break;
            }
        }
        if (!found) throw std::runtime_error("sqlite: no savepoint \"" + name + "\" is open");
        run("RELEASE \"" + name + "\"", {});
    }

    void rollback_to(const std::string& name) override {
        bool found = false;
        for (const auto& s : sps)
            if (s == name) {
                found = true;
                break;
            }
        if (!found) throw std::runtime_error("sqlite: no savepoint \"" + name + "\" is open");
        // The mark stays: rolling back to it does not forget it, so the same
        // stretch can be retried or released afterwards.
        run("ROLLBACK TO \"" + name + "\"", {});
    }

    void rollback() override {
        if (depth == 0) return;
        depth = 0;
        sps.clear();
        run("ROLLBACK", {});
    }

    /// Unwind anything still open, for a connection being closed with a
    /// transaction in flight: leaving one open would make SQLite roll the
    /// write back, and the error would land on the next statement instead of
    /// on the mistake.
    void rollback_all() {
        if (depth > 0) {
            depth = 0;
            sps.clear();
            run("ROLLBACK", {});
        }
    }

    static void bind(SqliteApi& a, void* stmt, int i, const Val& v) {
        int rc = SQLITE_OK;
        if (v.is_nil()) {
            rc = a.bind_null(stmt, i);
        } else if (v.is_int()) {
            rc = a.bind_int64(stmt, i, v.iv);
        } else if (v.is_flt()) {
            rc = a.bind_double(stmt, i, v.fv);
        } else if (v.is_bool()) {
            rc = a.bind_int64(stmt, i, v.bv ? 1 : 0);
        } else if (v.is_str()) {
            rc = a.bind_text(stmt, i, v.sv.c_str(), (int)v.sv.size(), HS_SQLITE_TRANSIENT);
        } else {
            // A container has no SQLite storage of its own, so it goes as the
            // JSON text it is; the column's kind says how to read it back.
            std::string j = v.to_json();
            rc = a.bind_text(stmt, i, j.c_str(), (int)j.size(), HS_SQLITE_TRANSIENT);
        }
        if (rc != SQLITE_OK) {
            throw std::runtime_error("sqlite: cannot bind value " + std::to_string(i) + ".");
        }
    }

    /// One column as a `Val`, by SQLite's own type rather than the declared
    /// one. The model's kind is applied later, in `db_decode`, so a column
    /// declared `Bool` comes back as `0`/`1` here and a boolean there.
    static Val read(SqliteApi& a, void* stmt, int i) {
        switch (a.column_type(stmt, i)) {
            case SQLITE_INTEGER: return Val::int_(a.column_int64(stmt, i));
            case SQLITE_FLOAT: return Val::flt(a.column_double(stmt, i));
            case SQLITE_NULL: return Val::nil();
            case SQLITE_TEXT:
            case SQLITE_BLOB:
            default: {
                const unsigned char* p = a.column_text(stmt, i);
                int n = a.column_bytes(stmt, i);
                return Val::text(p && n > 0 ? std::string((const char*)p, (size_t)n) : std::string());
            }
        }
    }
};

/// Open a database and make it the ambient one, which is what generated code
/// reads through `db_need()`.
inline SqliteDb* db_open_sqlite(const std::string& filename) {
    auto* d = new SqliteDb(filename);
    db_set(d);
    return d;
}

inline void db_close(DbBackend* b) {
    if (!b) return;
    if (b == db_get()) db_set(nullptr);
    delete b;
}

}  // namespace hs
#endif  // HS_RUNTIME_SQLITE_HPP
