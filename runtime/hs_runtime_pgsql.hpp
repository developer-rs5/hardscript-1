// hs_runtime_pgsql.hpp -- PostgreSQL backend for the ORM
//
// The other half of hs_runtime_postgres.hpp: that file speaks the wire
// protocol for `db_query`, where the SQL is one string a program wrote. This
// one speaks the *extended* protocol, where a statement is parsed once and its
// values travel separately, because a value that is interpolated into SQL is a
// value that can end the statement early.
//
// Both halves share the same connection code path in spirit and the same
// framing helpers; the difference is that a statement here is a message of its
// own with a row description to decode, not a string to hand back.

#ifndef HS_RUNTIME_PGSQL_HPP
#define HS_RUNTIME_PGSQL_HPP

#include "hs_runtime_postgres.hpp"
#include "hs_runtime_orm.hpp"

namespace hs {

// ===========================================================================
// Framing
// ===========================================================================

/// Reads messages off the socket, so that one short read does not become a
/// protocol error. A message is a type byte and an int32 length that counts the
/// length field itself.
struct PgReader {
    int fd = -1;
    std::string buf;
    size_t pos = 0;

    void need(size_t n) {
        // Used bytes are dropped once a good half of the buffer is spent. A
        // connection that runs a long query would otherwise hold every row it
        // ever read for as long as it stayed open.
        if (pos > 4096) {
            buf.erase(0, pos);
            pos = 0;
        }
        while (buf.size() - pos < n) {
            char tmp[8192];
            ssize_t got = recv(fd, tmp, sizeof tmp, 0);
            if (got <= 0) throw std::runtime_error("postgres: connection lost while reading a message");
            buf.append(tmp, (size_t)got);
        }
    }
    uint8_t u8() {
        need(1);
        return (uint8_t)buf[pos++];
    }
    int32_t i32() {
        need(4);
        uint32_t v = (uint8_t)buf[pos] << 24 | (uint8_t)buf[pos + 1] << 16 | (uint8_t)buf[pos + 2] << 8 |
                     (uint8_t)buf[pos + 3];
        pos += 4;
        return (int32_t)v;
    }
    int16_t i16() {
        need(2);
        int16_t v = (int16_t)((uint8_t)buf[pos] << 8 | (uint8_t)buf[pos + 1]);
        pos += 2;
        return v;
    }
    /// A NUL-terminated string, without the NUL.
    std::string cstr() {
        size_t end = buf.find('\0', pos);
        if (end == std::string::npos) {
            need(buf.size() - pos + 1);
            end = buf.find('\0', pos);
            if (end == std::string::npos) throw std::runtime_error("postgres: unterminated string in a message");
        }
        std::string s = buf.substr(pos, end - pos);
        pos = end + 1;
        return s;
    }
    std::string take(size_t n) {
        need(n);
        std::string s = buf.substr(pos, n);
        pos += n;
        return s;
    }
    /// One whole message: its type and its body, length prefix already off.
    char message(std::string& body) {
        char typ = (char)u8();
        int32_t len = i32();
        if (len < 4) throw std::runtime_error("postgres: message claims a length below the minimum");
        body = take((size_t)len - 4);
        return typ;
    }
};

inline void pg_put(std::string& out, const std::string& s) { out += s; }

inline void pg_put_i32(std::string& out, int32_t v) {
    out += (char)(uint8_t)((uint32_t)v >> 24);
    out += (char)(uint8_t)((uint32_t)v >> 16);
    out += (char)(uint8_t)((uint32_t)v >> 8);
    out += (char)(uint8_t)(uint32_t)v;
}

inline void pg_put_i16(std::string& out, int16_t v) {
    out += (char)(uint8_t)((uint16_t)v >> 8);
    out += (char)(uint8_t)(uint16_t)v;
}

/// One message: type byte, length, body.
inline void pg_send_msg(int fd, char type, const std::string& body) {
    std::string out;
    out += type;
    pg_put_i32(out, (int32_t)body.size() + 4);
    out += body;
    pg_write_all(fd, out);
}

inline void pg_put_cstr(std::string& out, const std::string& s) {
    out += s;
    out += '\0';
}

// ===========================================================================
// Values in, values out
// ===========================================================================

/// A double as the shortest string that reads back as the same double.
/// `%.17g` always round-trips and is almost never what a person wants to see in
/// a log, so each shorter form is tried until one survives the trip back.
inline std::string pg_double_text(double v) {
    char b[40];
    for (int prec = 15; prec <= 17; prec++) {
        snprintf(b, sizeof b, "%.*g", prec, v);
        if (strtod(b, nullptr) == v) break;
    }
    // Infinity and NaN have no SQL spelling in this protocol, and a database
    // that received "inf" would store a string. PostgreSQL has Infinity and
    // NaN spellings, which are what this must produce.
    double d = strtod(b, nullptr);
    if (d != d) return "NaN";
    if (d > 1.7976931348623157e308) return "Infinity";
    if (d < -1.7976931348623157e308) return "-Infinity";
    return b;
}

/// The text form of a value, or false when it has none. A list or an object is
/// not a SQL value, and saying so is better than sending its JSON and letting
/// the server guess. nil has no text form at all: NULL is a length of -1, not
/// a string, which is why the caller handles it before asking here.
inline bool pg_param_text(const Val& v, std::string& out) {
    if (v.is_int()) {
        out = std::to_string(v.iv);
        return true;
    }
    if (v.is_flt()) {
        out = pg_double_text(v.fv);
        return true;
    }
    if (v.is_bool()) {
        out = v.truthy() ? "t" : "f";
        return true;
    }
    if (v.is_str()) {
        out = v.sv;
        return true;
    }
    return false;
}

/// PostgreSQL type OIDs, for the columns a RowDescription names. A value comes
/// back as text, so the column's type is the only thing that says whether
/// "17" is the number seventeen or the string "17".
enum PgOid {
    PG_BOOL = 16,
    PG_BYTEA = 17,
    PG_CHAR = 18,
    PG_NAME = 19,
    PG_INT8 = 20,
    PG_INT2 = 21,
    PG_INT4 = 23,
    PG_TEXT = 25,
    PG_OID = 26,
    PG_XID = 28,
    PG_CID = 29,
    PG_FLOAT4 = 700,
    PG_FLOAT8 = 701,
    PG_MONEY = 790,
    PG_INET = 869,
    PG_BPCHAR = 1042,
    PG_VARCHAR = 1043,
    PG_DATE = 1082,
    PG_TIME = 1083,
    PG_TIMESTAMP = 1114,
    PG_NUMERIC = 1700,
    PG_UUID = 2950,
    PG_JSON = 114,
    PG_JSONB = 3802,
    PG_TIMESTAMPTZ = 1184,
    PG_ARRAY = 2277,
};

/// Decode one field, given the OID the server described it with. Anything not
/// listed arrives as text, which is the honest answer: the alternative is
/// guessing and being wrong about money.
inline Val pg_decode(uint32_t oid, const std::string& raw) {
    switch (oid) {
        case PG_BOOL:
            return Val::boolean(raw == "t" || raw == "true" || raw == "y" || raw == "yes" || raw == "1");
        case PG_INT2:
        case PG_INT4:
        case PG_INT8:
        case PG_OID:
        case PG_XID:
        case PG_CID: {
            errno = 0;
            long long n = strtoll(raw.c_str(), nullptr, 10);
            if (errno == ERANGE) return Val::text(raw);
            return Val::int_((int64_t)n);
        }
        case PG_FLOAT4:
        case PG_FLOAT8: {
            if (raw == "NaN") return Val::flt(0.0 / 0.0);
            return Val::flt(strtod(raw.c_str(), nullptr));
        }
        default:
            // A numeric, a timestamp, a uuid, json, an array: all text. Making
            // a float out of NUMERIC would quietly lose the digits that made
            // it money, and there is no Int that can hold 131072 digits.
            return Val::text(raw);
    }
}

// ===========================================================================
// The backend
// ===========================================================================

struct PgDb : DbBackend {
    int fd = -1;
    PgConnInfo info;
    int depth = 0;
    /// PostgreSQL remembers nothing about the last insert; a failed statement
    /// inside a transaction leaves the transaction unable to do anything until
    /// it is rolled back, and the next error says only that. Saying it here
    /// names the statement that caused it.
    bool aborted = false;
    std::string conninfo;
    /// Open savepoints, oldest first, stacked like the database stacks them.
    std::vector<std::string> sps;
    int sp_next = 0;

