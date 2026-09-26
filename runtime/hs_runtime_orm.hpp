#ifndef HS_RUNTIME_ORM_HPP
#define HS_RUNTIME_ORM_HPP
// The ORM runtime: model metadata, the query builder, and the backend seam the
// database adapters plug into (M5.3).
//
// The compiler does not generate SQL text. It generates *validated steps* —
// column names it has already checked against the schema, and values it emits
// as `hs::Val` expressions — and this layer is the one place that assembles
// them into a statement. One assembler means one set of rules about
// placeholders, ordering and identifier quoting, and it means a query plan can
// be inspected (`sql()`) and asserted on without a database.
//
// A value never becomes SQL text. Conditions keep their value in a `Val` and
// emit a placeholder; only column and table identifiers reach the statement,
// and those come from the schema, never from a request.
#include "hs_runtime_value.hpp"

#include <cctype>
#include <exception>
#include <map>

namespace hs {

// How a column's text is bound and read back. The compiler picks this from the
// declared type, so a `Bool` column comes back as a boolean rather than the
// 0/1 an SQLite cursor hands back.
enum class OrmKind : uint8_t { Int, Float, Bool, Text, Uuid, Time, Json };

// One model's table, as registered by generated code.
struct OrmModel {
    std::string name;
    std::string table;
    std::string pk;
    // Canonical column order. Positional decoding depends on this order
    // matching the order the SELECT lists, so it is never sorted per query.
    std::vector<std::string> columns;
    std::vector<OrmKind> kinds;
    // Columns the database fills in: an auto-increment key, a `@default`, a
    // timestamp. An insert may leave these out, and the one for `@updated_at`
    // is what `touch` writes.
    std::vector<std::string> generated;
    std::string updated_col;

    OrmModel(std::string n, std::string t, std::string p, std::vector<std::string> cols,
             std::vector<OrmKind> ks, std::vector<std::string> gen = {},
             std::string upd = "")
        : name(std::move(n)), table(std::move(t)), pk(std::move(p)),
          columns(std::move(cols)), kinds(std::move(ks)), generated(std::move(gen)),
          updated_col(std::move(upd)) {}

    int idx(const std::string& c) const {
        for (size_t i = 0; i < columns.size(); i++)
            if (columns[i] == c) return (int)i;
        return -1;
    }
    bool has(const std::string& c) const { return idx(c) >= 0; }
    OrmKind kind_of(const std::string& c) const {
        int i = idx(c);
        return i >= 0 && (size_t)i < kinds.size() ? kinds[i] : OrmKind::Text;
    }
    bool is_generated(const std::string& c) const {
        for (const auto& g : generated)
            if (g == c) return true;
        return false;
    }
    size_t size() const { return columns.size(); }
};

// Which backend a statement is built for. Only the placeholder spelling and
// identifier quoting differ; everything else is shared.
enum class DbDialect : uint8_t { Sqlite, Postgres };

// A result set, or the effect counts of a statement that returns no rows.
struct DbResult {
    std::vector<std::string> columns;
    std::vector<std::vector<Val>> rows;
    int64_t affected = 0;
    int64_t last_id = 0;
};

// The seam a database adapter implements. The ORM talks only to this, which is
// why the query builder is testable without a database and why adding SQLite
// or PostgreSQL touches no query code.
struct DbBackend {
    virtual ~DbBackend() = default;
    virtual const char* name() const = 0;
    virtual DbDialect dialect() const = 0;
    // Run one prepared statement. `params` are bound, never interpolated.
    virtual DbResult run(const std::string& sql, const std::vector<Val>& params) = 0;
    // Run one statement many times, once per parameter set, preparing (or
    // parsing) only once. Rows concatenate in order and effects add up. The
    // default loops `run`; adapters that can reuse the preparation override
    // it. Either way the caller owns atomicity: wrap the call in a
    // transaction when the sets belong together.
    virtual DbResult run_many(const std::string& sql, const std::vector<std::vector<Val>>& sets) {
        DbResult out;
        for (const auto& params : sets) {
            DbResult r = run(sql, params);
            if (out.columns.empty()) out.columns = r.columns;
            out.rows.insert(out.rows.end(), r.rows.begin(), r.rows.end());
            out.affected += r.affected;
            if (out.last_id == 0) out.last_id = r.last_id;
        }
        return out;
    }
    virtual void begin() = 0;
    virtual void commit() = 0;
    virtual void rollback() = 0;
    // Nesting depth of open transactions on this connection.
    virtual int tx_depth() const = 0;
    // Mark a point inside the open transaction, returning the name it is
    // known by. An empty name mints one (`hs_sp_1`, ...); a non-empty name is
    // validated first, because a savepoint name travels into SQL unquoted.
    virtual std::string savepoint(const std::string& name) = 0;
    // Forget a savepoint after a successful stretch: everything since it
    // stays, the mark goes.
    virtual void release_savepoint(const std::string& name) = 0;
    // Undo everything since the named savepoint. The transaction — and the
    // savepoint itself — stay open.
    virtual void rollback_to(const std::string& name) = 0;
};

/// A savepoint name is safe to interpolate only when it is already an
/// identifier. The compiler checks literal names; this is the backstop for
/// names built at runtime.
inline bool db_valid_savepoint_name(const std::string& name) {
    if (name.empty()) return false;
    if (!(isalpha((unsigned char)name[0]) || name[0] == '_')) return false;
    for (size_t i = 1; i < name.size(); i++) {
        if (!(isalnum((unsigned char)name[i]) || name[i] == '_')) return false;
    }
    return true;
}

/// Mark a point in the open transaction. Returns nil: a savepoint is an
/// effect, and handing back a value would invite assigning it.
inline Val db_savepoint(DbBackend* b, const Val& name) {
    if (!name.is_str() || !db_valid_savepoint_name(name.sv))
        throw std::runtime_error("db: savepoint name must use letters, digits and underscores");
    if (b->tx_depth() == 0)
        throw std::runtime_error("db: savepoint \"" + name.sv + "\" needs an open transaction");
    b->savepoint(name.sv);
    return Val::nil();
}

/// Undo everything since the named savepoint, keeping the transaction open.
inline Val db_rollback_to(DbBackend* b, const Val& name) {
    if (!name.is_str() || !db_valid_savepoint_name(name.sv))
        throw std::runtime_error("db: savepoint name must use letters, digits and underscores");
    if (b->tx_depth() == 0)
        throw std::runtime_error("db: rollback_to \"" + name.sv + "\" needs an open transaction");
    b->rollback_to(name.sv);
    return Val::nil();
}

/// One `db.transaction { ... }` block. The outermost guard opens the
/// transaction; a nested one marks a savepoint instead, so an inner failure
/// rolls back the inner block without killing the outer one. Destruction
/// commits a clean exit and rolls back an exceptional one, which is what
/// makes a runtime error inside the block unwind the block's writes.
struct TxGuard {
    DbBackend* b = nullptr;
    bool top = false;
    std::string sp;
    int exc = 0;

