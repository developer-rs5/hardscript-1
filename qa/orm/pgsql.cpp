// qa/orm/pgsql.cpp -- the PostgreSQL backend against a server on a socket
//
// The backend speaks the wire protocol: messages, field counts, NUL-terminated
// names, text-format values. None of that is exercised by a fake backend that
// returns rows from a vector, so this file is a server. It listens on loopback,
// reads the messages the client writes, and answers with whatever rows the test
// scripted for that statement.
//
// The point is not to be PostgreSQL. It is to be bytes: if the client frames a
// message wrong, this server sees it, and the assertions below are about what
// actually crossed the socket.

#include "hs_runtime_pgsql.hpp"
#include "hs_runtime_queue.hpp"

#include "test_support.hpp"

#include <atomic>
#include <cstring>
#include <mutex>
#include <thread>

#include "mock_pg.hpp"

namespace qa {
namespace {

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

const hs::OrmModel& pg_user_model() {
    static hs::OrmModel m(
        "User", "user", "id", {"id", "name", "age", "active", "note", "created", "updated"},
        {hs::OrmKind::Int, hs::OrmKind::Text, hs::OrmKind::Int, hs::OrmKind::Bool, hs::OrmKind::Text,
         hs::OrmKind::Time, hs::OrmKind::Time},
        {"id", "created", "updated"}, "updated");
    return m;
}

/// The `user` table the fixture writes through, with the generated columns a
/// model really has.
hs::Val a_user() {
    return hs::Val::object({{"name", hs::Val::text("ann")},
                            {"age", hs::Val::int_(30)},
                            {"active", hs::Val::boolean(true)},
                            {"note", hs::Val::nil()}});
}

/// An insert that succeeds and returns nothing, which is what an INSERT
/// without RETURNING looks like from the server's side.
PgReply insert_reply(const std::string& match = "INSERT INTO") {
    PgReply r;
    r.match = match;
    r.tag = "INSERT 0 1";
    r.affected = 1;
    return r;
}

/// The rows a `SELECT name, age FROM user` answers with.
PgReply users_reply() {
    PgReply r;
    r.match = "FROM \"user\"";
    r.columns = {PgColumn("name", 25), PgColumn("age", 23), PgColumn("active", 16)};
    r.rows = {{PgCell("ann"), PgCell("30"), PgCell("t")},
              {PgCell("bo"), PgCell("17"), PgCell("f")},
              {PgCell("cy"), PgCell("44"), PgCell("f")}};
    r.tag = "SELECT 3";
    r.affected = 0;
    return r;
}

// ---------------------------------------------------------------------------
// Tests: the handshake
// ---------------------------------------------------------------------------

void the_client_asks_for_a_connection_and_is_told_it_is_ready() {
    MockPg s({users_reply()});
    hs::PgDb db(s.conninfo());
    CHECK_EQ(s.startup_user(), std::string("tester"), "the startup message named the user");
    CHECK_EQ(s.startup_db(), std::string("qa"), "and the database");
    CHECK_EQ(s.auth_requests(), 0, "trust needs no password");
}

void a_password_is_sent_when_the_server_asks_for_one() {
    MockPg s({users_reply()});
    s.auth_mode = "cleartext";
    hs::PgDb db("host=127.0.0.1 port=" + std::to_string(s.port()) + " user=tester dbname=qa password=hunter2");
    CHECK_EQ(s.auth_requests(), 1, "one password message");
    CHECK_EQ(s.startup_password(), std::string("hunter2"), "and it was the password given");
}

void an_md5_challenge_is_answered_the_way_md5_is_meant_to_be() {
    // md5(md5(password + user) + salt), the way PostgreSQL has always done it.
    MockPg s({users_reply()});
    s.auth_mode = "md5";
    hs::PgDb db("host=127.0.0.1 port=" + std::to_string(s.port()) + " user=tester dbname=qa password=hunter2");
    std::string want = "md5" + hs::md5_hex(hs::md5_hex("hunter2tester") + "salt");
    CHECK_EQ(s.md5_response(), want, "the hash of the hash");
}

void a_connection_to_nowhere_says_where_it_tried() {
    // A port nothing is listening on: the message has to name the address, or
    // "connection refused" alone leaves the reader guessing which of ten
    // databases is misconfigured.
    std::string ci = "host=127.0.0.1 port=1 user=tester dbname=qa";
    CHECK_THROWS_MSG(hs::PgDb db(ci), "cannot connect to 127.0.0.1:1", "the address is in the message");
}

void a_bad_host_is_reported_before_any_socket_work() {
    CHECK_THROWS_MSG(hs::PgDb db("host=not a host port=5432"), "invalid host", "and it says which part");
}

// ---------------------------------------------------------------------------
// Tests: values
// ---------------------------------------------------------------------------

void a_statement_is_parsed_once_and_its_values_travel_apart() {
    MockPg s({users_reply()});
    hs::PgDb db(s.conninfo());
    auto r = db.run("SELECT \"id\", \"name\" FROM \"user\" WHERE age >= $1", {hs::Val::int_(18)});
    auto st = s.statements();
    CHECK_EQ(st.size(), size_t(1), "one statement");
    CHECK_EQ(st[0].sql, std::string("SELECT \"id\", \"name\" FROM \"user\" WHERE age >= $1"),
             "the SQL as written, with the value not in it");
    CHECK_EQ(st[0].params.size(), size_t(1), "one value");
    CHECK_EQ(st[0].params[0].text, std::string("18"), "bound, not interpolated");
    CHECK_EQ(r.rows.size(), size_t(3), "and the rows came back");
}

void every_kind_of_value_becomes_the_text_postgres_expects() {
    MockPg s({insert_reply(), users_reply()});
    hs::PgDb db(s.conninfo());
    (void)db.run("INSERT INTO \"user\" (\"name\", \"age\", \"active\", \"note\") VALUES ($1, $2, $3, $4)",
                 {hs::Val::text("ann"), hs::Val::int_(30), hs::Val::boolean(true), hs::Val::nil()});
    (void)db.run("INSERT INTO \"user\" (\"name\", \"age\", \"active\", \"note\") VALUES ($1, $2, $3, $4)",
                 {hs::Val::text("bo"), hs::Val::flt(0.1), hs::Val::boolean(false),
                  hs::Val::text("")});
    auto st = s.statements();
    CHECK_EQ(st[0].params[0].text, std::string("ann"), "text is itself");
    CHECK_EQ(st[0].params[1].text, std::string("30"), "an int is decimal");
    CHECK_EQ(st[0].params[2].text, std::string("t"), "true is t");
    CHECK(st[0].params[3].is_null, "nil is a NULL, which is not the empty string");
    CHECK_EQ(st[1].params[1].text, std::string("0.1"), "a float is the shortest text that reads back");
    CHECK_EQ(st[1].params[2].text, std::string("f"), "false is f");
    CHECK(!st[1].params[3].is_null, "and an empty string is a string, not a NULL");
}

void a_double_is_written_so_it_reads_back_as_the_same_double() {
    // %.17g always round-trips but turns 0.1 into 0.10000000000000001 in a log.
    for (double v : {0.1, 1.0 / 3.0, 1e308, 1.7976931348623157e308, 2.2250738585072014e-308}) {
        std::string t;
        CHECK(hs::pg_param_text(hs::Val::flt(v), t), "a float has a text form");
        CHECK_EQ(strtod(t.c_str(), nullptr), v, "and reading it back gives the same double");
    }
    std::string t;
    hs::pg_param_text(hs::Val::flt(0.0 / 0.0), t);
    CHECK_EQ(t, std::string("NaN"), "NaN, which SQL has a name for");
    hs::pg_param_text(hs::Val::flt(1.0 / 0.0), t);
    CHECK_EQ(t, std::string("Infinity"), "and Infinity");
    hs::pg_param_text(hs::Val::flt(-1.0 / 0.0), t);
    CHECK_EQ(t, std::string("-Infinity"), "in both directions");
}

void a_list_is_not_a_value_a_column_can_hold() {
    MockPg s({users_reply()});
    hs::PgDb db(s.conninfo());
    CHECK_THROWS_MSG(db.run("INSERT INTO \"user\" (\"name\") VALUES ($1)",
                            {hs::Val::list({hs::Val::int_(1)})}),
                     "not a value a column can hold", "the error says why");
}

// ---------------------------------------------------------------------------
// Tests: what comes back
// ---------------------------------------------------------------------------

void a_column_type_says_whether_those_digits_are_a_number() {
    // The same three bytes "17" are a number in an INT4 column and text in a
    // TEXT one. The type OID is the only thing that knows which.
    CHECK(hs::pg_decode(20, "9007199254740993").is_int(), "int8 is a number");
    CHECK_EQ(hs::pg_decode(20, "9007199254740993").iv, int64_t(9007199254740993LL), "with every digit of it");
    CHECK(hs::pg_decode(23, "17").is_int(), "int4 as well");
    CHECK(hs::pg_decode(701, "1.5").is_flt(), "float8 is a double");
    CHECK_EQ(hs::pg_decode(701, "1.5").fv, 1.5, "with the value");
    CHECK_EQ(hs::pg_decode(16, "t").bv, true, "t is true");
    CHECK_EQ(hs::pg_decode(16, "f").bv, false, "f is false");
    CHECK(hs::pg_decode(25, "17").is_str(), "text is text, even when it is digits");
    CHECK(hs::pg_decode(1184, "2026-01-02 03:04:05+00").is_str(),
          "a timestamptz stays text: it is not a number, and the model decides what it means");
    CHECK(hs::pg_decode(1700, "10.25").is_str(), "and numeric stays text too, because a double would lose "
                                             "the digits that made it money");
    CHECK(hs::pg_decode(2950, "6f1a2b").is_str(), "a uuid is a string");
    CHECK(hs::pg_decode(701, "NaN").is_flt(), "a NaN is still a double");
    CHECK(hs::pg_decode(701, "NaN").fv != hs::pg_decode(701, "NaN").fv, "and it is not equal to itself, as NaN is not");
}

void a_row_of_text_becomes_objects_the_orm_can_hand_out() {
    std::vector<PgReply> replies = {
        PgReply(),
    };
    replies[0].match = "FROM \"user\"";
    replies[0].columns = {PgColumn("id", 20), PgColumn("name", 25), PgColumn("age", 23),
                          PgColumn("active", 16), PgColumn("note", 25), PgColumn("created", 1184),
                          PgColumn("updated", 1184)};
    replies[0].rows = {{PgCell("1"), PgCell("ann"), PgCell("30"), PgCell("t"), PgCell::null(),
                        PgCell("2026-01-02 03:04:05+00"), PgCell("2026-01-02 03:04:06+00")},
                       {PgCell("2"), PgCell("bo"), PgCell("17"), PgCell("f"), PgCell("hi"),
                        PgCell("2026-01-03"), PgCell("2026-01-04")}};
    replies[0].tag = "SELECT 2";
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    auto rows = hs::OrmQuery(&pg_user_model()).order_by("age", false).all(&db);
    CHECK_EQ(rows.arr.size(), size_t(2), "two rows");
    const hs::Val& ann = rows.arr[0];
    CHECK_EQ(ann.find("name")->sv, std::string("ann"), "a text column is a string");
    CHECK_EQ(ann.find("age")->iv, int64_t(30), "an integer column is a number");
    CHECK_EQ(ann.find("active")->bv, true, "a boolean column is a boolean");
    CHECK(ann.find("note")->is_nil(), "a NULL field is nil, not an empty string");
    CHECK_EQ(ann.find("created")->sv, std::string("2026-01-02 03:04:05+00"), "a timestamp is its text");
    CHECK_EQ(rows.arr[1].find("note")->sv, std::string("hi"), "and a value that is there is not nil");
}

void a_command_tag_says_how_many_rows_moved() {
    std::vector<PgReply> replies(2);
    replies[0].match = "UPDATE";
    replies[0].tag = "UPDATE 3";
    replies[1].match = "DELETE";
    replies[1].tag = "DELETE 0";
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    CHECK_EQ(db.run("UPDATE \"user\" SET \"age\" = $1 WHERE \"age\" < $2",
                    {hs::Val::int_(0), hs::Val::int_(18)}).affected,
             int64_t(3), "UPDATE 3 is three rows");
    CHECK_EQ(db.run("DELETE FROM \"user\" WHERE \"age\" < $1", {hs::Val::int_(0)}).affected, int64_t(0),
             "a tag that matched nothing is zero");
}

void postgres_asks_for_the_key_it_just_made() {
    // There is no last_insert_rowid() here, so the insert says RETURNING and
    // the row is the answer.
    std::vector<PgReply> replies(1);
    replies[0].match = "INSERT INTO";
    replies[0].columns = {PgColumn("id", 23)};
    replies[0].rows = {{PgCell("42")}};
    replies[0].tag = "INSERT 0 1";
    replies[0].affected = 1;
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    hs::Val created = hs::orm_create(&pg_user_model(), &db, a_user());
    CHECK_EQ(created.find("id")->iv, int64_t(42), "the generated key is on the record");
    auto st = s.statements();
    CHECK(st[0].sql.find("RETURNING \"id\"") != std::string::npos,
          "and the statement asked for it rather than guessing afterwards");
    CHECK(st[0].sql.find("'ann'") == std::string::npos, "with no value written into the SQL");
}

void a_batch_parses_once_and_binds_every_set() {
    // One Parse for the whole batch, then a Bind per record: the planning
    // happens once no matter how many rows follow.
    std::vector<PgReply> replies(1);
    replies[0].match = "UPDATE";
    replies[0].tag = "UPDATE 1";
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    hs::Val a = hs::Val::object({{"id", hs::Val::int_(1)},
                                 {"name", hs::Val::text("ann")},
                                 {"age", hs::Val::int_(30)},
                                 {"active", hs::Val::boolean(true)},
                                 {"note", hs::Val::nil()}});
    hs::Val b = hs::Val::object({{"id", hs::Val::int_(2)},
                                 {"name", hs::Val::text("bo")},
                                 {"age", hs::Val::int_(17)},
                                 {"active", hs::Val::boolean(false)},
                                 {"note", hs::Val::nil()}});
    (void)hs::orm_update_many(&pg_user_model(), &db, hs::Val::list({a, b}));
    auto parsed = s.statements();
    CHECK_EQ(parsed.size(), size_t(1), "one Parse for two records");
    // Executions include the transaction's own BEGIN and COMMIT; the batch
    // is the UPDATEs.
    std::vector<PgSeen> ran;
    for (auto& e : s.executions())
        if (e.sql.find("UPDATE") != std::string::npos) ran.push_back(e);
    CHECK_EQ(ran.size(), size_t(2), "but two executions");
    CHECK_EQ(ran[0].params[0].text, std::string("ann"), "first record's values");
    CHECK_EQ(ran[1].params[0].text, std::string("bo"), "second record's values");
    CHECK(ran[0].params[3].is_null, "a nil note binds NULL, not an empty string");
    CHECK(ran[0].sql.find("$5") != std::string::npos, "with numbered placeholders");
}

void a_created_batch_reads_its_keys_off_returning_rows() {
    std::vector<PgReply> replies(1);
    replies[0].match = "INSERT INTO";
    replies[0].columns = {PgColumn("id", 23)};
    replies[0].rows = {{PgCell("7")}, {PgCell("8")}};
    replies[0].tag = "INSERT 0 1";
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    hs::Val created = hs::orm_create_many(
        &pg_user_model(), &db,
        hs::Val::list({hs::Val::object({{"name", hs::Val::text("ann")},
                                        {"age", hs::Val::int_(30)},
                                        {"active", hs::Val::boolean(true)},
                                        {"note", hs::Val::nil()}})}));
    CHECK_EQ(created.arr.size(), size_t(1), "one record");
    CHECK_EQ(created.arr[0].find("id")->iv, int64_t(7), "with the key the row carried");
}

void a_found_batch_keeps_key_order_on_postgres_too() {
    std::vector<PgReply> replies(1);
    replies[0].match = "IN (";
    replies[0].columns = {PgColumn("id", 23), PgColumn("name", 25)};
    // Deliberately out of order: the database answers 1, 2 and the batch
    // must hand back 2, 1.
    replies[0].rows = {{PgCell("1"), PgCell("ann")}, {PgCell("2"), PgCell("bo")}};
    replies[0].tag = "SELECT 2";
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    hs::Val rows = hs::orm_find_many(&pg_user_model(), &db,
                                     hs::Val::list({hs::Val::int_(2), hs::Val::int_(1)}));
    CHECK_EQ(rows.arr.size(), size_t(2), "both rows");
    CHECK_EQ(rows.arr[0].find("name")->sv, std::string("bo"), "in the keys' order");
    CHECK_EQ(rows.arr[1].find("name")->sv, std::string("ann"), "in the keys' order");
}

void a_selected_value_is_not_a_generated_key() {
    // `last_id` means "the key the database just assigned". The first column of
    // a SELECT is a value someone asked for, and reading it as a key would
    // stamp a record with whatever happened to be in front of it.
    std::vector<PgReply> replies = {
        PgReply(),
    };
    replies[0].match = "SELECT";
    replies[0].columns = {PgColumn("age", 23)};
    replies[0].rows = {{PgCell("44")}};
    replies[0].tag = "SELECT 1";
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    CHECK_EQ(db.run("SELECT \"age\" FROM \"user\"", {}).last_id, int64_t(0), "a select assigns nothing");
}

void sqlite_does_not_ask_because_sqlite_remembers() {
    // The other backend's insert is unchanged: it has an id to read off the
    // connection. This is the half of the seam that must not move.
    std::string sql = hs::orm_insert_sql(pg_user_model(), hs::DbDialect::Sqlite, {"name", "age"});
    CHECK_EQ(sql, std::string("INSERT INTO \"user\" (\"name\", \"age\") VALUES (?, ?)"), "as it was");
    std::string pg = hs::orm_insert_sql(pg_user_model(), hs::DbDialect::Postgres, {"name", "age"});
    CHECK_EQ(pg, std::string("INSERT INTO \"user\" (\"name\", \"age\") VALUES ($1, $2) RETURNING \"id\""),
             "and the other one asks");
}

// ---------------------------------------------------------------------------
// Tests: failures
// ---------------------------------------------------------------------------

void a_database_error_arrives_with_its_code_and_its_hint() {
    std::vector<PgReply> replies(1);
    replies[0].match = "INSERT INTO";
    replies[0].error = "duplicate key value violates unique constraint \"user_pkey\"";
    replies[0].error_code = "23505";
    replies[0].error_detail = "Key (id)=(1) already exists.";
    replies[0].error_hint = "Use a different id, or let the database assign one.";
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    CHECK_THROWS_MSG(db.run("INSERT INTO \"user\" (\"id\") VALUES ($1)", {hs::Val::int_(1)}),
                     "23505", "the SQLSTATE is there");
    CHECK_THROWS_MSG(db.run("INSERT INTO \"user\" (\"id\") VALUES ($1)", {hs::Val::int_(1)}),
                     "already exists", "so is what it said about the row");
    CHECK_THROWS_MSG(db.run("INSERT INTO \"user\" (\"id\") VALUES ($1)", {hs::Val::int_(1)}),
                     "let the database assign one", "and what to do about it");
    CHECK_THROWS_MSG(db.run("INSERT INTO \"user\" (\"id\") VALUES ($1)", {hs::Val::int_(1)}),
                     "-- while running: INSERT INTO", "and the statement that failed");
}

void a_failed_statement_inside_a_transaction_is_named_the_next_time() {
    // After an error, PostgreSQL refuses everything until the transaction is
    // rolled back, and its own message only says "current transaction is
    // aborted". Saying which statement caused it is the difference between a
    // five-second and a five-minute search.
    std::vector<PgReply> replies(1);
    replies[0].match = "INSERT INTO";
    replies[0].error = "not_null_violation";
    replies[0].error_code = "23502";
    replies[0].ready = 'E';
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    db.begin();
    bool threw = false;
    try {
        (void)db.run("INSERT INTO \"user\" (\"name\") VALUES ($1)", {hs::Val::nil()});
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "the statement failed");
    CHECK_THROWS_MSG(db.run("SELECT 1", {}), "the statement that failed was: INSERT INTO",
                     "and the next refusal names the cause");
    db.rollback();
    CHECK(!db.tx_depth(), "a rollback clears it");
    // The connection is usable again: this one succeeds because the script has
    // nothing for it and the server's own error is about a relation, not about
    // an aborted transaction.
    CHECK_THROWS_MSG(db.run("SELECT 1", {}), "42P01", "and the server speaks for itself again");
}

void a_rollback_to_is_the_way_out_of_a_failed_postgres_transaction() {
    // `ROLLBACK TO SAVEPOINT` is the one statement PostgreSQL accepts from a
    // failed transaction, so the refusal that guards every other statement
    // must let it through — and afterwards the transaction runs again.
    std::vector<PgReply> replies(2);
    replies[0].match = "INSERT INTO";
    replies[0].error = "not_null_violation";
    replies[0].error_code = "23502";
    replies[0].ready = 'E';
    replies[1] = users_reply();
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    db.begin();
    db.savepoint("before");
    bool threw = false;
    try {
        (void)db.run("INSERT INTO \"user\" (\"name\") VALUES ($1)", {hs::Val::nil()});
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "the statement failed and the transaction with it");
    // This is the call the refusal must not eat: without the exemption the
    // guard's own recovery throws, and a nested block can never unwind
    // partially on this backend.
    db.rollback_to("before");
    auto rows = db.run("SELECT \"id\", \"name\" FROM \"user\"", {});
    CHECK_EQ(rows.rows.size(), size_t(3), "and statements run again afterwards");
    db.rollback();
    auto seen = s.statements();
    bool saw_to = false, saw_savepoint = false;
    for (auto& st : seen) {
        if (st.sql.find("ROLLBACK TO \"before\"") != std::string::npos) saw_to = true;
        if (st.sql.find("SAVEPOINT \"before\"") != std::string::npos) saw_savepoint = true;
    }
    CHECK(saw_savepoint, "the savepoint went out on the wire");
    CHECK(saw_to, "and so did the rollback to it");
}

void a_nested_guard_unwinds_partially_on_postgres_too() {
    MockPg s({users_reply()});
    hs::PgDb db(s.conninfo());
    {
        hs::TxGuard outer(&db);
        bool threw = false;
        try {
            hs::TxGuard inner(&db);
            throw std::runtime_error("inner");
        } catch (const std::exception&) {
            threw = true;
        }
        CHECK(threw, "the inner error propagates");
    }
    auto seen = s.statements();
    std::string traffic;
    for (auto& st : seen) traffic += st.sql + "\n";
    CHECK(traffic.find("SAVEPOINT \"hs_sp_1\"") != std::string::npos, "a savepoint opened the inner block");
    CHECK(traffic.find("ROLLBACK TO \"hs_sp_1\"") != std::string::npos, "a rollback unwound it");
    CHECK(traffic.find("RELEASE \"hs_sp_1\"") != std::string::npos, "and a release forgot it");
    CHECK_EQ(s.count("COMMIT"), size_t(1), "while the outer block committed once");
}

void a_fresh_beginning_is_never_failed() {
    // A new top-level transaction cannot inherit the previous one's failure:
    // whatever aborted is over when the rollback or commit that ended it ran.
    std::vector<PgReply> replies(2);
    replies[0].match = "INSERT INTO";
    replies[0].error = "not_null_violation";
    replies[0].error_code = "23502";
    replies[0].ready = 'E';
    replies[1] = users_reply();
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    db.begin();
    bool threw = false;
    try {
        (void)db.run("INSERT INTO \"user\" (\"name\") VALUES ($1)", {hs::Val::nil()});
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "failed");
    db.rollback();
    db.begin();
    auto rows = db.run("SELECT \"id\", \"name\" FROM \"user\"", {});
    CHECK_EQ(rows.rows.size(), size_t(3), "the next transaction runs clean");
    db.commit();
}

void a_statement_with_no_rows_is_not_a_failure() {
    MockPg s({users_reply()});
    hs::PgDb db(s.conninfo());
    auto n = hs::OrmQuery(&pg_user_model()).where_eq("age", hs::Val::int_(999)).count(&db);
    CHECK_EQ(n.iv, int64_t(0), "a COUNT that matched nothing is zero, not an error");
    CHECK_EQ(s.statements().size(), size_t(1), "one statement");
    CHECK(s.statements()[0].sql.find("COUNT") != std::string::npos, "and it was the count");
}

// ---------------------------------------------------------------------------
// Tests: transactions
// ---------------------------------------------------------------------------

void transactions_nest_the_way_both_databases_allow() {
    MockPg s({users_reply()});
    hs::PgDb db(s.conninfo());
    db.begin();
    CHECK_EQ(db.tx_depth(), 1, "one open");
    db.begin();
    CHECK_EQ(db.tx_depth(), 2, "two");
    db.commit();
    CHECK_EQ(db.tx_depth(), 1, "an inner commit ends only the inner marker");
    CHECK_EQ(s.count("COMMIT"), size_t(0), "and sends nothing: the transaction is still open");
    db.commit();
    CHECK_EQ(db.tx_depth(), 0, "the outer commit ends it");
    CHECK_EQ(s.count("COMMIT"), size_t(1), "one COMMIT on the wire");
    db.begin();
    db.rollback();
    CHECK_EQ(s.count("ROLLBACK"), size_t(1), "and a rollback is a rollback");
    CHECK_EQ(db.tx_depth(), 0, "nothing left open");
}

void a_commit_with_nothing_open_is_not_a_statement() {
    MockPg s({users_reply()});
    hs::PgDb db(s.conninfo());
    db.commit();
    db.rollback();
    CHECK_EQ(s.seen().size(), size_t(0), "no traffic for a transaction that never began");
}

// ---------------------------------------------------------------------------
// Tests: the seam
// ---------------------------------------------------------------------------

void the_backend_says_which_one_it_is() {
    MockPg s({users_reply()});
    hs::PgDb db(s.conninfo());
    CHECK_EQ(std::string(db.name()), std::string("postgres"), "by name");
    CHECK(db.dialect() == hs::DbDialect::Postgres, "and by dialect");
    hs::DbBackend* as_backend = &db;
    CHECK_EQ(as_backend->tx_depth(), 0, "through the interface a program holds");
}

void a_query_builder_asks_postgres_for_its_own_spelling() {
    // The builder already speaks both dialects; this is the check that the
    // seam is what chooses, not the SQL that was written.
    qa::DialectOnly pg(hs::DbDialect::Postgres);
    qa::DialectOnly lite(hs::DbDialect::Sqlite);
    CHECK_EQ(hs::OrmQuery(&pg_user_model()).where_eq("name", hs::Val::text("ann")).sql(&pg),
             std::string("SELECT \"id\", \"name\", \"age\", \"active\", \"note\", \"created\", \"updated\" "
                         "FROM \"user\" WHERE \"name\" = $1"),
             "numbered placeholders");
    CHECK_EQ(hs::OrmQuery(&pg_user_model()).where_like("name", hs::Val::text("a%")).sql(&pg),
             std::string("SELECT \"id\", \"name\", \"age\", \"active\", \"note\", \"created\", \"updated\" "
                         "FROM \"user\" WHERE \"name\" ILIKE $1"),
             "and ILIKE, which SQLite spells LIKE");
    CHECK(hs::OrmQuery(&pg_user_model()).where_like("name", hs::Val::text("a%")).sql(&lite).find("ILIKE") ==
              std::string::npos,
          "while SQLite's own spelling has no ILIKE at all");
}

void an_order_and_a_page_survive_the_trip() {
    std::vector<PgReply> replies(1);
    replies[0].match = "ORDER BY";
    replies[0].columns = {PgColumn("id", 23), PgColumn("name", 25), PgColumn("age", 23),
                          PgColumn("active", 16), PgColumn("note", 25), PgColumn("created", 1184),
                          PgColumn("updated", 1184)};
    replies[0].rows = {{PgCell("1"), PgCell("ann"), PgCell("30"), PgCell("t"), PgCell::null(),
                        PgCell("2026-01-01"), PgCell("2026-01-01")},
                       {PgCell("2"), PgCell("cy"), PgCell("44"), PgCell("f"), PgCell::null(),
                        PgCell("2026-01-02"), PgCell("2026-01-02")}};
    replies[0].tag = "SELECT 2";
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    auto page = hs::OrmQuery(&pg_user_model()).order_by("age", false).limit(2).offset(1).all(&db);
    CHECK_EQ(page.arr.size(), size_t(2), "two rows");
    CHECK_EQ(page.arr[0].find("name")->sv, std::string("ann"), "in order");
    CHECK_EQ(page.arr[0].find("note")->is_nil(), true, "with a NULL read as nil");
    auto st = s.statements();
    CHECK(st[0].sql.find("ORDER BY \"age\" LIMIT 2 OFFSET 1") != std::string::npos,
          "and the order and the page in one statement");
}

void a_relationship_predicate_goes_out_as_a_parameter() {
    std::vector<PgReply> replies(1);
    replies[0].match = "post_tag";
    replies[0].columns = {PgColumn("id", 23), PgColumn("name", 25)};
    replies[0].rows = {{PgCell("1"), PgCell("blue")}, {PgCell("2"), PgCell("red")}};
    replies[0].tag = "SELECT 2";
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    auto tags = hs::OrmQuery(&pg_user_model()).of_record(hs::Val::object({{"id", hs::Val::int_(7)}}), "id",
                                                              "post_tag.post_id",
                                                              "INNER JOIN \"post_tag\" ON "
                                                              "\"post_tag\".\"tag_id\" = \"user\".\"id\"")
                      .all(&db);
    CHECK_EQ(tags.arr.size(), size_t(2), "both sides of the junction");
    auto st = s.statements();
    CHECK_EQ(st[0].params.size(), size_t(1), "one bound value");
    CHECK_EQ(st[0].params[0].text, std::string("7"), "the record's key, bound");
    CHECK(st[0].sql.find("\"post_tag\".\"post_id\" = $1") != std::string::npos,
          "against the parent's side of the junction");
}

void a_null_column_is_matched_with_is_null_here_too() {
    std::vector<PgReply> replies(1);
    replies[0].match = "IS NULL";
    replies[0].columns = {PgColumn("n", 20)};
    replies[0].rows = {{PgCell("2")}};
    replies[0].tag = "SELECT 1";
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    CHECK_EQ(hs::OrmQuery(&pg_user_model()).where_eq("note", hs::Val::nil()).count(&db).iv, int64_t(2),
             "a count of the rows with no note");
    auto st = s.statements();
    CHECK_EQ(st[0].params.size(), size_t(0), "IS NULL binds nothing, so there is nothing to send");
    CHECK(st[0].sql.find("\"note\" IS NULL") != std::string::npos, "and says so in the SQL");
}

// ---------------------------------------------------------------------------
// Tests: connection strings
// ---------------------------------------------------------------------------

void a_connection_string_is_read_either_way_it_is_written() {
    hs::PgConnInfo u = hs::pg_parse_conninfo("postgresql://ann:secret@db.internal:6543/shop");
    CHECK_EQ(u.host, std::string("db.internal"), "the host");
    CHECK_EQ(u.port, 6543, "the port");
    CHECK_EQ(u.user, std::string("ann"), "the user");
    CHECK_EQ(u.password, std::string("secret"), "the password");
    CHECK_EQ(u.dbname, std::string("shop"), "the database");

    hs::PgConnInfo k = hs::pg_parse_conninfo("host=127.0.0.1 port=6000 user=bob dbname=blog");
    CHECK_EQ(k.host, std::string("127.0.0.1"), "key=value: the host");
    CHECK_EQ(k.port, 6000, "the port");
    CHECK_EQ(k.user, std::string("bob"), "the user");
    CHECK_EQ(k.dbname, std::string("blog"), "the database");
    CHECK_EQ(k.password, std::string(""), "and no password, which is not the same as an empty one");

    hs::PgConnInfo d = hs::pg_parse_conninfo("");
    CHECK_EQ(d.host, std::string("127.0.0.1"), "an empty string is the defaults");
    CHECK_EQ(d.port, 5432, "the standard port");
    CHECK_EQ(d.dbname, d.user, "and the database named after the user");
    CHECK_EQ(hs::pg_parse_conninfo("postgresql://localhost/x").port, 5432, "a url with no port");
}

void a_server_outlives_one_goodbye() {
    // Connections come one after another in every long-lived program, and
    // the second handshake must not wait on a server thread that went home
    // after the first goodbye.
    MockPg s({users_reply()});
    {
        hs::PgDb first(s.conninfo());
        (void)first.run("SELECT \"id\" FROM \"user\"", {});
    }
    hs::PgDb second(s.conninfo());
    auto rows = second.run("SELECT \"id\" FROM \"user\"", {});
    CHECK(!rows.rows.empty(), "the second connection runs");
    CHECK(s.server_error().empty(), "and the server saw no protocol error: " + s.server_error());
}

void a_job_queue_runs_on_postgres_through_the_same_backend() {
    // The SQL job backend reads the dialect off the connection: against
    // PostgreSQL it claims with SELECT..FOR UPDATE SKIP LOCKED and reads keys
    // off RETURNING, all in one round trip per call. The mock scripts those
    // answers; the assertions are about which SQL crossed the socket.
    std::vector<PgReply> replies;
    PgReply ins;
    ins.match = "INSERT INTO \"hs_jobs\"";
    ins.columns = {PgColumn("id", 23)};
    ins.rows = {{PgCell("5")}};
    ins.tag = "INSERT 0 1";
    replies.push_back(ins);
    PgReply poll;
    poll.match = "FOR UPDATE SKIP LOCKED";
    poll.columns = {PgColumn("id", 23), PgColumn("type", 25), PgColumn("payload", 25),
                    PgColumn("attempts", 23), PgColumn("max_attempts", 23), PgColumn("priority", 23),
                    PgColumn("run_after", 20), PgColumn("last_error", 25), PgColumn("created", 20)};
    poll.rows = {{PgCell("5"), PgCell("mail"), PgCell("{\"to\":\"a\"}"), PgCell("0"), PgCell("5"),
                  PgCell("0"), PgCell("1000000"), PgCell(""), PgCell("1000000")}};
    poll.tag = "SELECT 1";
    replies.push_back(poll);
    PgReply del;
    del.match = "DELETE FROM \"hs_jobs\"";
    del.tag = "DELETE 1";
    replies.push_back(del);
    PgReply count;
    count.match = "COUNT(*)";
    count.columns = {PgColumn("n", 20)};
    count.rows = {{PgCell("0")}};
    count.tag = "SELECT 1";
    replies.push_back(count);
    // Catch-all last: DDL, the recovery UPDATE, anything the test does not
    // assert on. First match wins, so the specific scripts above keep theirs.
    replies.push_back(PgReply());
    MockPg s(replies);
    hs::PgDb db(s.conninfo());
    hs::SqlJobBackend b(&db);
    hs::Job j;
    j.type = "mail";
    j.payload = hs::Val::object({{"to", hs::Val::text("a")}});
    j.max_attempts = 5;
    CHECK_EQ(b.push(j), int64_t(5), "the key off RETURNING");
    hs::Job out;
    CHECK(b.poll({"mail"}, 1000000, out), "claimed");
    CHECK_EQ(out.id, int64_t(5), "the same job");
    CHECK_EQ(out.payload.find("to")->sv, std::string("a"), "payload decoded");
    b.complete(out.id);
    auto seen = s.statements();
    bool saw_returning = false, saw_skip_locked = false;
    for (auto& st : seen) {
        if (st.sql.find("RETURNING \"id\"") != std::string::npos) saw_returning = true;
        if (st.sql.find("FOR UPDATE SKIP LOCKED") != std::string::npos) saw_skip_locked = true;
        CHECK(st.sql.find("?") == std::string::npos, "numbered placeholders, never question marks");
    }
    CHECK(saw_returning, "postgres insert asks for its key");
    CHECK(saw_skip_locked, "postgres claim skips locked rows");
}

void a_closing_connection_says_goodbye() {
    // Terminate is the one message with no reply, and the server thread exits
    // when it arrives. A client that just dropped the socket would leave the
    // server waiting on a query that is never coming.
    MockPg s({users_reply()});
    {
        hs::PgDb db(s.conninfo());
        (void)db.run("SELECT 1 FROM \"user\"", {});
    }
    CHECK_EQ(s.server_error(), std::string(""), "the server saw no protocol error");
}

void a_dropped_connection_says_the_connection_was_lost() {
    // A server that goes away mid-query is an ordinary event: a restart, a
    // failover, an idle timeout. The message has to say that, because a
    // truncated read otherwise looks like a protocol bug in the client.
    MockPg s({users_reply()});
    hs::PgDb db(s.conninfo());
    (void)db.run("SELECT \"id\" FROM \"user\"", {});
    s.drop_client();
    CHECK_THROWS_MSG(db.run("SELECT \"id\" FROM \"user\"", {}), "connection lost",
                     "and it is the connection, not the statement");
}

}  // namespace
}  // namespace qa