    PgDb() = default;
    explicit PgDb(const std::string& ci) { open(ci); }
    PgDb(const PgDb&) = delete;
    PgDb& operator=(const PgDb&) = delete;
    ~PgDb() override { close(); }

    const char* name() const override { return "postgres"; }
    DbDialect dialect() const override { return DbDialect::Postgres; }

    void open(const std::string& ci) {
        info = pg_parse_conninfo(ci);
        conninfo = ci;
        fd = socket(AF_INET, SOCK_STREAM, 0);
        if (fd < 0) throw std::runtime_error("postgres: cannot create a socket");
        struct sockaddr_in a;
        memset(&a, 0, sizeof a);
        a.sin_family = AF_INET;
        a.sin_port = htons((uint16_t)info.port);
        if (inet_pton(AF_INET, info.host.c_str(), &a.sin_addr) != 1) {
            close_fd();
            throw std::runtime_error("postgres: invalid host " + info.host);
        }
        if (::connect(fd, (struct sockaddr*)&a, sizeof a) != 0) {
            int e = errno;
            close_fd();
            throw std::runtime_error("postgres: cannot connect to " + info.host + ":" +
                                     std::to_string(info.port) + " -- " + strerror(e));
        }
        // The protocol is a ping-pong of small messages, which is exactly the
        // shape Nagle's algorithm delays: every driver sets TCP_NODELAY, and
        // so does this one.
        int one = 1;
        setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
        startup();
    }