    explicit TxGuard(DbBackend* bb)
        : b(bb), top(bb->tx_depth() == 0), exc(std::uncaught_exceptions()) {
        if (top) {
            b->begin();
        } else {
            sp = b->savepoint("");
        }
    }
    TxGuard(const TxGuard&) = delete;
    TxGuard& operator=(const TxGuard&) = delete;

    ~TxGuard() noexcept {
        try {
            bool failed = std::uncaught_exceptions() > exc;
            if (top) {
                if (failed) {
                    b->rollback();
                } else {
                    b->commit();
                }
            } else if (failed) {
                // Unwind the inner block completely: roll back to the mark,
                // then forget the mark itself.
                b->rollback_to(sp);
                b->release_savepoint(sp);
            } else {
                b->release_savepoint(sp);
            }
        } catch (...) {
            // A destructor that throws terminates the program, and a guard
            // whose commit failed has nothing useful left to say.
        }
    }
};

// The ambient connection. One per thread, matching how the auth guard scopes
// its per-thread state.
inline DbBackend*& db_slot() {
    static thread_local DbBackend* b = nullptr;
    return b;
}
inline void db_set(DbBackend* b) { db_slot() = b; }
inline DbBackend* db_get() { return db_slot(); }

/// The open database, or a clear failure. Pointer, not reference, so it can be
/// stored in a generated call the same way `db_get` hands it out.
inline DbBackend* db_need() {
    DbBackend* b = db_slot();
    if (!b) throw std::runtime_error("db: no database is open");
    return b;
}

// Quote an identifier. Schema-derived only, so this escapes rather than
// validates.
inline std::string db_quote_ident(const std::string& n) {
    std::string out = "\"";
    for (char c : n) {
        if (c == '"') out.push_back('"');
        out.push_back(c);
    }
    out.push_back('"');
    return out;
}

// Quote an identifier that may be qualified, so a `table.column` becomes
// `"table"."column"` rather than one nonsense name. Used only by the join
// condition, which is the one place a column lives on a second table.
inline std::string db_quote_path(const std::string& n) {
    std::string out;
    size_t start = 0;
    while (start <= n.size()) {
        size_t dot = n.find('.', start);
        std::string part = n.substr(start, dot == std::string::npos ? std::string::npos : dot - start);
        if (!out.empty()) out += ".";
        out += db_quote_ident(part);
        if (dot == std::string::npos) break;
        start = dot + 1;
    }
    return out;
}

// The nth placeholder of a dialect (`?` for SQLite, `$n` for PostgreSQL,
// counting from one).
inline std::string db_placeholder(DbDialect d, int n) {
    if (d == DbDialect::Sqlite) return "?";
    return "$" + std::to_string(n);
}

// The canonical table name a model is stored under. Generated code uses this,
// and so do the DDL writers, so a table can never be spelled two ways.
inline std::string db_table_of(const OrmModel& m) { return m.table; }

// Encode a value for a placeholder, per column kind.
inline std::string db_bind_text(const Val& v, OrmKind k) {
    switch (k) {
        case OrmKind::Bool:
            if (v.is_bool()) return v.bv ? "1" : "0";
            if (v.is_int()) return v.iv ? "1" : "0";
            return v.truthy() ? "1" : "0";
        case OrmKind::Int:
        case OrmKind::Float:
            if (v.is_nil()) return "NULL";
            if (v.is_num()) {
                std::string s = v.is_flt() ? std::to_string(v.fv) : std::to_string(v.iv);
                return s;
            }
            return v.is_str() ? v.sv : "NULL";
        case OrmKind::Json:
            if (v.is_str()) return v.sv;
            return v.to_json();
        default:
            if (v.is_nil()) return "NULL";
            if (v.is_str()) return v.sv;
            return v.to_text_dbg();
    }
}

// Decode one column of text into the `Val` the model is built from.
inline Val db_decode(const Val& raw, OrmKind k) {
    switch (k) {
        case OrmKind::Int:
            if (raw.is_int()) return raw;
            if (raw.is_flt()) return Val::int_((int64_t)raw.fv);
            if (raw.is_str()) {
                try {
                    return Val::int_(std::stoll(raw.sv));
                } catch (...) {
                    return Val::nil();
                }
            }
            return Val::nil();
        case OrmKind::Float:
            if (raw.is_num()) return Val::flt(raw.num());
            if (raw.is_str()) {
                try {
                    return Val::flt(std::stod(raw.sv));
                } catch (...) {
                    return Val::nil();
                }
            }
            return Val::nil();
        case OrmKind::Bool:
            if (raw.is_bool()) return raw;
            if (raw.is_int()) return Val::boolean(raw.iv != 0);
            if (raw.is_flt()) return Val::boolean(raw.fv != 0);
            if (raw.is_str()) return Val::boolean(raw.sv == "1" || raw.sv == "true" || raw.sv == "t");
            return Val::nil();
        case OrmKind::Json:
            if (raw.is_str()) return hs::parse_json(raw.sv);
            return raw;
        default:
            if (raw.is_str()) return raw;
            if (raw.is_num() || raw.is_bool()) return Val::text(raw.to_text_dbg());
            return raw;
    }
}

/// One result row as an object keyed by column name, decoded through the
/// model's column kinds. Columns past the names are `col<N>`: a projection
/// that names nothing is still a row worth handing out.
inline Val orm_decode_row(const OrmModel* m, const std::vector<std::string>& columns,
                          const std::vector<Val>& row) {
    Val o = Val::object({});
    for (size_t i = 0; i < row.size(); i++) {
        std::string key = i < columns.size() ? columns[i] : ("col" + std::to_string(i));
        OrmKind k = m ? m->kind_of(key) : OrmKind::Text;
        o.set(key, db_decode(row[i], k));
    }
    return o;
}

enum class CondOp : uint8_t { Eq, Ne, Lt, Le, Gt, Ge, Like, In };

inline const char* cond_sql(CondOp op) {
    switch (op) {
        case CondOp::Eq: return "=";
        case CondOp::Ne: return "<>";
        case CondOp::Lt: return "<";
        case CondOp::Le: return "<=";
        case CondOp::Gt: return ">";
        case CondOp::Ge: return ">=";
        // SQLite has LIKE built in; PostgreSQL needs ILIKE for the
        // case-insensitive match the pattern syntax implies.
        case CondOp::Like: return "LIKE";
        case CondOp::In: return "IN";
    }
    return "=";
}

// A fluent query over one model.
//
// Every method returns `*this`, so the generated code is a straight chain and
// the plan is inspectable before anything runs.
struct OrmQuery {
    const OrmModel* m = nullptr;
    /// How a value is stored, which is how it has to be bound. The record's own
    /// value is the honest answer here: a relationship's target column can sit
    /// on a third table the model being queried knows nothing about.
    static OrmKind orm_kind_of(const Val& v) {
        switch (v.t) {
            case Val::T::Int: return OrmKind::Int;
            case Val::T::Flt: return OrmKind::Float;
            case Val::T::Bool: return OrmKind::Bool;
            default: return OrmKind::Text;
        }
    }

