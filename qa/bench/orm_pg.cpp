// orm_pg.cpp -- operation benchmarks against the scripted mock server
//
// The same lanes as orm_sqlite.cpp, speaking the wire protocol to MockPg on
// loopback. The server answers from a script without executing anything, so
// these numbers are the client stack plus a socket round trip: the cost of
// framing, parsing the replies, and decoding rows. The report labels them
// "mock" for exactly that reason.
#include "hs_runtime_pgsql.hpp"

#include "bench.hpp"
#include "mock_pg.hpp"

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

std::vector<qa::PgReply> script() {
    std::vector<qa::PgReply> out;
    qa::PgReply ddl;
    ddl.match = "CREATE TABLE";
    out.push_back(ddl);
    qa::PgReply users;
    users.match = "FROM \"user\"";
    users.columns = {qa::PgColumn("id", 23), qa::PgColumn("name", 25), qa::PgColumn("age", 23)};
    users.rows = {{qa::PgCell("1"), qa::PgCell("ann"), qa::PgCell("30")}};
    users.tag = "SELECT 1";
    out.push_back(users);
    qa::PgReply posts;
    posts.match = "FROM \"post\"";
    posts.columns = {qa::PgColumn("id", 23), qa::PgColumn("title", 25), qa::PgColumn("user_id", 23)};
    posts.rows = {{qa::PgCell("1"), qa::PgCell("p1"), qa::PgCell("1")},
                  {qa::PgCell("2"), qa::PgCell("p2"), qa::PgCell("1")}};
    posts.tag = "SELECT 2";
    out.push_back(posts);
    qa::PgReply ins;
    ins.match = "INSERT INTO";
    ins.columns = {qa::PgColumn("id", 23)};
    ins.rows = {{qa::PgCell("100")}};
    ins.tag = "INSERT 0 1";
    ins.affected = 1;
    out.push_back(ins);
    qa::PgReply upd;
    upd.match = "UPDATE";
    upd.tag = "UPDATE 1";
    out.push_back(upd);
    qa::PgReply del;
    del.match = "DELETE";
    del.tag = "DELETE 1";
    out.push_back(del);
    return out;
}

hs::Val mkuser(int i) {
    return hs::Val::object(
        {{"name", hs::Val::text("user" + std::to_string(i))}, {"age", hs::Val::int_(20 + i % 50)}});
}

const char* USER_DDL = "CREATE TABLE \"user\" (\"id\" INTEGER PRIMARY KEY,\"name\" TEXT NOT NULL,\"age\" INTEGER "
                       "NOT NULL)";
const char* POST_DDL = "CREATE TABLE \"post\" (\"id\" INTEGER PRIMARY KEY,\"title\" TEXT NOT NULL,\"user_id\" "
                       "INTEGER NOT NULL)";

}  // namespace

int main() {
    const char* BE = "pg-mock";
    qa::MockPg server(script());
    {
        bench::Timer t("startup", BE);
        for (int i = 0; i < 20; i++) {
            hs::PgDb db(server.conninfo());
            db.run(USER_DDL, {});
            t.add(1);
        }
        t.done();
    }
    hs::PgDb db(server.conninfo());
    db.run(USER_DDL, {});
    db.run(POST_DDL, {});
    // One connection for the whole run: the mock serves a single client at a
    // time, and batch lanes isolate themselves with their own table names.
    {
        bench::Timer t("create", BE);
        for (int i = 0; i < 500; i++) {
            (void)hs::orm_create(&user_model(), &db, mkuser(i));
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("update", BE);
        for (int i = 1; i <= 500; i++) {
            hs::Val rec =
                hs::Val::object({{"id", hs::Val::int_(i)}, {"age", hs::Val::int_(99)}});
            (void)hs::orm_save(&user_model(), &db, rec);
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("find", BE);
        for (int i = 1; i <= 500; i++) {
            (void)hs::OrmQuery(&user_model()).where_eq("id", hs::Val::int_(i)).first(&db);
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("find_many", BE);
        for (int base = 1; base <= 500; base += 100) {
            std::vector<hs::Val> ids;
            for (int i = 0; i < 100; i++) ids.push_back(hs::Val::int_(base + i));
            (void)hs::orm_find_many(&user_model(), &db, hs::Val::list(ids));
            t.add(100);
        }
        t.done();
    }
    {
        bench::Timer t("join_has_many", BE);
        for (int i = 1; i <= 200; i++) {
            hs::Val u = hs::Val::object({{"id", hs::Val::int_(i)}});
            (void)hs::OrmQuery(&post_model()).of_record(u, "id", "user_id").all(&db);
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("join_belongs_to", BE);
        for (int i = 1; i <= 200; i++) {
            hs::Val p = hs::Val::object(
                {{"id", hs::Val::int_(i)}, {"user_id", hs::Val::int_(1 + i % 2000)}});
            (void)hs::OrmQuery(&user_model()).of_record(p, "user_id", "id").first(&db);
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("tx_commit", BE);
        for (int i = 0; i < 500; i++) {
            hs::TxGuard g(&db);
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("tx_savepoint", BE);
        for (int i = 0; i < 200; i++) {
            hs::TxGuard outer(&db);
            hs::TxGuard inner(&db);
            t.add(1);
        }
        t.done();
    }
    {
        bench::Timer t("tx_rollback", BE);
        for (int i = 0; i < 200; i++) {
            try {
                hs::TxGuard g(&db);
                db.run("INSERT INTO \"user\" (\"name\", \"age\") VALUES ($1, $2)",
                       {hs::Val::text("x"), hs::Val::int_(1)});
                throw std::runtime_error("bench rollback");
            } catch (const std::exception&) {
            }
            t.add(1);
        }
        t.done();
    }
    for (int n : {100, 1000}) {
        std::string tbl = "buser" + std::to_string(n);
        std::string ddl = "CREATE TABLE \"" + tbl +
                          "\" (\"id\" INTEGER PRIMARY KEY,\"name\" TEXT NOT NULL,\"age\" INTEGER NOT NULL)";
        db.run(ddl, {});
        // The batch model points at this lane's table; the mock answers any
        // CREATE TABLE, and every other statement matches by shape.
        hs::OrmModel bm("User", tbl, "id", {"id", "name", "age"},
                        {hs::OrmKind::Int, hs::OrmKind::Text, hs::OrmKind::Int}, {"id"}, "");
        std::vector<hs::Val> recs;
        for (int i = 0; i < n; i++) recs.push_back(mkuser(i));
        char name[32];
        snprintf(name, sizeof name, "batch_create_%d", n);
        bench::Timer t(name, BE);
        hs::Val created = hs::orm_create_many(&bm, &db, hs::Val::list(recs));
        t.add(n);
        t.done();
        (void)created;
    }
    {
        bench::Timer t("delete", BE);
        for (int i = 1; i <= 500; i++) {
            (void)hs::orm_delete_key(&user_model(), &db, hs::Val::int_(i));
            t.add(1);
        }
        t.done();
    }
    bench::metrics(BE);
    return 0;
}