    void close() {
        if (fd < 0) return;
        // A Terminate is how a server learns the connection ended on purpose
        // rather than by a crash, and it is the one message with no reply.
        pg_send_msg(fd, 'X', "");
        close_fd();
    }

    void close_fd() {
        if (fd >= 0) {
            ::close(fd);
            fd = -1;
        }
    }

    /// Send the startup message and read until the server says it is ready.
    void startup() {
        std::string body;
        pg_put_i32(body, 196608);  // protocol 3.0
        pg_put_cstr(body, "user");
        pg_put_cstr(body, info.user);
        pg_put_cstr(body, "database");
        pg_put_cstr(body, info.dbname);
        pg_put_cstr(body, "application_name");
        pg_put_cstr(body, "hardscript");
        if (!info.password.empty()) {
            pg_put_cstr(body, "password");
            pg_put_cstr(body, info.password);
        }
        body += '\0';
        std::string out;
        pg_put_i32(out, (int32_t)body.size() + 4);
        out += body;
        pg_write_all(fd, out);

        PgReader r;
        r.fd = fd;
        for (;;) {
            std::string payload;
            char typ = r.message(payload);
            if (typ == 'R') {
                authenticate(payload);
                continue;
            }
            if (typ == 'E') throw std::runtime_error("postgres: " + pg_error_text(payload));
            if (typ == 'S' || typ == 'K' || typ == 'N') continue;  // parameters, keys, notices
            if (typ == 'Z') return;                                 // ready
            throw std::runtime_error(std::string("postgres: unexpected message '") + typ +
                                     "' while connecting");
        }
    }

    void authenticate(const std::string& payload) {
        PgReader p;
        p.buf = payload;
        int32_t code = p.i32();
        if (code == 0) return;  // AuthenticationOk
        if (code == 3) {        // cleartext
            pg_put_cstr_raw('p', info.password);
            return;
        }
        if (code == 5) {  // md5
            std::string salt = p.take(4);
            std::string first = md5_hex(info.password + info.user);
            pg_put_cstr_raw('p', "md5" + md5_hex(first + salt));
            return;
        }
        if (code == 10) {
            throw std::runtime_error(
                "postgres: the server asks for SCRAM-SHA-256, which this client does not implement; "
                "set the server's password_encryption to md5 or use trust/cleartext for local work");
        }
        throw std::runtime_error("postgres: unsupported authentication method " + std::to_string(code));
    }