    struct Cond {
        std::string column;
        CondOp op = CondOp::Eq;
        OrmKind kind = OrmKind::Text;
        std::vector<Val> values;  // one for a comparison, N for `in`
        // A relationship's own predicate. The column is on the *target* and
        // the value comes off the *parent* record, so the comparison can only
        // be bound once the record is known; the SQL text is the same either
        // way, which is what keeps `sql()` exact.
        std::string parent_col;
    };
    std::vector<Cond> conds;
    int64_t limit_n = -1;
    int64_t offset_n = -1;
    std::vector<std::pair<std::string, bool>> order;
    /// The record a relationship is read from, and any `INNER JOIN` the link
    /// needs. Both come from the schema by way of the compiler.
    Val parent;
    std::string join_sql;

    OrmQuery() = default;
    explicit OrmQuery(const OrmModel* mm) : m(mm) {}

    // ---- builder ----
    OrmQuery& where_eq(const std::string& col, const Val& v) { return add(col, CondOp::Eq, {v}); }
    OrmQuery& where_ne(const std::string& col, const Val& v) { return add(col, CondOp::Ne, {v}); }
    OrmQuery& where_lt(const std::string& col, const Val& v) { return add(col, CondOp::Lt, {v}); }
    OrmQuery& where_le(const std::string& col, const Val& v) { return add(col, CondOp::Le, {v}); }
    OrmQuery& where_gt(const std::string& col, const Val& v) { return add(col, CondOp::Gt, {v}); }
    OrmQuery& where_ge(const std::string& col, const Val& v) { return add(col, CondOp::Ge, {v}); }
    OrmQuery& where_like(const std::string& col, const Val& v) { return add(col, CondOp::Like, {v}); }
    /// Takes a list value rather than a `std::vector`, so a generated
    /// `where_in(id, [1, 2])` can pass the list expression straight through.
    OrmQuery& where_in(const std::string& col, const Val& list) {
        std::vector<Val> vs;
        if (list.is_arr()) {
            for (const auto& v : list.arr) vs.push_back(v);
        } else if (!list.is_nil()) {
            // A single value is one-element membership, which is almost always
            // what was meant and never what `IN` on a non-list would mean.
            vs.push_back(list);
        }
        return add(col, CondOp::In, std::move(vs));
    }
    OrmQuery& limit(int64_t n) {
        limit_n = n < 0 ? -1 : n;
        return *this;
    }
    OrmQuery& offset(int64_t n) {
        offset_n = n < 0 ? -1 : n;
        return *this;
    }
    OrmQuery& order_by(const std::string& col, bool desc) {
        order.emplace_back(col, desc);
        return *this;
    }

