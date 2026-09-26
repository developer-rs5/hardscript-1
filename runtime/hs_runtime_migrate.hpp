// hs_runtime_migrate.hpp -- migration files, history table, and the runner
//
// A migration is a versioned file with an up side and a down side that were
// generated together, so the two cannot drift apart. The database remembers
// which versions it applied and the checksum each file had when it did, so a
// file that changed afterwards is a conflict rather than a silent re-run.

#ifndef HS_RUNTIME_MIGRATE_HPP
#define HS_RUNTIME_MIGRATE_HPP

#include "hs_runtime_orm.hpp"

#include <algorithm>
#include <cctype>
#include <cstdint>
#include <cstdio>
#include <ctime>

namespace hs {

// ===========================================================================
// Files
// ===========================================================================

/// One migration file, parsed and ready to run.
struct Migration {
    /// `0001`, the digits before the first underscore of the file name. The
    /// name after them is prose: renaming `0001_init.sql` to `0001_users.sql`
    /// does not rewrite history, because history keys on the number.
    std::string version;
    /// `init`, the prose after the number, recorded for `status` to show.
    std::string name;
    /// The schema fingerprint the file was generated from, when the generator
    /// wrote one. Empty for hand-written files.
    std::string fingerprint;
    /// The up statements, in run order.
    std::vector<std::string> up;
    /// The down statements, in run order.
    std::vector<std::string> down;
    /// Checksum of the file as parsed, so a later edit is detectable.
    std::string checksum;
};

/// FNV-1a over bytes, as sixteen hex digits. It is not a cryptographic hash;
/// its job is to notice that a file changed, not to resist an adversary.
inline std::string migration_checksum(const std::string& text) {
    uint64_t h = 1469598103934665603ULL;
    for (unsigned char c : text) {
        h ^= (uint64_t)c;
        h *= 1099511628211ULL;
    }
    char b[17];
    snprintf(b, sizeof b, "%016llx", (unsigned long long)h);
    return b;
}

/// Undo the one-line escaping of a header value: `\\` first, then `\n`.
/// Anything else after a backslash is left alone, so a hand-written header
/// with a stray backslash survives as written rather than as an error.
inline std::string migration_unescape(const std::string& s) {
    std::string out;
    for (size_t i = 0; i < s.size();) {
        if (s[i] == '\\' && i + 1 < s.size() && (s[i + 1] == '\\' || s[i + 1] == 'n')) {
            out += s[i + 1] == 'n' ? '\n' : '\\';
            i += 2;
            continue;
        }
        out += s[i];
        i++;
    }
    return out;
}

inline std::string migration_trim(const std::string& s) {
    size_t a = 0, b = s.size();
    while (a < b && isspace((unsigned char)s[a])) a++;
    while (b > a && isspace((unsigned char)s[b - 1])) b--;
    return s.substr(a, b - a);
}

/// Split SQL text into statements on semicolons that are really terminators:
/// not one inside a string, a quoted identifier, a comment, or a dollar-quoted
/// body. The returned statements keep their text but not the semicolon.
inline std::vector<std::string> split_sql_statements(const std::string& sql, const std::string& what) {
    std::vector<std::string> out;
    std::string cur;
    bool in_str = false;      // '...'
    bool in_ident = false;    // "..."
    bool in_line = false;     // -- ...\n
    bool in_block = false;    // /* ... */
    std::string dollar;       // non-empty inside $tag$ ... $tag$
    int line = 1;
    auto fail = [&](const std::string& why) {
        throw std::runtime_error("migration: " + what + " ends with " + why + " (line " +
                                 std::to_string(line) + ")");
    };
    for (size_t i = 0; i < sql.size();) {
        char c = sql[i];
        char n = i + 1 < sql.size() ? sql[i + 1] : '\0';
        if (c == '\n') line++;
        if (!dollar.empty()) {
            if (sql.compare(i, dollar.size(), dollar) == 0) {
                cur += dollar;
                i += dollar.size();
                dollar.clear();
                continue;
            }
            cur += c;
            i++;
            continue;
        }
        if (in_line) {
            cur += c;
            i++;
            if (c == '\n') in_line = false;
            continue;
        }
        if (in_block) {
            cur += c;
            i++;
            if (c == '*' && n == '/') {
                cur += n;
                i++;
                in_block = false;
            }
            continue;
        }
        if (in_str) {
            cur += c;
            i++;
            if (c == '\'') {
                if (n == '\'') {
                    cur += n;
                    i++;
                } else {
                    in_str = false;
                }
            }
            continue;
        }
        if (in_ident) {
            cur += c;
            i++;
            if (c == '"') {
                if (n == '"') {
                    cur += n;
                    i++;
                } else {
                    in_ident = false;
                }
            }
            continue;
        }
        if (c == '-' && n == '-') {
            in_line = true;
            cur += c;
            cur += n;
            i += 2;
            continue;
        }
        if (c == '/' && n == '*') {
            in_block = true;
            cur += c;
            cur += n;
            i += 2;
            continue;
        }
        if (c == '\'') {
            in_str = true;
            cur += c;
            i++;
            continue;
        }
        if (c == '"') {
            in_ident = true;
            cur += c;
            i++;
            continue;
        }
        if (c == '$') {
            // A dollar-quoted body: $tag$ ... $tag$, where the tag is empty
            // ($$...$$) or an identifier. Anything else is just a dollar sign.
            size_t j = i + 1;
            while (j < sql.size() &&
                   (isalnum((unsigned char)sql[j]) || sql[j] == '_'))
                j++;
            if (j < sql.size() && sql[j] == '$') {
                dollar = sql.substr(i, j - i + 1);
                cur += dollar;
                i += dollar.size();
                continue;
            }
            cur += c;
            i++;
            continue;
        }
        if (c == ';') {
            std::string stmt = migration_trim(cur);
            // A statement that is only comments is not a statement.
            std::string code;
            {
                bool ls = false, bs = false;
                for (size_t k = 0; k < stmt.size();) {
                    if (ls) {
                        if (stmt[k] == '\n') ls = false;
                        k++;
                        continue;
                    }
                    if (bs) {
                        if (stmt[k] == '*' && k + 1 < stmt.size() && stmt[k + 1] == '/') {
                            bs = false;
                            k += 2;
                        } else {
                            k++;
                        }
                        continue;
                    }
                    if (stmt[k] == '-' && k + 1 < stmt.size() && stmt[k + 1] == '-') {
                        ls = true;
                        k += 2;
                        continue;
                    }
                    if (stmt[k] == '/' && k + 1 < stmt.size() && stmt[k + 1] == '*') {
                        bs = true;
                        k += 2;
                        continue;
                    }
                    code += stmt[k];
                    k++;
                }
            }
            if (!migration_trim(code).empty()) out.push_back(stmt);
            cur.clear();
            i++;
            continue;
        }
        cur += c;
        i++;
    }
    if (in_str) fail("an unterminated string");
    if (in_ident) fail("an unterminated quoted identifier");
    if (in_block) fail("an unterminated block comment");
    if (!dollar.empty()) fail("an unterminated dollar-quoted body");
    std::string rest = migration_trim(cur);
    if (!rest.empty()) out.push_back(rest);
    return out;
}

/// Parse one migration file. The version comes from the file name
/// (`0001_init.sql`), and the two sides from `-- +migrate Up` and
/// `-- +migrate Down` markers, so a file with the SQL in the wrong half is a
/// parse error rather than a migration that undoes itself.
inline Migration parse_migration_file(const std::string& filename, const std::string& text) {
    std::string base = filename;
    size_t slash = base.find_last_of("/\\");
    if (slash != std::string::npos) base = base.substr(slash + 1);
    size_t dot = base.find_last_of('.');
    std::string stem = dot == std::string::npos ? base : base.substr(0, dot);
    auto bad_name = [&]() {
        throw std::runtime_error("migration: file name must look like 0001_init.sql, got " + filename);
    };
    // Four digits, so versions sort as strings in the order they were written.
    // `1_init` would sort after `0001` and before `0002` only by accident.
    size_t us = stem.find('_');
    if (us != 4) bad_name();
    for (size_t i = 0; i < 4; i++)
        if (!isdigit((unsigned char)stem[i])) bad_name();
    for (size_t i = 5; i < stem.size(); i++)
        if (!isalnum((unsigned char)stem[i]) && stem[i] != '_' && stem[i] != '-' && stem[i] != '.')
            bad_name();
    std::string version = stem.substr(0, 4);
    std::string name = stem.substr(5);

    std::string norm;
    norm.reserve(text.size());
    for (size_t i = 0; i < text.size(); i++) {
        if (text[i] == '\r' && i + 1 < text.size() && text[i + 1] == '\n') continue;
        norm += text[i];
    }
    std::vector<std::string> lines;
    {
        size_t i = 0;
        while (i <= norm.size()) {
            size_t e = norm.find('\n', i);
            if (e == std::string::npos) {
                lines.push_back(norm.substr(i));
                break;
            }
            lines.push_back(norm.substr(i, e - i));
            i = e + 1;
        }
    }
    int up_at = -1, down_at = -1;
    std::string fingerprint;
    const std::string fp_mark = "-- hardscript:fingerprint ";
    const std::string id_mark = "-- hardscript:migration ";
    for (size_t i = 0; i < lines.size(); i++) {
        std::string t = migration_trim(lines[i]);
        if (t == "-- +migrate Up") {
            if (up_at >= 0)
                throw std::runtime_error("migration: " + filename + " marks its up side twice");
            up_at = (int)i;
        } else if (t == "-- +migrate Down") {
            if (down_at >= 0)
                throw std::runtime_error("migration: " + filename + " marks its down side twice");
            down_at = (int)i;
        } else if (t.compare(0, fp_mark.size(), fp_mark) == 0 && up_at < 0) {
            fingerprint = migration_trim(t.substr(fp_mark.size()));
        } else if (t.compare(0, id_mark.size(), id_mark) == 0 && up_at < 0) {
            std::string named = migration_trim(t.substr(id_mark.size()));
            if (!named.empty() && named != version && named != stem)
                throw std::runtime_error("migration: " + filename + " says it is " + named);
        }
    }
    if (up_at < 0 || down_at < 0 || down_at < up_at)
        throw std::runtime_error("migration: " + filename +
                                 " needs a `-- +migrate Up` section followed by a `-- +migrate Down` section");
    std::string up_text, down_text;
    for (int i = up_at + 1; i < down_at; i++) {
        up_text += lines[(size_t)i];
        up_text += '\n';
    }
    for (size_t i = (size_t)down_at + 1; i < lines.size(); i++) {
        down_text += lines[i];
        down_text += '\n';
    }
    Migration m;
    m.version = version;
    m.name = name;
    // The header is one line, so a fingerprint written there arrives escaped;
    // what history stores is the fingerprint itself, newlines and all.
    m.fingerprint = migration_unescape(fingerprint);
    m.up = split_sql_statements(up_text, filename + " (up)");
    m.down = split_sql_statements(down_text, filename + " (down)");
    if (m.up.empty())
        throw std::runtime_error("migration: " + filename + " has an empty up side, which migrates nothing");
    // The checksum covers the two sides, not the comments around them: adding
    // a comment to a file must not rewrite history, but changing a statement
    // after it ran must be a conflict.
    std::string body;
    for (const auto& s : m.up) {
        body += s;
        body += ";\n";
    }
    body += "-- +migrate Down\n";
    for (const auto& s : m.down) {
        body += s;
        body += ";\n";
    }
    m.checksum = migration_checksum(body);
    return m;
}

// ===========================================================================
// History
// ===========================================================================

/// What the database remembers about applied migrations.
struct AppliedMigration {
    std::string version;
    std::string name;
    int64_t applied_at = 0;
    std::string checksum;
    std::string fingerprint;
};

/// Where the database stands against the files on disk.
struct MigrationStatus {
    std::vector<AppliedMigration> applied;
    /// Files newer than everything applied, in version order.
    std::vector<Migration> pending;
    /// Versions the database applied that no file on disk has anymore.
    std::vector<std::string> missing;
    /// Non-empty when up or down must refuse: the conflict, in plain words.
    std::string conflict;
};

inline std::string migration_placeholders(DbDialect d, int n) {
    std::string out;
    for (int i = 1; i <= n; i++) {
        if (i > 1) out += ", ";
        out += db_placeholder(d, i);
    }
    return out;
}

/// Create the history table when it is not there. It carries one row per
/// applied version, so applying the same file twice is impossible by key.
inline void ensure_migrations_table(DbBackend* b) {
    DbDialect d = b->dialect();
    std::string sql = "CREATE TABLE IF NOT EXISTS ";
    sql += db_quote_ident("schema_migrations");
    sql += " (";
    sql += db_quote_ident("version") + " TEXT PRIMARY KEY, ";
    sql += db_quote_ident("name") + " TEXT NOT NULL, ";
    sql += db_quote_ident("applied_at") + " INTEGER NOT NULL, ";
    sql += db_quote_ident("checksum") + " TEXT NOT NULL, ";
    sql += db_quote_ident("fingerprint") + " TEXT NOT NULL DEFAULT ''";
    sql += ")";
    (void)d;
    b->run(sql, {});
}

/// Parse, sort, and check a set of migration files against the history table.
/// Files are `{filename, text}` pairs; sorting is by version, and two files
/// with one version are a conflict no matter what they contain.
inline MigrationStatus migration_status(DbBackend* b, const std::vector<std::pair<std::string, std::string>>& files) {
    MigrationStatus st;
    std::vector<Migration> migrations;
    migrations.reserve(files.size());
    for (const auto& f : files) migrations.push_back(parse_migration_file(f.first, f.second));
    std::sort(migrations.begin(), migrations.end(),
              [](const Migration& a, const Migration& c) { return a.version < c.version; });
    for (size_t i = 1; i < migrations.size(); i++) {
        if (migrations[i].version == migrations[i - 1].version) {
            st.conflict = "migration conflict: two files share version " + migrations[i].version +
                          "; rename one so history has one row per version";
            return st;
        }
    }

    ensure_migrations_table(b);
    DbResult r = b->run(
        std::string("SELECT ") + db_quote_ident("version") + ", " + db_quote_ident("name") + ", " +
            db_quote_ident("applied_at") + ", " + db_quote_ident("checksum") + ", " +
            db_quote_ident("fingerprint") + " FROM " + db_quote_ident("schema_migrations") +
            " ORDER BY " + db_quote_ident("version"),
        {});
    for (const auto& row : r.rows) {
        AppliedMigration a;
        if (row.size() > 0 && row[0].is_str()) a.version = row[0].sv;
        if (row.size() > 1 && row[1].is_str()) a.name = row[1].sv;
        if (row.size() > 2 && row[2].is_int()) a.applied_at = row[2].iv;
        if (row.size() > 3 && row[3].is_str()) a.checksum = row[3].sv;
        if (row.size() > 4 && row[4].is_str()) a.fingerprint = row[4].sv;
        st.applied.push_back(a);
    }

    for (const auto& a : st.applied) {
        const Migration* file = nullptr;
        for (const auto& m : migrations)
            if (m.version == a.version) {
                file = &m;
                break;
            }
        if (!file) {
            st.missing.push_back(a.version);
            continue;
        }
        if (!a.checksum.empty() && file->checksum != a.checksum) {
            st.conflict = "migration conflict: " + a.version +
                          " changed since it was applied; history records the file as it was, "
                          "and applying the edited file would rewrite what the database already did";
            return st;
        }
    }
    if (!st.missing.empty()) {
        st.conflict = "migration conflict: the database applied " + st.missing[0] +
                      " but no file on disk has that version; restore the file or reconcile history first";
        return st;
    }
    // A file older than the newest applied version that was never applied is
    // not pending, it is out of order: applying it now would run history
    // backwards.
    std::string newest;
    for (const auto& a : st.applied)
        if (a.version > newest) newest = a.version;
    for (const auto& m : migrations) {
        bool done = false;
        for (const auto& a : st.applied)
            if (a.version == m.version) {
                done = true;
                break;
            }
        if (done) continue;
        if (!newest.empty() && m.version < newest) {
            st.conflict = "migration conflict: " + m.version + " is older than applied " + newest +
                          " but was never applied; move it after history or fold it into a new migration";
            return st;
        }
        st.pending.push_back(m);
    }
    return st;
}

/// Apply every pending migration, oldest first, each in its own transaction
/// with its history row. A failure rolls back that migration's statements and
/// names the version and the statement that failed.
inline std::vector<std::string> migrate_up(
    DbBackend* b, const std::vector<std::pair<std::string, std::string>>& files) {
    MigrationStatus st = migration_status(b, files);
    if (!st.conflict.empty()) throw std::runtime_error(st.conflict);
    std::vector<std::string> done;
    for (const auto& m : st.pending) {
        b->begin();
        try {
            int n = 0;
            for (const auto& sql : m.up) {
                n++;
                try {
                    b->run(sql, {});
                } catch (const std::exception& e) {
                    throw std::runtime_error("migration " + m.version + " statement " + std::to_string(n) +
                                             " failed: " + e.what());
                }
            }
            std::string sql = std::string("INSERT INTO ") + db_quote_ident("schema_migrations") + " (" +
                              db_quote_ident("version") + ", " + db_quote_ident("name") + ", " +
                              db_quote_ident("applied_at") + ", " + db_quote_ident("checksum") + ", " +
                              db_quote_ident("fingerprint") + ") VALUES (" +
                              migration_placeholders(b->dialect(), 5) + ")";
            b->run(sql, {Val::text(m.version), Val::text(m.name),
                         Val::int_((int64_t)std::time(nullptr)), Val::text(m.checksum),
                         Val::text(m.fingerprint)});
            b->commit();
        } catch (...) {
            b->rollback();
            throw;
        }
        done.push_back(m.version);
    }
    return done;
}

/// Roll back the newest `steps` applied migrations. Each down side runs in its
/// own transaction with its history row removed, so a failed rollback leaves
/// history exactly as it was.
inline std::vector<std::string> migrate_down(
    DbBackend* b, const std::vector<std::pair<std::string, std::string>>& files, int steps) {
    if (steps <= 0) throw std::runtime_error("migration: nothing to roll back");
    MigrationStatus st = migration_status(b, files);
    if (!st.conflict.empty()) throw std::runtime_error(st.conflict);
    if ((int)st.applied.size() < steps)
        throw std::runtime_error("migration: asked to roll back " + std::to_string(steps) +
                                 " but only " + std::to_string(st.applied.size()) + " applied");
    std::vector<std::string> undone;
    std::vector<Migration> migrations;
    migrations.reserve(files.size());
    for (const auto& f : files) migrations.push_back(parse_migration_file(f.first, f.second));
    for (int i = 0; i < steps; i++) {
        const AppliedMigration& a = st.applied[st.applied.size() - 1 - (size_t)i];
        const Migration* file = nullptr;
        for (const auto& m : migrations)
            if (m.version == a.version) {
                file = &m;
                break;
            }
        if (!file)
            throw std::runtime_error("migration conflict: cannot roll back " + a.version +
                                     " without its file");
        b->begin();
        try {
            int n = 0;
            for (const auto& sql : file->down) {
                n++;
                try {
                    b->run(sql, {});
                } catch (const std::exception& e) {
                    throw std::runtime_error("migration " + file->version + " rollback statement " +
                                             std::to_string(n) + " failed: " + e.what());
                }
            }
            std::string sql = std::string("DELETE FROM ") + db_quote_ident("schema_migrations") +
                              " WHERE " + db_quote_ident("version") + " = " +
                              db_placeholder(b->dialect(), 1);
            b->run(sql, {Val::text(file->version)});
            b->commit();
        } catch (...) {
            b->rollback();
            throw;
        }
        undone.push_back(file->version);
    }
    return undone;
}

/// Run a seed file: every statement in one transaction, so a seed either lands
/// whole or not at all. Seeds are data, not history, and are not recorded.
inline int run_seed(DbBackend* b, const std::string& filename, const std::string& text) {
    std::vector<std::string> stmts;
    try {
        stmts = split_sql_statements(text, filename);
    } catch (const std::exception& e) {
        throw std::runtime_error(std::string("seed failed: ") + filename + ": " + e.what());
    }
    b->begin();
    try {
        int n = 0;
        for (const auto& sql : stmts) {
            n++;
            try {
                b->run(sql, {});
            } catch (const std::exception& e) {
                throw std::runtime_error("seed failed: " + filename + " statement " + std::to_string(n) +
                                         " failed: " + e.what());
            }
        }
        b->commit();
    } catch (...) {
        b->rollback();
        throw;
    }
    return (int)stmts.size();
}

}  // namespace hs

#endif  // HS_RUNTIME_MIGRATE_HPP
