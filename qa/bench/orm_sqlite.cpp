// orm_sqlite.cpp -- operation benchmarks against SQLite
//
// One workload per RESULT line, each timed around the ORM call and nothing
// else: setup (DDL, seeding) happens outside the timer it feeds. The database
// is in-memory, so the numbers are CPU and library, not disk; the report
// says so.
#include "hs_runtime_sqlite.hpp"

#include "bench.hpp"

namespace {

const hs::OrmModel& user_model() {
    static hs::OrmModel m("User", "user", "id", {"id", "name", "age"},
                          {hs::OrmKind::Int, hs::OrmKind::Text, hs::OrmKind::Int}, {"id"}, "");
    return m;
}

const hs::OrmModel& post_model() {
    static hs::OrmModel m("Post", "post", "id", {"id", "title", "user_id"},
                          {hs::OrmKind::Int, hs::OrmKind::Text, hs::OrmKind::Int}, {"id"}, "");
    return m;
}

const char* USER_DDL = "CREATE TABLE \"user\" (\"id\" INTEGER PRIMARY KEY AUTOINCREMENT,"
                       "\"name\" TEXT NOT NULL,\"age\" INTEGER NOT NULL)";
const char* POST_DDL = "CREATE TABLE \"post\" (\"id\" INTEGER PRIMARY KEY AUTOINCREMENT,"
                       "\"title\" TEXT NOT NULL,\"user_id\" INTEGER NOT NULL)";

hs::Val mkuser(int i) {
    return hs::Val::object(
        {{"name", hs::Val::text("user" + std::to_string(i))}, {"age", hs::Val::int_(20 + i % 50)}});
}

}  // namespace

int main() {
    const char* BE = "sqlite";
    {
        bench::Timer t("startup", BE);
        for (int i = 0; i < 20; i++) {
            hs::SqliteDb db(":memory:");
            db.run(USER_DDL, {});
            t.add(1);
        }
        t.done();
    }
    // One working database for the single-row lanes: 2000 users, 2000 posts.
    hs::SqliteDb db(":memory:");
    db.run(USER_DDL, {});
    db.run(POST_DDL, {});
    {
        std::vector<hs::Val> users, posts;
        for (int i = 0; i < 2000; i++) {
            users.push_back(mkuser(i));
            posts.push_back(hs::Val::object({{"title", hs::Val::text("post" + std::to_string(i))},
                                             {"user_id", hs::Val::int_(1 + i % 2000)}}));
        }
        (void)hs::orm_create_many(&user_model(), &db, hs::Val::list(users));
        (void)hs::orm_create_many(&post_model(), &db, hs::Val::list(posts));
    }
    {
        hs::SqliteDb fresh(":memory:");
        fresh.run(USER_DDL, {});
        bench::Timer t("create", BE);
        for (int i = 0; i < 2000; i++) {
            (void)hs::orm_create(&user_model(), &fresh, mkuser(i));
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("update", BE);
        for (int i = 1; i <= 2000; i++) {
            hs::Val rec = hs::Val::object(
                {{"id", hs::Val::int_(i)}, {"age", hs::Val::int_(99)}});
            (void)hs::orm_save(&user_model(), &db, rec);
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("find", BE);
        for (int i = 1; i <= 2000; i++) {
            (void)hs::OrmQuery(&user_model()).where_eq("id", hs::Val::int_(i)).first(&db);
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("find_many", BE);
        for (int base = 1; base <= 2000; base += 200) {
            std::vector<hs::Val> ids;
            for (int i = 0; i < 200; i++) ids.push_back(hs::Val::int_(base + i));
            (void)hs::orm_find_many(&user_model(), &db, hs::Val::list(ids));
            t.add(200);
        }
        t.done();
    }
    {
        bench::Timer t("join_has_many", BE);
        for (int i = 1; i <= 500; i++) {
            hs::Val u = hs::Val::object({{"id", hs::Val::int_(i)}});
            (void)hs::OrmQuery(&post_model()).of_record(u, "id", "user_id").all(&db);
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("join_belongs_to", BE);
        for (int i = 1; i <= 500; i++) {
            hs::Val p = hs::Val::object(
                {{"id", hs::Val::int_(i)}, {"user_id", hs::Val::int_(1 + i % 2000)}});
            (void)hs::OrmQuery(&user_model()).of_record(p, "user_id", "id").first(&db);
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("tx_commit", BE);
        for (int i = 0; i < 1000; i++) {
            hs::TxGuard g(&db);
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("tx_savepoint", BE);
        for (int i = 0; i < 500; i++) {
            hs::TxGuard outer(&db);
            hs::TxGuard inner(&db);
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("tx_rollback", BE);
        for (int i = 0; i < 500; i++) {
            try {
                hs::TxGuard g(&db);
                db.run("INSERT INTO \"user\" (\"name\", \"age\") VALUES (?, ?)",
                       {hs::Val::text("x"), hs::Val::int_(1)});
                throw std::runtime_error("bench rollback");
            } catch (const std::exception&) {
            }
            t.add(1);
        }
        t.done();
    }
    for (int n : {100, 1000, 10000}) {
        hs::SqliteDb bdb(":memory:");
        bdb.run(USER_DDL, {});
        std::vector<hs::Val> recs;
        for (int i = 0; i < n; i++) recs.push_back(mkuser(i));
        char name[32];
        snprintf(name, sizeof name, "batch_create_%d", n);
        bench::Timer t(name, BE);
        hs::Val created = hs::orm_create_many(&user_model(), &bdb, hs::Val::list(recs));
        t.add(n);
        t.done();
        (void)created;
    }
    {
        hs::SqliteDb bdb(":memory:");
        bdb.run(USER_DDL, {});
        std::vector<hs::Val> recs;
        for (int i = 0; i < 10000; i++) recs.push_back(mkuser(i));
        hs::Val created = hs::orm_create_many(&user_model(), &bdb, hs::Val::list(recs));
        std::vector<hs::Val> ids;
        for (auto& r : created.arr) ids.push_back(*r.find("id"));
        bench::Timer t("batch_find_10000", BE);
        (void)hs::orm_find_many(&user_model(), &bdb, hs::Val::list(ids));
        t.add(10000);
        t.done();
        std::vector<hs::Val> upd;
        for (auto& r : created.arr) {
            hs::Val u = r;
            u.set("age", hs::Val::int_(1));
            upd.push_back(u);
        }
        bench::Timer t2("batch_update_10000", BE);
        (void)hs::orm_update_many(&user_model(), &bdb, hs::Val::list(upd));
        t2.add(10000);
        t2.done();
        bench::Timer t3("batch_delete_10000", BE);
        (void)hs::orm_delete_many(&user_model(), &bdb, hs::Val::list(ids));
        t3.add(10000);
        t3.done();
    }
    {
        bench::Timer t("delete", BE);
        for (int i = 1; i <= 2000; i++) {
            (void)hs::orm_delete_key(&user_model(), &db, hs::Val::int_(i));
            t.add(1);
        }
        t.done();
    }
    bench::metrics(BE);
    return 0;
}