    /// Read a relationship off `rec`: rows whose `target_col` equals the
    /// parent record's `parent_col`, joined through `join` when the link needs
    /// a third table. The compiler has already resolved both column names.
    OrmQuery& of_record(const Val& rec, const std::string& parent_col, const std::string& target_col,
                        const std::string& join = std::string()) {
        parent = rec;
        join_sql = join;
        Cond c;
        c.column = target_col;
        c.parent_col = parent_col;
        c.op = CondOp::Eq;
        conds.insert(conds.begin(), std::move(c));
        return *this;
    }

    OrmQuery& add(const std::string& col, CondOp op, std::vector<Val> vs) {
        Cond c;
        c.column = col;
        c.op = op;
        c.kind = m ? m->kind_of(col) : OrmKind::Text;
        c.values = std::move(vs);
        conds.push_back(std::move(c));
        return *this;
    }

    // ---- assembly ----
    DbDialect dialect(DbBackend* b) const {
        return b ? b->dialect() : DbDialect::Sqlite;
    }

    /// The `WHERE` clause and its bound values. Placeholders are numbered in
    /// the order the values are pushed, so the two always line up.
    void where_sql(DbDialect d, std::string& sql, std::vector<std::string>& bound) const {
        if (conds.empty()) return;
        sql += " WHERE ";
        bool first = true;
        for (const auto& c : conds) {
            if (!first) sql += " AND ";
            first = false;
            std::string col = c.column.find('.') == std::string::npos ? db_quote_ident(c.column)
                                                                      : db_quote_path(c.column);
            if (!c.parent_col.empty()) {
                // The value is not known until the record is, so it is read
                // here rather than stored in the condition. A parent that has
                // no such column matches nothing: a relationship read from a
                // record that never had the key is empty, not an error.
                const Val* pv = parent.find(c.parent_col);
                Val v = pv ? *pv : Val::nil();
                if (pv) {
                    sql += col + " = " + db_placeholder(d, (int)bound.size() + 1);
                    bound.push_back(db_bind_text(v, orm_kind_of(v)));
                } else {
                    sql += "1 = 0";
                }
                continue;
            }
            if (c.op == CondOp::In) {
                // An empty IN list is a query for nothing, and the SQL way to
                // say that portably is a false predicate. Emitting `IN ()` is
                // a syntax error on both backends.
                if (c.values.empty()) {
                    sql += "1 = 0";
                    continue;
                }
                sql += col + " IN (";
                for (size_t i = 0; i < c.values.size(); i++) {
                    if (i) sql += ", ";
                    sql += db_placeholder(d, (int)bound.size() + 1);
                    bound.push_back(db_bind_text(c.values[i], c.kind));
                }
                sql += ")";
                continue;
            }
            // A comparison against nothing is a question SQL answers with
            // `NULL`, and `col = NULL` is never true -- so asking for a row
            // whose column is null by writing a bound nil would silently match
            // nothing. `IS NULL` is what the programmer meant, and it binds
            // nothing at all.
            if ((c.op == CondOp::Eq || c.op == CondOp::Ne) && c.values.size() == 1 &&
                c.values[0].is_nil()) {
                sql += col + (c.op == CondOp::Eq ? " IS NULL" : " IS NOT NULL");
                continue;
            }
            sql += col;
            if (c.op == CondOp::Like) {
                // ILIKE on PostgreSQL, LIKE on SQLite: SQLite's LIKE is already
                // case-insensitive for ASCII.
                sql += (d == DbDialect::Postgres) ? " ILIKE " : " LIKE ";
            } else {
                sql += " ";
                sql += cond_sql(c.op);
                sql += " ";
            }
            sql += db_placeholder(d, (int)bound.size() + 1);
            bound.push_back(db_bind_text(c.values.empty() ? Val::nil() : c.values[0], c.kind));
        }
    }

    void order_limit_sql(DbDialect d, std::string& sql) const {
        if (!order.empty()) {
            sql += " ORDER BY ";
            for (size_t i = 0; i < order.size(); i++) {
                if (i) sql += ", ";
                sql += (order[i].first.find(".") == std::string::npos ? db_quote_ident(order[i].first) : db_quote_path(order[i].first));
                if (order[i].second) sql += " DESC";
            }
        }
        if (limit_n >= 0) {
            sql += " LIMIT " + std::to_string(limit_n);
        } else if (offset_n >= 0 && d == DbDialect::Sqlite) {
            // SQLite needs a LIMIT before OFFSET; -1 means "no limit".
            sql += " LIMIT -1";
        }
        if (offset_n >= 0) sql += " OFFSET " + std::to_string(offset_n);
    }

    /// The columns a SELECT lists: the model's own, or a narrowed set.
    std::vector<std::string> select_columns(const std::vector<std::string>& only) const {
        if (!only.empty()) return only;
        return m ? m->columns : std::vector<std::string>();
    }

    std::string build_select(DbDialect d, const std::vector<std::string>& cols,
                             std::vector<std::string>& bound) const {
        std::string sql = "SELECT ";
        if (cols.empty()) {
            sql += "*";
        } else {
            for (size_t i = 0; i < cols.size(); i++) {
                if (i) sql += ", ";
                sql += db_quote_ident(cols[i]);
            }
        }
        sql += " FROM ";
        sql += db_quote_ident(m ? m->table : std::string());
        if (!join_sql.empty()) {
            sql += " ";
            sql += join_sql;
        }
        where_sql(d, sql, bound);
        order_limit_sql(d, sql);
        return sql;
    }