    void pg_put_cstr_raw(char type, const std::string& s) {
        std::string body;
        body += s;
        body += '\0';
        pg_send_msg(fd, type, body);
    }

    /// Fields of an ErrorResponse or NoticeResponse, as one line. The message
    /// alone is often not enough: a constraint violation without the constraint
    /// name and the detail sends the reader looking through the schema.
    static std::string pg_error_text(const std::string& payload) {
        std::string severity, code, message, detail, hint;
        PgReader p;
        p.buf = payload;
        while (p.pos < p.buf.size()) {
            char f = (char)p.u8();
            if (f == '\0') break;
            std::string v = p.cstr();
            if (f == 'S') severity = v;
            else if (f == 'C') code = v;
            else if (f == 'M') message = v;
            else if (f == 'D') detail = v;
            else if (f == 'H') hint = v;
        }
        std::string out = message;
        if (code.empty()) code = severity;
        if (!code.empty()) out += " (" + code + ")";
        if (!detail.empty()) out += " -- " + detail;
        if (!hint.empty()) out += " -- " + hint;
        return out;
    }

    /// Run one statement through the extended protocol and read every message
    /// it produces. Parse, Bind, Describe, Execute, Sync: five messages in, one
    /// `ReadyForQuery` out, which is the only reliable end of a statement.
    DbResult run(const std::string& sql, const std::vector<Val>& params) override {
        DbResult out;
        if (fd < 0) throw std::runtime_error("postgres: the connection is closed");
        if (aborted && depth > 0 && !is_rollback_sql(sql)) {
            throw std::runtime_error(
                "postgres: the transaction is already failed, so this statement cannot run; roll it back "
                "first (the statement that failed was: " + failed_stmt + ")");
        }

        std::string parse, describe, execute;
        pg_put_cstr(parse, "");  // an unnamed statement, so the server keeps no plan for us
        pg_put_cstr(parse, sql);
        // Zero declared parameter types: the server infers each one from where
        // it appears, which is right for every statement an ORM writes and
        // saves this client from a table of OIDs that could go stale.
        pg_put_i16(parse, 0);
        pg_send_msg(fd, 'P', parse);

        pg_send_msg(fd, 'B', build_bind(params, sql));

        pg_put_cstr(describe, "P");
        pg_send_msg(fd, 'D', describe);

        pg_put_cstr(execute, "");
        pg_put_i32(execute, 0);  // every row, not a batch of some
        pg_send_msg(fd, 'E', execute);
        pg_send_msg(fd, 'S', "");

        read_result(out, sql);
        return out;
    }

    /// One Bind message body for a parameter set. Shared by `run` and
    /// `run_many` so a value is encoded exactly once, in one place.
    static std::string build_bind(const std::vector<Val>& params, const std::string& sql) {
        std::string bind;
        pg_put_cstr(bind, "");  // unnamed portal
        pg_put_cstr(bind, "");
        pg_put_i16(bind, 1);
        pg_put_i16(bind, 0);  // every parameter in text format
        pg_put_i16(bind, (int16_t)params.size());
        for (const Val& v : params) {
            if (v.is_nil()) {
                // NULL is a length of -1. An empty string is a different value
                // and a column that holds NULL does not hold ''.
                pg_put_i32(bind, -1);
                continue;
            }
            std::string text;
            if (!pg_param_text(v, text)) {
                throw std::runtime_error(std::string("postgres: a ") + v.kind_name() +
                                         " is not a value a column can hold -- " + sql);
            }
            pg_put_i32(bind, (int32_t)text.size());
            pg_put(bind, text);
        }
        pg_put_i16(bind, 1);
        pg_put_i16(bind, 0);  // every result in text format
        return bind;
    }

