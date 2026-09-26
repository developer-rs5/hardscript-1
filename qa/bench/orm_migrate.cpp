// orm_migrate.cpp -- migration and seed benchmarks against SQLite
//
// Parsing is pure CPU; applying is DDL plus history writes. Both scale with
// the migration, so the bench builds 1-, 5- and 20-table migrations with the
// same shape the compiler generates (tables, indexes, a foreign key) and
// times parse, apply, rollback, status and seeding separately.
#include "hs_runtime_migrate.hpp"
#include "hs_runtime_sqlite.hpp"

#include "bench.hpp"

namespace {

// One table the way the compiler writes it: key, two columns, one index.
std::string table_ddl(int t) {
    std::string n = "t" + std::to_string(t);
    std::string out = "CREATE TABLE \"" + n + "\" (\n  \"id\" INTEGER PRIMARY KEY AUTOINCREMENT,\n  \"name\" "
                      "TEXT NOT NULL,\n  \"ref\" INTEGER REFERENCES \"t0\"(\"id\")\n);\n";
    out += "CREATE INDEX \"idx_" + n + "\" ON \"" + n + "\" (\"name\");\n";
    return out;
}

std::pair<std::string, std::string> migration_file(int tables, int version) {
    char name[32];
    snprintf(name, sizeof name, "%04d_tables.sql", version);
    std::string text = "-- +migrate Up\n";
    for (int t = 0; t < tables; t++) text += table_ddl(t);
    text += "-- +migrate Down\n";
    for (int t = tables - 1; t >= 0; t--) {
        std::string n = "t" + std::to_string(t);
        text += "DROP INDEX IF EXISTS \"idx_" + n + "\";\nDROP TABLE IF EXISTS \"" + n + "\";\n";
    }
    return {name, text};
}

std::string seed_text(int rows) {
    // One parent first: `ref` has a foreign key, and a seed that violates its
    // own schema is not a benchmark but a bug report.
    std::string out = "INSERT INTO \"t0\" (\"name\", \"ref\") VALUES ('parent', NULL);\n";
    for (int i = 0; i < rows; i++) {
        out += "INSERT INTO \"t0\" (\"name\", \"ref\") VALUES ('seed" + std::to_string(i) + "', 1);\n";
    }
    return out;
}

}  // namespace

int main() {
    const char* BE = "sqlite";
    {
        auto f = migration_file(3, 1);
        bench::Timer t("migrate_parse", BE);
        for (int i = 0; i < 50; i++) {
            (void)hs::parse_migration_file(f.first, f.second);
            t.add(1);
        }
        t.done();
    }
    for (int tables : {1, 5, 20}) {
        auto f = migration_file(tables, 1);
        std::vector<std::pair<std::string, std::string>> files = {f};
        char name[32];
        snprintf(name, sizeof name, "migrate_up_%dtbl", tables);
        if (tables == 1) {
            // Warm up once: the first open loads libsqlite3, and that cost
            // belongs to process startup, not to applying a migration.
            hs::SqliteDb warm(":memory:");
            (void)hs::migrate_up(&warm, files);
        }
        bench::Timer t(name, BE);
        for (int i = 0; i < 10; i++) {
            hs::SqliteDb db(":memory:");
            (void)hs::migrate_up(&db, files);
            t.add(1);
        }
        t.done();
    }
    {
        auto f = migration_file(5, 1);
        std::vector<std::pair<std::string, std::string>> files = {f};
        bench::Timer t("migrate_status", BE);
        hs::SqliteDb db(":memory:");
        (void)hs::migrate_up(&db, files);
        for (int i = 0; i < 50; i++) {
            (void)hs::migration_status(&db, files);
            t.add(1);
        }
        t.done();
    }
    {
        auto f = migration_file(5, 1);
        std::vector<std::pair<std::string, std::string>> files = {f};
        bench::Timer t("migrate_down", BE);
        for (int i = 0; i < 10; i++) {
            hs::SqliteDb db(":memory:");
            (void)hs::migrate_up(&db, files);
            (void)hs::migrate_down(&db, files, 1);
            t.add(1);
        }
        t.done();
    }
    {
        auto f = migration_file(1, 1);
        std::vector<std::pair<std::string, std::string>> files = {f};
        hs::SqliteDb db(":memory:");
        (void)hs::migrate_up(&db, files);
        bench::Timer t("seed_1000", BE);
        int n = hs::run_seed(&db, "seed.sql", seed_text(1000));
        t.add(n);
        t.done();
    }
    bench::metrics(BE);
    return 0;
}