    /// The statement this query would run, for inspection and tests.
    std::string sql(DbBackend* b = nullptr) const {
        std::vector<std::string> bound;
        return build_select(dialect(b), select_columns({}), bound);
    }

    /// The bound values, in placeholder order.
    std::vector<std::string> bound_values() const {
        std::vector<std::string> out;
        std::string ignored;
        DbDialect d = DbDialect::Sqlite;
        where_sql(d, ignored, out);
        return out;
    }

    // ---- execution ----
    DbResult run(DbBackend* b, const std::vector<std::string>& cols) const {
        std::vector<Val> params;
        // Values are carried as Vals to the backend for binding; the text form
        // is only for the SQL text and for the bound list the tests read.
        for (const auto& c : conds) {
            if (!c.parent_col.empty()) {
                // The relationship's own value, read off the record. It is
                // skipped when the record has no such column, because
                // `where_sql` rendered a false predicate that binds nothing.
                if (const Val* pv = parent.find(c.parent_col)) params.push_back(*pv);
                continue;
            }
            // `IS NULL` binds nothing, so a nil value is not a parameter here
            // either. The two have to agree or the placeholders shift.
            if ((c.op == CondOp::Eq || c.op == CondOp::Ne) && c.values.size() == 1 &&
                c.values[0].is_nil())
                continue;
            for (const auto& v : c.values) params.push_back(v);
        }
        DbDialect d = b->dialect();
        std::vector<std::string> bound;
        std::string sql = build_select(d, select_columns(cols), bound);
        return b->run(sql, params);
    }

    /// Every matching row, as a list of objects keyed by column name.
    Val all(DbBackend* b, const std::vector<std::string>& cols = {}) const {
        DbResult r = run(b, cols);
        std::vector<Val> out;
        out.reserve(r.rows.size());
        for (const auto& row : r.rows) out.push_back(orm_decode_row(m, r.columns, row));
        return Val::list(std::move(out));
    }

    /// The first row, or nil. `order_by` decides which row that is, so an
    /// unordered `first` is a database's choice and not a promise.
    Val first(DbBackend* b, const std::vector<std::string>& cols = {}) const {
        OrmQuery q = *this;
        if (q.limit_n < 0) q.limit_n = 1;
        DbResult r = q.run(b, cols);
        if (r.rows.empty()) return Val::nil();
        return orm_decode_row(m, r.columns, r.rows[0]);
    }

    /// A `COUNT(*)` over the same predicates, so a count and a page of rows
    /// always describe the same set.
    Val count(DbBackend* b) const {
        std::string sql = "SELECT COUNT(*)";
        sql += " FROM ";
        sql += db_quote_ident(m ? m->table : std::string());
        std::vector<std::string> bound;
        where_sql(b->dialect(), sql, bound);
        std::vector<Val> params;
        for (const auto& c : conds) {
            if ((c.op == CondOp::Eq || c.op == CondOp::Ne) && c.values.size() == 1 &&
                c.values[0].is_nil())
                continue;
            for (const auto& v : c.values) params.push_back(v);
        }
        DbResult r = b->run(sql, params);
        if (r.rows.empty() || r.rows[0].empty()) return Val::int_(0);
        const Val& v = r.rows[0][0];
        if (v.is_int()) return v;
        if (v.is_str()) {
            try {
                return Val::int_(std::stoll(v.sv));
            } catch (...) {
                return Val::int_(0);
            }
        }
        return Val::int_(0);
    }