    /// Run one statement against many parameter sets, parsing once. One
    /// Parse, then a Bind/Describe/Execute per set, then a single Sync: the
    /// server plans once and the client round-trips once, which is where the
    /// batch time goes on this backend.
    DbResult run_many(const std::string& sql, const std::vector<std::vector<Val>>& sets) override {
        DbResult out;
        if (fd < 0) throw std::runtime_error("postgres: the connection is closed");
        if (sets.empty()) return out;
        if (aborted && depth > 0 && !is_rollback_sql(sql)) {
            throw std::runtime_error(
                "postgres: the transaction is already failed, so this statement cannot run; roll it back "
                "first (the statement that failed was: " + failed_stmt + ")");
        }
        std::string parse;
        pg_put_cstr(parse, "");
        pg_put_cstr(parse, sql);
        pg_put_i16(parse, 0);
        pg_send_msg(fd, 'P', parse);
        for (const auto& params : sets) {
            pg_send_msg(fd, 'B', build_bind(params, sql));
            std::string describe;
            pg_put_cstr(describe, "P");
            pg_send_msg(fd, 'D', describe);
            std::string execute;
            pg_put_cstr(execute, "");
            pg_put_i32(execute, 0);
            pg_send_msg(fd, 'E', execute);
        }
        pg_send_msg(fd, 'S', "");
        read_result(out, sql);
        return out;
    }

    void read_result(DbResult& out, const std::string& sql) {
        PgReader r;
        r.fd = fd;
        std::vector<uint32_t> oids;
        std::string first_error;
        char status = 'I';
        // Only an INSERT's key belongs in `last_id`. A SELECT's first column is
        // a value someone selected, not a key anybody assigned.
        bool inserting = false;
        for (;;) {
            std::string payload;
            char typ = r.message(payload);
            // ParseComplete, BindComplete, ParameterDescription, NoData,
            // PortalSuspended, EmptyQueryResponse: the bookkeeping messages
            // that say a step happened and carry nothing to decode.
            if (typ == '1' || typ == '2' || typ == 't' || typ == 'n' || typ == 's' || typ == 'I') continue;
            if (typ == 'T') {
                PgReader p;
                p.buf = payload;
                int16_t n = p.i16();
                // A fresh description per result group: batched executions
                // describe the same columns again, and appending would shift
                // every later row's decoding.
                oids.clear();
                out.columns.clear();
                for (int16_t i = 0; i < n; i++) {
                    out.columns.push_back(p.cstr());
                    p.take(6);      // table oid, column number
                    oids.push_back((uint32_t)p.i32());
                    p.i16();        // type size
                    p.i32();        // type modifier
                    p.i16();        // format code
                }
                continue;
            }
            if (typ == 'D') {
                PgReader p;
                p.buf = payload;
                int16_t n = p.i16();
                std::vector<Val> row;
                row.reserve((size_t)n);
                for (int16_t i = 0; i < n; i++) {
                    int32_t len = p.i32();
                    if (len < 0) {
                        row.push_back(Val::nil());
                        continue;
                    }
                    std::string raw = p.take((size_t)len);
                    row.push_back(pg_decode(i < (int)oids.size() ? oids[i] : (uint32_t)PG_TEXT, raw));
                }
                out.rows.push_back(std::move(row));
                continue;
            }
            if (typ == 'C') {
                // "INSERT 0 1", "UPDATE 3", "SELECT 2", "DELETE 0". The count is
                // the last word, and it is how a statement says how many rows
                // it touched.
                PgReader p;
                p.buf = payload;
                std::string tag = p.cstr();
                size_t sp = tag.rfind(' ');
                if (sp != std::string::npos) {
                    std::string n = tag.substr(sp + 1);
                    bool numeric = !n.empty();
                    for (char ch : n)
                        if (ch < '0' || ch > '9') numeric = false;
                    if (numeric) out.affected += strtoll(n.c_str(), nullptr, 10);
                }
                inserting = tag.compare(0, 6, "INSERT") == 0;
                continue;
            }
            if (typ == 'E') {
                if (first_error.empty()) first_error = pg_error_text(payload);
                continue;
            }
            if (typ == 'N') continue;  // a notice is not a failure
            if (typ == 'Z') {
                status = payload.empty() ? 'I' : payload[0];
                break;
            }
            throw std::runtime_error(std::string("postgres: unexpected message '") + typ + "' from a query");
        }
        if (!first_error.empty()) {
            // 'E' is the server saying the transaction is now failed, and it
            // will refuse everything until it is rolled back.
            aborted = (status == 'E');
            if (aborted) failed_stmt = sql;
            throw std::runtime_error("postgres: " + first_error + " -- while running: " + sql);
        }
        if (inserting && !out.rows.empty() && !out.columns.empty()) out.last_id = orm_row_id(out);
    }

