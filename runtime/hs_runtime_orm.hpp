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
    virtual void begin() = 0;
    virtual void commit() = 0;
    virtual void rollback() = 0;
    // Nesting depth of open transactions on this connection.
    virtual int tx_depth() const = 0;
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
    struct Cond {
        std::string column;
        CondOp op = CondOp::Eq;
        OrmKind kind = OrmKind::Text;
        std::vector<Val> values;  // one for a comparison, N for `in`
    };
    std::vector<Cond> conds;
    int64_t limit_n = -1;
    int64_t offset_n = -1;
    std::vector<std::pair<std::string, bool>> order;

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
            std::string col = db_quote_ident(c.column);
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
                sql += db_quote_ident(order[i].first);
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
        for (const auto& row : r.rows) {
            Val o = Val::object({});
            for (size_t i = 0; i < row.size(); i++) {
                std::string key = i < r.columns.size() ? r.columns[i] : ("col" + std::to_string(i));
                OrmKind k = m ? m->kind_of(key) : OrmKind::Text;
                o.set(key, db_decode(row[i], k));
            }
            out.push_back(std::move(o));
        }
        return Val::list(std::move(out));
    }

    /// The first row, or nil. `order_by` decides which row that is, so an
    /// unordered `first` is a database's choice and not a promise.
    Val first(DbBackend* b, const std::vector<std::string>& cols = {}) const {
        OrmQuery q = *this;
        if (q.limit_n < 0) q.limit_n = 1;
        DbResult r = q.run(b, cols);
        if (r.rows.empty()) return Val::nil();
        Val o = Val::object({});
        for (size_t i = 0; i < r.rows[0].size(); i++) {
            std::string key = i < r.columns.size() ? r.columns[i] : ("col" + std::to_string(i));
            OrmKind k = m ? m->kind_of(key) : OrmKind::Text;
            o.set(key, db_decode(r.rows[0][i], k));
        }
        return o;
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
        for (const auto& c : conds)
            for (const auto& v : c.values) params.push_back(v);
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
    // A generated key that was left out is the one number worth reading back.
    // A key the record already carried is the caller's, and is kept: an upsert
    // that inserts a keyed row has to come back with the key it was given.
    if (!m.pk.empty() && r.last_id > 0 && !out.find(m.pk)) out.set(m.pk, Val::int_(r.last_id));
    return out;
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