    /// Whether any row matches. `LIMIT 1` is pushed into SQL rather than
    /// counting in C++, so this stays O(1) on the database.
    Val exists(DbBackend* b) const {
        OrmQuery q = *this;
        q.limit_n = 1;
        DbResult r = q.run(b, {});
        return Val::boolean(!r.rows.empty());
    }
};

// ---- writes (M5.3.3) ------------------------------------------------------
//
// A write is assembled from the same model metadata a read is, so the column
// names in an INSERT or UPDATE come from the schema and never from the record
// being saved. The record only decides *which* columns are present.

/// The columns and values of an insert built from `obj`.
///
/// A column the database generates is left out unless the record carries it,
/// so `create({name: "x"})` on an auto-increment table inserts without naming
/// the key. A column that is neither present nor generated has no value to
/// insert, which the compiler has already rejected; here it is a hard error
/// rather than a malformed statement.
inline bool orm_insert_parts(const OrmModel& m, const Val& obj, std::vector<std::string>& cols,
                             std::vector<Val>& vals) {
    if (!obj.is_obj()) return false;
    for (const auto& c : m.columns) {
        const Val* v = obj.find(c);
        if (v) {
            cols.push_back(c);
            vals.push_back(*v);
        } else if (!m.is_generated(c)) {
            return false;
        }
    }
    return true;
}

inline std::string orm_insert_sql(const OrmModel& m, DbDialect d, const std::vector<std::string>& cols) {
    std::string sql = "INSERT INTO ";
    sql += db_quote_ident(m.table);
    sql += " (";
    for (size_t i = 0; i < cols.size(); i++) {
        if (i) sql += ", ";
        sql += db_quote_ident(cols[i]);
    }
    sql += ") VALUES (";
    for (size_t i = 0; i < cols.size(); i++) {
        if (i) sql += ", ";
        sql += db_placeholder(d, (int)i + 1);
    }
    sql += ")";
    // SQLite remembers the key it assigned and hands it back afterwards.
    // PostgreSQL keeps no such record, so the statement asks for the key it
    // just made and the row is the answer. Same insert, one extra clause, and
    // no second round trip to find out what the id was.
    if (d == DbDialect::Postgres && !m.pk.empty() && m.is_generated(m.pk)) {
        sql += " RETURNING ";
        sql += db_quote_ident(m.pk);
    }
    return sql;
}

/// The `SET` half of an update, from the record's own columns.
///
/// The key is excluded: it is the `WHERE`, not a value to write, and letting it
/// be reassigned would make a save able to move a row.
inline bool orm_update_parts(const OrmModel& m, const Val& obj, std::vector<std::string>& cols,
                             std::vector<Val>& vals) {
    if (!obj.is_obj()) return false;
    for (const auto& c : m.columns) {
        if (c == m.pk) continue;
        if (const Val* v = obj.find(c)) {
            cols.push_back(c);
            vals.push_back(*v);
        }
    }
    return !cols.empty();
}

inline std::string orm_update_sql(const OrmModel& m, DbDialect d,
                                  const std::vector<std::string>& cols) {
    std::string sql = "UPDATE ";
    sql += db_quote_ident(m.table);
    sql += " SET ";
    for (size_t i = 0; i < cols.size(); i++) {
        if (i) sql += ", ";
        sql += db_quote_ident(cols[i]);
        sql += " = ";
        sql += db_placeholder(d, (int)i + 1);
    }
    sql += " WHERE ";
    sql += db_quote_ident(m.pk);
    sql += " = ";
    sql += db_placeholder(d, (int)cols.size() + 1);
    return sql;
}

inline std::string orm_delete_sql(const OrmModel& m, DbDialect d) {
    std::string sql = "DELETE FROM ";
    sql += db_quote_ident(m.table);
    sql += " WHERE ";
    sql += db_quote_ident(m.pk);
    sql += " = ";
    sql += db_placeholder(d, 1);
    return sql;
}

/// Read the generated key off an insert result and onto the record. A key the
/// record already carried is the caller's, and is kept: an upsert that
/// inserts a keyed row has to come back with the key it was given.
/// A backend answers either by handing the key back (`last_id`, SQLite's
/// connection state) or by returning the row that has it (`RETURNING`,
/// PostgreSQL's); both are the same number and either one is enough.
inline void orm_attach_key(const OrmModel& m, Val& out, const DbResult& r) {
    if (m.pk.empty() || out.find(m.pk)) return;
    if (r.last_id > 0) {
        out.set(m.pk, Val::int_(r.last_id));
        return;
    }
    if (!r.rows.empty() && !r.rows[0].empty() && r.rows[0][0].is_int())
        out.set(m.pk, r.rows[0][0]);
}

/// `INSERT` one record. Returns the new row as an object, including the key the
/// database assigned, so the caller does not have to read it back.
inline Val orm_create(const OrmModel* mp, DbBackend* b, const Val& obj) {
    const OrmModel& m = *mp;
    std::vector<std::string> cols;
    std::vector<Val> vals;
    if (!orm_insert_parts(m, obj, cols, vals)) {
        throw std::runtime_error("orm: " + m.name + " is missing a value for a column with no default");
    }
    DbResult r = b->run(orm_insert_sql(m, b->dialect(), cols), vals);
    Val out = obj.is_obj() ? obj : Val::object({});
    orm_attach_key(m, out, r);
    return out;
}

// ===========================================================================
// Batch operations
// ===========================================================================

/// How many bound values one statement may carry. SQLite historically caps at
/// 999 and PostgreSQL at 65535; staying below both keeps one code path honest
/// on every backend.
inline size_t db_chunk_vars(DbDialect d) {
    return d == DbDialect::Postgres ? 32000 : 900;
}

/// Insert every record of a list, in one transaction, and hand back the
/// created records with their keys. A record that fails validation fails the
/// whole batch: a half-inserted list is the thing transactions are for.
/// Generated columns are skipped per record exactly as `create` skips them.
inline Val orm_create_many(const OrmModel* mp, DbBackend* b, const Val& list) {
    const OrmModel& m = *mp;
    if (!list.is_arr()) throw std::runtime_error("orm: " + m.name + ".create_many needs a list of records");
    if (list.arr.empty()) return Val::list({});
    TxGuard tx(b);
    std::vector<Val> out;
    out.reserve(list.arr.size());
    size_t n = 0;
    for (const auto& rec : list.arr) {
        n++;
        if (!rec.is_obj())
            throw std::runtime_error("orm: " + m.name + ".create_many record " + std::to_string(n) +
                                     " is not an object");
        std::vector<std::string> cols;
        std::vector<Val> vals;
        if (!orm_insert_parts(m, rec, cols, vals)) {
            throw std::runtime_error("orm: " + m.name + ".create_many record " + std::to_string(n) +
                                     " is missing a value for a column with no default");
        }
        DbResult r = b->run(orm_insert_sql(m, b->dialect(), cols), vals);
        Val created = rec;
        orm_attach_key(m, created, r);
        out.push_back(std::move(created));
    }
    return Val::list(std::move(out));
}

/// Update every record of a list by its key, in one transaction, and hand
/// back the records. A record without a key cannot be addressed and fails the
/// batch; a record with nothing but its key is left alone, the way `save`
/// leaves it. Records that set the same columns share one prepared statement
/// through `run_many`.
inline Val orm_update_many(const OrmModel* mp, DbBackend* b, const Val& list) {
    const OrmModel& m = *mp;
    if (!list.is_arr()) throw std::runtime_error("orm: " + m.name + ".update_many needs a list of records");
    if (list.arr.empty()) return Val::list({});
    TxGuard tx(b);
    // Group by set-clause so each group shares one statement.
    std::map<std::string, size_t> groups;
    std::vector<std::vector<std::string>> group_cols;
    std::vector<std::vector<std::vector<Val>>> group_sets;
    std::vector<Val> out;
    out.reserve(list.arr.size());
    size_t n = 0;
    for (const auto& rec : list.arr) {
        n++;
        if (!rec.is_obj())
            throw std::runtime_error("orm: " + m.name + ".update_many record " + std::to_string(n) +
                                     " is not an object");
        const Val* k = rec.find(m.pk);
        if (!k || k->is_nil())
            throw std::runtime_error("orm: " + m.name + ".update_many record " + std::to_string(n) +
                                     " has no " + m.pk + " to identify it");
        std::vector<std::string> cols;
        std::vector<Val> vals;
        if (!orm_update_parts(m, rec, cols, vals)) {
            out.push_back(rec);
            continue;
        }
        std::string gk;
        for (const auto& c : cols) {
            gk += c;
            gk += '\x1f';
        }
        auto it = groups.find(gk);
        size_t gi;
        if (it == groups.end()) {
            gi = group_cols.size();
            groups[gk] = gi;
            group_cols.push_back(cols);
            group_sets.emplace_back();
        } else {
            gi = it->second;
        }
        vals.push_back(*k);
        group_sets[gi].push_back(std::move(vals));
        out.push_back(rec);
    }
    for (size_t gi = 0; gi < group_cols.size(); gi++) {
        b->run_many(orm_update_sql(m, b->dialect(), group_cols[gi]), group_sets[gi]);
    }
    return Val::list(std::move(out));
}

/// Delete the rows with the given keys, in one transaction, and report how
/// many went. Long id lists are cut into chunks that fit the backend's
/// variable cap, because one statement per thousand keys is still one
/// transaction.
inline Val orm_delete_many(const OrmModel* mp, DbBackend* b, const Val& ids) {
    const OrmModel& m = *mp;
    if (!ids.is_arr()) throw std::runtime_error("orm: " + m.name + ".delete_many needs a list of keys");
    if (ids.arr.empty() || m.pk.empty()) return Val::int_(0);
    TxGuard tx(b);
    int64_t total = 0;
    size_t cap = db_chunk_vars(b->dialect());
    for (size_t i = 0; i < ids.arr.size(); i += cap) {
        size_t n = std::min(cap, ids.arr.size() - i);
        std::string sql = "DELETE FROM ";
        sql += db_quote_ident(m.table);
        sql += " WHERE ";
        sql += db_quote_ident(m.pk);
        sql += " IN (";
        for (size_t j = 0; j < n; j++) {
            if (j) sql += ", ";
            sql += db_placeholder(b->dialect(), (int)j + 1);
        }
        sql += ")";
        std::vector<Val> params(ids.arr.begin() + (int64_t)i, ids.arr.begin() + (int64_t)(i + n));
        total += b->run(sql, params).affected;
    }
    return Val::int_(total);
}

/// The map key for one id value. Integers and strings with the same text are
/// different keys on most databases, so the kind travels with the value.
inline std::string db_key_text(const Val& v) {
    if (v.is_int()) return "i:" + std::to_string(v.iv);
    if (v.is_flt()) return "f:" + std::to_string(v.fv);
    if (v.is_bool()) return v.truthy() ? "b:1" : "b:0";
    if (v.is_str()) return "s:" + v.sv;
    return "n:";
}

/// Read the rows with the given keys, in the keys' order, skipping keys with
/// no row. `IN` does not promise an order, so the rows are matched back to
/// the keys that asked for them.
inline Val orm_find_many(const OrmModel* mp, DbBackend* b, const Val& ids) {
    const OrmModel& m = *mp;
    if (!ids.is_arr()) throw std::runtime_error("orm: " + m.name + ".find_many needs a list of keys");
    if (ids.arr.empty() || m.pk.empty()) return Val::list({});
    std::vector<std::string> cols;
    for (const auto& c : m.columns) cols.push_back(c);
    std::map<std::string, Val> by_key;
    size_t cap = db_chunk_vars(b->dialect());
    for (size_t i = 0; i < ids.arr.size(); i += cap) {
        size_t n = std::min(cap, ids.arr.size() - i);
        std::string sql = "SELECT ";
        for (size_t c = 0; c < cols.size(); c++) {
            if (c) sql += ", ";
            sql += db_quote_ident(cols[c]);
        }
        sql += " FROM ";
        sql += db_quote_ident(m.table);
        sql += " WHERE ";
        sql += db_quote_ident(m.pk);
        sql += " IN (";
        for (size_t j = 0; j < n; j++) {
            if (j) sql += ", ";
            sql += db_placeholder(b->dialect(), (int)j + 1);
        }
        sql += ")";
        std::vector<Val> params(ids.arr.begin() + (int64_t)i, ids.arr.begin() + (int64_t)(i + n));
        DbResult r = b->run(sql, params);
        for (const auto& row : r.rows) {
            Val rec = orm_decode_row(mp, r.columns, row);
            const Val* k = rec.find(m.pk);
            if (k) by_key[db_key_text(*k)] = rec;
        }
    }
    std::vector<Val> out;
    for (const auto& id : ids.arr) {
        auto it = by_key.find(db_key_text(id));
        if (it != by_key.end()) out.push_back(it->second);
    }
    return Val::list(std::move(out));
}

/// The primary key of a loaded record, or a clear failure. A record that came
/// back from a projection without its key cannot be written, and saying so is
/// better than a statement that matches nothing.
inline Val orm_key_of(const OrmModel* mp, const Val& rec) {
    const OrmModel& m = *mp;
    if (!rec.is_obj()) throw std::runtime_error("orm: " + m.name + " record is not an object");
    const Val* k = rec.find(m.pk);
    if (!k || k->is_nil()) {
        throw std::runtime_error("orm: " + m.name + " record has no " + m.pk +
                                 " to identify it; include it in the projection");
    }
    return *k;
}

/// Update the row a record came from, then return the record with its new
/// values. A record without a key is an insert, which is what `save` means
/// for a value that was never stored.
inline Val orm_save(const OrmModel* mp, DbBackend* b, const Val& rec) {
    const OrmModel& m = *mp;
    const Val* k = rec.is_obj() ? rec.find(m.pk) : nullptr;
    if (!k || k->is_nil()) return orm_create(mp, b, rec);

    std::vector<std::string> cols;
    std::vector<Val> vals;
    if (!orm_update_parts(m, rec, cols, vals)) {
        // Nothing to write: the record holds only its key. Leave the row alone
        // rather than issuing `UPDATE ... SET` with an empty list.
        return rec;
    }
    vals.push_back(*k);
    b->run(orm_update_sql(m, b->dialect(), cols), vals);
    return rec;
}

/// Refresh a row's `@updated_at` column. The value is the database's clock, so
/// two servers writing the same row cannot disagree about the time.
inline Val orm_touch(const OrmModel* mp, DbBackend* b, const Val& rec) {
    const OrmModel& m = *mp;
    const Val key = orm_key_of(mp, rec);
    if (m.updated_col.empty()) {
        throw std::runtime_error("orm: " + m.name + " has no @updated_at column to touch");
    }
    std::string sql = "UPDATE ";
    sql += db_quote_ident(m.table);
    sql += " SET ";
    sql += db_quote_ident(m.updated_col);
    if (b->dialect() == DbDialect::Postgres) {
        sql += " = NOW() WHERE ";
    } else {
        sql += " = CURRENT_TIMESTAMP WHERE ";
    }
    sql += db_quote_ident(m.pk);
    sql += " = ";
    sql += db_placeholder(b->dialect(), 1);
    b->run(sql, {key});
    return rec;
}

/// Delete the row a record came from. Reports whether a row went away, so a
/// delete of something already gone is distinguishable from a delete that hit.
inline Val orm_delete(const OrmModel* mp, DbBackend* b, const Val& rec) {
    const OrmModel& m = *mp;
    const Val key = orm_key_of(mp, rec);
    DbResult r = b->run(orm_delete_sql(m, b->dialect()), {key});
    return Val::boolean(r.affected > 0);
}

/// `Model.delete(key)`, for a key rather than a loaded record.
inline Val orm_delete_key(const OrmModel* mp, DbBackend* b, const Val& key) {
    const OrmModel& m = *mp;
    if (key.is_nil()) throw std::runtime_error("orm: " + m.name + " delete needs a key");
    DbResult r = b->run(orm_delete_sql(m, b->dialect()), {key});
    return Val::boolean(r.affected > 0);
}

/// Update by key, inserting when the key names no row. The key is written
/// explicitly rather than left to the sequence, so an upsert with an id
/// behaves the same on both backends.
inline Val orm_upsert(const OrmModel* mp, DbBackend* b, const Val& obj) {
    const OrmModel& m = *mp;
    const Val* k = obj.is_obj() ? obj.find(m.pk) : nullptr;
    if (!k || k->is_nil()) return orm_create(mp, b, obj);

    std::vector<std::string> cols;
    std::vector<Val> vals;
    if (orm_update_parts(m, obj, cols, vals)) {
        vals.push_back(*k);
        DbResult r = b->run(orm_update_sql(m, b->dialect(), cols), vals);
        if (r.affected > 0) return obj;
    }
    return orm_create(mp, b, obj);
}

/// Read by a set of attributes, inserting them as a new row if nothing
/// matches. The lookup is a conjunction of equality, so the record that comes
/// back is one the caller would have got from the same `where`.
inline Val orm_find_or_create(const OrmModel* mp, DbBackend* b, const Val& attrs) {
    const OrmModel& m = *mp;
    if (!attrs.is_obj()) throw std::runtime_error("orm: " + m.name + " find_or_create needs an object");
    OrmQuery q(&m);
    for (const auto& kv : attrs.obj) {
        if (!m.has(kv.first)) {
            throw std::runtime_error("orm: " + m.name + " has no field " + kv.first);
        }
        q.where_eq(kv.first, kv.second);
    }
    Val found = q.first(b);
    if (!found.is_nil()) return found;
    return orm_create(mp, b, attrs);
}

// Register a model. Generated code calls this once per `model` declaration.
inline OrmModel& orm_register_model(const std::string& name, const std::string& table,
                                    const std::string& pk, std::vector<std::string> cols,
                                    std::vector<OrmKind> kinds) {
    static std::vector<std::unique_ptr<OrmModel>> reg;
    reg.push_back(std::make_unique<OrmModel>(name, table, pk, std::move(cols), std::move(kinds)));
    return *reg.back();
}

}  // namespace hs

#endif  // HS_RUNTIME_ORM_HPP