    /// The first column of the first row, when a statement returned one. This
    /// is how a generated key comes back: PostgreSQL has no
    /// `last_insert_rowid()`, so the insert asks for the key with `RETURNING`
    /// and reads it from the row.
    static int64_t orm_row_id(const DbResult& r) {
        if (r.rows.empty() || r.rows[0].empty()) return 0;
        const Val& v = r.rows[0][0];
        if (v.is_int()) return v.iv;
        return 0;
    }

    void begin() override {
        // An inner `BEGIN` is not a warning to ignore, it is a different
        // transaction that never began: the outermost one owns the transaction,
        // and the outermost commit or rollback ends it. The same mapping
        // SQLite gets, for the same reason.
        if (depth == 0) {
            // A fresh top-level transaction cannot be failed: whatever aborted
            // the last one ended with it.
            aborted = false;
            run("BEGIN", {});
        }
        depth++;
    }

    void commit() override {
        if (depth == 0) return;
        depth--;
        if (depth == 0) {
            sps.clear();
            run("COMMIT", {});
            aborted = false;
        }
    }

    void rollback() override {
        if (depth == 0) return;
        depth = 0;
        sps.clear();
        run("ROLLBACK", {});
        aborted = false;
    }

    /// `ROLLBACK` and `ROLLBACK TO SAVEPOINT` are the two statements
    /// PostgreSQL accepts from a failed transaction, which is why they pass
    /// the refusal above: they are the way out, not another statement piling
    /// onto the failure.
    static bool is_rollback_sql(const std::string& sql) {
        size_t i = 0;
        while (i < sql.size() && isspace((unsigned char)sql[i])) i++;
        const char* want = "rollback";
        for (int k = 0; k < 8; k++) {
            if (i + (size_t)k >= sql.size()) return false;
            if (tolower((unsigned char)sql[i + (size_t)k]) != want[k]) return false;
        }
        return true;
    }

    std::string savepoint(const std::string& name) override {
        if (depth == 0) throw std::runtime_error("postgres: savepoint needs an open transaction");
        std::string sp = name;
        if (sp.empty()) sp = "hs_sp_" + std::to_string(++sp_next);
        if (!db_valid_savepoint_name(sp))
            throw std::runtime_error("postgres: savepoint name must use letters, digits and underscores");
        run("SAVEPOINT \"" + sp + "\"", {});
        sps.push_back(sp);
        return sp;
    }

    void release_savepoint(const std::string& name) override {
        bool found = false;
        while (!sps.empty()) {
            std::string top = sps.back();
            sps.pop_back();
            if (top == name) {
                found = true;
                break;
            }
        }
        if (!found) throw std::runtime_error("postgres: no savepoint \"" + name + "\" is open");
        run("RELEASE \"" + name + "\"", {});
    }

    void rollback_to(const std::string& name) override {
        bool found = false;
        for (const auto& s : sps)
            if (s == name) {
                found = true;
                break;
            }
        if (!found) throw std::runtime_error("postgres: no savepoint \"" + name + "\" is open");
        run("ROLLBACK TO \"" + name + "\"", {});
        // Rolling back to a savepoint un-aborts the transaction on the
        // server, so the flag goes with it. Anything after this runs again.
        aborted = false;
    }

    int tx_depth() const override { return depth; }

    /// The statement that left the transaction failed, so the next refusal can
    /// point back at the cause instead of at itself.
    std::string failed_stmt;
};

/// Open a connection. Throws with the server's own words if it will not.
inline PgDb* db_open_pgsql(const std::string& conninfo) { return new PgDb(conninfo); }

inline void db_close_pgsql(PgDb* db) { delete db; }

}  // namespace hs

#endif  // HS_RUNTIME_PGSQL_HPP
