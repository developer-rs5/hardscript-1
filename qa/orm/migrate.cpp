// qa/orm/migrate.cpp -- migrations and seeds against a real database
//
// The migration runner is storage code wearing a versioning hat: files become
// statements, statements run in transactions, and history remembers what ran.
// Every test below runs against SQLite on a socket of its own (an in-memory
// database), because a fake backend cannot say whether the statements are
// valid SQL.

#include "hs_runtime_migrate.hpp"
#include "hs_runtime_sqlite.hpp"

#include "test_support.hpp"

namespace qa {
namespace {

using Files = std::vector<std::pair<std::string, std::string>>;

const char* CREATE_USERS_UP = R"SQL(-- +migrate Up
CREATE TABLE "user" (
  "id" INTEGER PRIMARY KEY AUTOINCREMENT,
  "name" TEXT NOT NULL
);
CREATE INDEX "user_name" ON "user" ("name");
-- +migrate Down
DROP INDEX IF EXISTS "user_name";
DROP TABLE IF EXISTS "user";
)SQL";

Files one_migration() { return {{"0001_init.sql", CREATE_USERS_UP}}; }

int64_t table_count(hs::SqliteDb& db, const std::string& table) {
    auto r = db.run("SELECT COUNT(*) FROM \"" + table + "\"", {});
    return r.rows[0][0].iv;
}

bool table_exists(hs::SqliteDb& db, const std::string& table) {
    auto r = db.run("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?", {hs::Val::text(table)});
    return r.rows[0][0].iv > 0;
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

void a_migration_file_parses_into_two_sides() {
    hs::Migration m = hs::parse_migration_file("0001_init.sql", CREATE_USERS_UP);
    CHECK_EQ(m.version, std::string("0001"), "the version is the number");
    CHECK_EQ(m.name, std::string("init"), "and the name is the prose after it");
    CHECK_EQ(m.up.size(), size_t(2), "two statements up");
    CHECK_EQ(m.down.size(), size_t(2), "two down");
    CHECK(m.up[0].find("CREATE TABLE") != std::string::npos, "in order");
    CHECK(!m.checksum.empty(), "and it has a checksum");
}

void a_file_name_has_to_look_like_a_version() {
    for (const char* bad : {"init.sql", "1_init.sql", "00001_init.sql", "0001.sql", "_init.sql", ".sql", "0001 init.sql"}) {
        bool threw = false;
        try {
            hs::parse_migration_file(bad, CREATE_USERS_UP);
        } catch (const std::exception& e) {
            threw = true;
            CHECK(std::string(e.what()).find("0001_init.sql") != std::string::npos,
                  "and the error shows the shape a name must have");
        }
        CHECK(threw, std::string("rejected: ") + bad);
    }
}

void the_two_sides_must_both_be_there_exactly_once() {
    bool threw = false;
    try {
        hs::parse_migration_file("0001_x.sql", "-- +migrate Up\nSELECT 1;\n");
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("Down") != std::string::npos;
    }
    CHECK(threw, "a missing down side says which half is missing");
    threw = false;
    try {
        hs::parse_migration_file("0001_x.sql", "-- +migrate Down\nSELECT 1;\n");
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "an up side that never opens is not a migration");
    threw = false;
    try {
        hs::parse_migration_file("0001_x.sql", "-- +migrate Up\nSELECT 1;\n-- +migrate Up\nSELECT 2;\n-- +migrate Down\nSELECT 3;\n");
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "and neither is a file with two up sides");
}

void a_header_may_name_the_version_and_the_schema() {
    std::string text = "-- hardscript:migration 0001_init\n-- hardscript:fingerprint abc123\n";
    text += CREATE_USERS_UP;
    hs::Migration m = hs::parse_migration_file("0001_init.sql", text);
    CHECK_EQ(m.fingerprint, std::string("abc123"), "the fingerprint is carried, not executed");
    bool threw = false;
    try {
        hs::parse_migration_file("0001_init.sql", "-- hardscript:migration 0002_other\n" + std::string(CREATE_USERS_UP));
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("0002_other") != std::string::npos;
    }
    CHECK(threw, "but a header naming another version is a lie worth refusing");
}

void a_semicolon_in_a_string_is_not_a_terminator() {
    std::string text = "-- +migrate Up\n";
    text += "INSERT INTO \"user\" (\"name\") VALUES ('a;b'); -- a comment; with one\n";
    text += "/* block; comment */\nINSERT INTO \"user\" (\"name\") VALUES ('c');\n";
    text += "-- +migrate Down\nDELETE FROM \"user\";\n";
    hs::Migration m = hs::parse_migration_file("0001_x.sql", text);
    CHECK_EQ(m.up.size(), size_t(2), "two inserts, not four fragments");
    CHECK(m.up[0].find("'a;b'") != std::string::npos, "the string survived whole");
}

void an_unterminated_string_is_a_parse_error_not_a_hang() {
    bool threw = false;
    try {
        hs::parse_migration_file("0001_x.sql", "-- +migrate Up\nINSERT INTO t VALUES ('oops);\n-- +migrate Down\nSELECT 1;\n");
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("unterminated string") != std::string::npos;
    }
    CHECK(threw, "and it says what never ended");
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

void up_applies_pending_in_order_and_records_them() {
    hs::SqliteDb db(":memory:");
    auto done = hs::migrate_up(&db, one_migration());
    CHECK_EQ(done.size(), size_t(1), "one applied");
    CHECK_EQ(done[0], std::string("0001"), "this one");
    CHECK(table_exists(db, "user"), "the table is there");
    CHECK(table_exists(db, "schema_migrations"), "and so is history");
    auto st = hs::migration_status(&db, one_migration());
    CHECK_EQ(st.applied.size(), size_t(1), "recorded");
    CHECK(st.pending.empty(), "nothing left pending");
    CHECK(st.conflict.empty(), "and no conflict");
    // Applying again is a no-op, not a duplicate key error.
    CHECK(hs::migrate_up(&db, one_migration()).empty(), "running up twice applies nothing twice");
}

void down_undoes_the_newest_first() {
    hs::SqliteDb db(":memory:");
    Files files = {{"0001_init.sql", CREATE_USERS_UP},
                   {"0002_names.sql",
                    "-- +migrate Up\nALTER TABLE \"user\" ADD COLUMN \"nick\" TEXT NULL;\n"
                    "-- +migrate Down\nALTER TABLE \"user\" DROP COLUMN \"nick\";\n"}};
    hs::migrate_up(&db, files);
    auto undone = hs::migrate_down(&db, files, 1);
    CHECK_EQ(undone.size(), size_t(1), "one rollback");
    CHECK_EQ(undone[0], std::string("0002"), "the newest goes first");
    auto st = hs::migration_status(&db, files);
    CHECK_EQ(st.applied.size(), size_t(1), "history lost exactly one row");
    CHECK_EQ(st.pending.size(), size_t(1), "and the file is pending again");
    hs::migrate_down(&db, files, 1);
    CHECK(!table_exists(db, "user"), "the first migration's down drops its table");
    bool threw = false;
    try {
        hs::migrate_down(&db, files, 1);
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "rolling back nothing is an error, not a silence");
}

void a_failed_migration_leaves_no_half_applied_state() {
    hs::SqliteDb db(":memory:");
    Files files = {{"0001_broken.sql",
                    "-- +migrate Up\nCREATE TABLE \"user\" (\"id\" INTEGER PRIMARY KEY);\n"
                    "INSERT INTO \"missing\" VALUES (1);\n-- +migrate Down\nDROP TABLE \"user\";\n"}};
    bool threw = false;
    try {
        hs::migrate_up(&db, files);
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("migration 0001") != std::string::npos &&
                std::string(e.what()).find("statement 2") != std::string::npos;
    }
    CHECK(threw, "the error names the version and the statement");
    CHECK(!table_exists(db, "user"), "the first statement rolled back with the second");
    auto st = hs::migration_status(&db, files);
    CHECK(st.applied.empty(), "and history recorded nothing");
    CHECK_EQ(st.pending.size(), size_t(1), "so the migration is still pending");
}

void an_edited_file_is_a_conflict_not_a_re_run() {
    hs::SqliteDb db(":memory:");
    hs::migrate_up(&db, one_migration());
    // A comment is not a change: history checksums the statements, not the
    // prose around them.
    std::string commented = std::string("-- a note to the future\n") + CREATE_USERS_UP;
    auto st = hs::migration_status(&db, {{"0001_init.sql", commented}});
    CHECK(st.conflict.empty(), "comments do not rewrite history");
    // A statement is.
    std::string edited = CREATE_USERS_UP;
    size_t at = edited.find("TEXT NOT NULL");
    edited.replace(at, 12, "TEXT NULL");
    st = hs::migration_status(&db, {{"0001_init.sql", edited}});
    CHECK(!st.conflict.empty(), "but an edited statement is");
    CHECK(st.conflict.find("changed since it was applied") != std::string::npos, "and it says so plainly");
    bool threw = false;
    try {
        hs::migrate_up(&db, {{"0001_init.sql", edited}});
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("conflict") != std::string::npos;
    }
    CHECK(threw, "and up refuses while the conflict stands");
}

void a_missing_file_is_a_conflict_too() {
    hs::SqliteDb db(":memory:");
    hs::migrate_up(&db, one_migration());
    auto st = hs::migration_status(&db, {});
    CHECK_EQ(st.missing.size(), size_t(1), "one version with no file");
    CHECK_EQ(st.missing[0], std::string("0001"), "the database remembers what the disk forgot");
    CHECK(!st.conflict.empty(), "and status says so");
}

void an_out_of_order_file_does_not_run_history_backwards() {
    hs::SqliteDb db(":memory:");
    Files files = {{"0002_second.sql",
                    "-- +migrate Up\nCREATE TABLE \"b\" (\"id\" INTEGER PRIMARY KEY);\n-- +migrate Down\nDROP TABLE \"b\";\n"}};
    hs::migrate_up(&db, files);
    files.push_back({"0001_first.sql",
                     "-- +migrate Up\nCREATE TABLE \"a\" (\"id\" INTEGER PRIMARY KEY);\n-- +migrate Down\nDROP TABLE \"a\";\n"});
    auto st = hs::migration_status(&db, files);
    CHECK(!st.conflict.empty(), "a file older than history but never applied is refused");
    CHECK(st.conflict.find("older than applied") != std::string::npos, "with the reason");
}

void two_files_cannot_share_a_version() {
    hs::SqliteDb db(":memory:");
    Files files = {{"0001_a.sql", CREATE_USERS_UP}, {"0001_b.sql", CREATE_USERS_UP}};
    auto st = hs::migration_status(&db, files);
    CHECK(!st.conflict.empty(), "one version is one migration");
}

void the_fingerprint_travels_into_history() {
    hs::SqliteDb db(":memory:");
    std::string text = "-- hardscript:fingerprint fp-42\n";
    text += CREATE_USERS_UP;
    hs::migrate_up(&db, {{"0001_init.sql", text}});
    auto r = db.run("SELECT \"fingerprint\" FROM \"schema_migrations\"", {});
    CHECK_EQ(r.rows[0][0].sv, std::string("fp-42"), "so status can compare the database against the models");
}

// ---------------------------------------------------------------------------
// Seeds
// ---------------------------------------------------------------------------

void a_seed_lands_whole() {
    hs::SqliteDb db(":memory:");
    hs::migrate_up(&db, one_migration());
    int n = hs::run_seed(&db, "seed.sql",
                         "INSERT INTO \"user\" (\"name\") VALUES ('ann');\n"
                         "INSERT INTO \"user\" (\"name\") VALUES ('bo');\n");
    CHECK_EQ(n, 2, "two statements ran");
    CHECK_EQ(table_count(db, "user"), int64_t(2), "and both rows are there");
}

void a_seed_fails_whole() {
    hs::SqliteDb db(":memory:");
    hs::migrate_up(&db, one_migration());
    bool threw = false;
    try {
        hs::run_seed(&db, "seed.sql",
                     "INSERT INTO \"user\" (\"name\") VALUES ('ann');\n"
                     "INSERT INTO \"missing\" VALUES (1);\n");
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("seed failed") != std::string::npos &&
                std::string(e.what()).find("statement 2") != std::string::npos;
    }
    CHECK(threw, "the error names the file and the statement");
    CHECK_EQ(table_count(db, "user"), int64_t(0), "and the first insert rolled back with the second");
}

}  // namespace
}  // namespace qa

int main() {
    qa::a_migration_file_parses_into_two_sides();
    qa::a_file_name_has_to_look_like_a_version();
    qa::the_two_sides_must_both_be_there_exactly_once();
    qa::a_header_may_name_the_version_and_the_schema();
    qa::a_semicolon_in_a_string_is_not_a_terminator();
    qa::an_unterminated_string_is_a_parse_error_not_a_hang();

    qa::up_applies_pending_in_order_and_records_them();
    qa::down_undoes_the_newest_first();
    qa::a_failed_migration_leaves_no_half_applied_state();
    qa::an_edited_file_is_a_conflict_not_a_re_run();
    qa::a_missing_file_is_a_conflict_too();
    qa::an_out_of_order_file_does_not_run_history_backwards();
    qa::two_files_cannot_share_a_version();
    qa::the_fingerprint_travels_into_history();

    qa::a_seed_lands_whole();
    qa::a_seed_fails_whole();

    return qa::report("migrate");
}