int main() {
    qa::the_client_asks_for_a_connection_and_is_told_it_is_ready();
    qa::a_password_is_sent_when_the_server_asks_for_one();
    qa::an_md5_challenge_is_answered_the_way_md5_is_meant_to_be();
    qa::a_connection_to_nowhere_says_where_it_tried();
    qa::a_bad_host_is_reported_before_any_socket_work();

    qa::a_statement_is_parsed_once_and_its_values_travel_apart();
    qa::every_kind_of_value_becomes_the_text_postgres_expects();
    qa::a_double_is_written_so_it_reads_back_as_the_same_double();
    qa::a_list_is_not_a_value_a_column_can_hold();

    qa::a_column_type_says_whether_those_digits_are_a_number();
    qa::a_row_of_text_becomes_objects_the_orm_can_hand_out();
    qa::a_command_tag_says_how_many_rows_moved();
    qa::postgres_asks_for_the_key_it_just_made();
    qa::a_batch_parses_once_and_binds_every_set();
    qa::a_created_batch_reads_its_keys_off_returning_rows();
    qa::a_found_batch_keeps_key_order_on_postgres_too();
    qa::a_selected_value_is_not_a_generated_key();
    qa::sqlite_does_not_ask_because_sqlite_remembers();

    qa::a_database_error_arrives_with_its_code_and_its_hint();
    qa::a_failed_statement_inside_a_transaction_is_named_the_next_time();
    qa::a_rollback_to_is_the_way_out_of_a_failed_postgres_transaction();
    qa::a_nested_guard_unwinds_partially_on_postgres_too();
    qa::a_fresh_beginning_is_never_failed();
    qa::a_statement_with_no_rows_is_not_a_failure();

    qa::transactions_nest_the_way_both_databases_allow();
    qa::a_commit_with_nothing_open_is_not_a_statement();

    qa::the_backend_says_which_one_it_is();
    qa::a_query_builder_asks_postgres_for_its_own_spelling();
    qa::an_order_and_a_page_survive_the_trip();
    qa::a_relationship_predicate_goes_out_as_a_parameter();
    qa::a_null_column_is_matched_with_is_null_here_too();

    qa::a_connection_string_is_read_either_way_it_is_written();
    qa::a_server_outlives_one_goodbye();
    qa::a_job_queue_runs_on_postgres_through_the_same_backend();
    qa::a_closing_connection_says_goodbye();
    qa::a_dropped_connection_says_the_connection_was_lost();

    return qa::report("pgsql");
}
