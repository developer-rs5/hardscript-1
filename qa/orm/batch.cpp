// qa/orm/batch.cpp -- batch operations
//
// One row at a time is a loop the programmer writes; a batch is a loop the
// runtime owns, inside one transaction, so ten thousand rows either land or
// do not. The fake backend below checks the shape of the calls (one
// transaction, one statement shape per group); real SQLite checks the rows.

#include "hs_runtime_orm.hpp"
#include "hs_runtime_sqlite.hpp"

#include "test_support.hpp"

namespace qa {
namespace {

const hs::OrmModel& user_model() {
    static hs::OrmModel m(
        "User", "user", "id", {"id", "name", "age", "active", "note", "created", "updated"},
        {hs::OrmKind::Int, hs::OrmKind::Text, hs::OrmKind::Int, hs::OrmKind::Bool, hs::OrmKind::Text,
         hs::OrmKind::Time, hs::OrmKind::Time},
        {"id", "created", "updated"}, "updated");
    return m;
}

hs::Val user(const char* name, int age) {
    return hs::Val::object({{std::string("name"), hs::Val::text(name)},
                            {std::string("age"), hs::Val::int_(age)},
                            {std::string("active"), hs::Val::boolean(true)},
                            {std::string("note"), hs::Val::nil()}});
}

const char* USER_DDL =
    "CREATE TABLE \"user\" (\"id\" INTEGER PRIMARY KEY AUTOINCREMENT,\"name\" TEXT NOT NULL,\"age\" INTEGER "
    "NOT NULL,\"active\" INTEGER NOT NULL,\"note\" TEXT,\"created\" TEXT NOT NULL DEFAULT "
    "(datetime('now')),\"updated\" TEXT NOT NULL DEFAULT (datetime('now')))";

int64_t user_count(hs::SqliteDb& db) {
    return db.run("SELECT COUNT(*) FROM \"user\"", {}).rows[0][0].iv;
}

/// A fake that records how `run_many` groups its sets, so the grouping logic
/// is observable without a database.
struct BatchFake : FakeBackend {
    std::vector<size_t> group_sizes;
    hs::DbResult run_many(const std::string& sql,
                          const std::vector<std::vector<hs::Val>>& sets) override {
        group_sizes.push_back(sets.size());
        return FakeBackend::run_many(sql, sets);
    }
};

// ---------------------------------------------------------------------------
// create_many
// ---------------------------------------------------------------------------

void created_records_come_back_with_keys_in_order() {
    hs::SqliteDb db(":memory:");
    db.run(USER_DDL, {});
    hs::Val created = hs::orm_create_many(
        &user_model(), &db,
        hs::Val::list({user("ann", 30), user("bo", 17), user("cy", 44)}));
    CHECK_EQ(created.arr.size(), size_t(3), "three records");
    CHECK_EQ(created.arr[0].find("id")->iv, int64_t(1), "keys in order");
    CHECK_EQ(created.arr[1].find("id")->iv, int64_t(2), "keys in order");
    CHECK_EQ(created.arr[2].find("id")->iv, int64_t(3), "keys in order");
    CHECK_EQ(created.arr[0].find("name")->sv, std::string("ann"), "values carried through");
    CHECK_EQ(user_count(db), int64_t(3), "and three rows");
}

void an_empty_batch_inserts_nothing_and_opens_nothing() {
    FakeBackend b;
    hs::Val out = hs::orm_create_many(&user_model(), &b, hs::Val::list({}));
    CHECK_EQ(out.arr.size(), size_t(0), "empty in, empty out");
    CHECK_EQ(b.calls(), size_t(0), "and no statement at all, not even a transaction");
}

void one_batch_is_one_transaction() {
    FakeBackend b;
    (void)hs::orm_create_many(&user_model(), &b, hs::Val::list({user("ann", 30), user("bo", 17)}));
    CHECK_EQ(b.begins, 1, "one begin");
    CHECK_EQ(b.commits, 1, "one commit");
    CHECK_EQ(b.rollbacks, 0, "no rollback");
    CHECK_EQ(b.calls(), size_t(2), "one statement per record");
    CHECK(b.last_sql().find("INSERT INTO") != std::string::npos, "inserts");
}

void a_bad_record_fails_the_whole_batch() {
    hs::SqliteDb db(":memory:");
    db.run(USER_DDL, {});
    bool threw = false;
    try {
        (void)hs::orm_create_many(&user_model(), &db,
                                  hs::Val::list({user("ann", 30),
                                                 hs::Val::object({{"name", hs::Val::text("bo")}}),
                                                 user("cy", 44)}));
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("record 2") != std::string::npos;
    }
    CHECK(threw, "the error names the record");
    CHECK_EQ(user_count(db), int64_t(0), "and nothing was inserted, not even the good ones");
}

void a_non_object_is_not_a_record() {
    hs::SqliteDb db(":memory:");
    db.run(USER_DDL, {});
    bool threw = false;
    try {
        (void)hs::orm_create_many(&user_model(), &db, hs::Val::list({hs::Val::int_(1)}));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "a number is not a record");
    threw = false;
    try {
        (void)hs::orm_create_many(&user_model(), &db, hs::Val::int_(1));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "and a non-list is not a batch");
}

// ---------------------------------------------------------------------------
// update_many
// ---------------------------------------------------------------------------

void updated_records_come_back_updated() {
    hs::SqliteDb db(":memory:");
    db.run(USER_DDL, {});
    hs::Val created =
        hs::orm_create_many(&user_model(), &db, hs::Val::list({user("ann", 30), user("bo", 17)}));
    hs::Val first = created.arr[0];
    first.set("age", hs::Val::int_(31));
    hs::Val second = created.arr[1];
    second.set("name", hs::Val::text("bob"));
    second.set("age", hs::Val::int_(18));
    hs::Val out = hs::orm_update_many(&user_model(), &db, hs::Val::list({first, second}));
    CHECK_EQ(out.arr.size(), size_t(2), "both back");
    CHECK_EQ(out.arr[0].find("age")->iv, int64_t(31), "with new values");
    CHECK_EQ(db.run("SELECT \"age\" FROM \"user\" WHERE \"id\" = 1", {}).rows[0][0].iv, int64_t(31),
             "in the table");
    CHECK_EQ(db.run("SELECT \"name\" FROM \"user\" WHERE \"id\" = 2", {}).rows[0][0].sv,
             std::string("bob"), "in the table");
}

void alike_records_share_one_statement() {
    BatchFake b;
    hs::Val a = user("ann", 30);
    a.set("id", hs::Val::int_(1));
    hs::Val c = user("cy", 44);
    c.set("id", hs::Val::int_(3));
    (void)hs::orm_update_many(&user_model(), &b, hs::Val::list({a, c}));
    CHECK_EQ(b.group_sizes.size(), size_t(1), "one group");
    CHECK_EQ(b.group_sizes[0], size_t(2), "holding both records");
    CHECK_EQ(b.calls(), size_t(2), "run once per record through the shared statement");
    CHECK_EQ(b.params[0].size(), size_t(5), "four values and the key");
    CHECK(b.last_sql().find("UPDATE") != std::string::npos, "an update");
}

void unalike_records_do_not_share() {
    BatchFake b;
    hs::Val a = user("ann", 30);
    a.set("id", hs::Val::int_(1));
    hs::Val c = hs::Val::object({{"id", hs::Val::int_(3)}, {"age", hs::Val::int_(44)}});
    (void)hs::orm_update_many(&user_model(), &b, hs::Val::list({a, c}));
    CHECK_EQ(b.group_sizes.size(), size_t(2), "one group per column shape");
}

void a_record_without_a_key_cannot_be_updated() {
    hs::SqliteDb db(":memory:");
    db.run(USER_DDL, {});
    bool threw = false;
    try {
        (void)hs::orm_update_many(&user_model(), &db, hs::Val::list({user("ann", 30)}));
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("no id") != std::string::npos;
    }
    CHECK(threw, "and the error names the missing key");
}

void a_key_only_record_is_left_alone() {
    FakeBackend b;
    hs::Val keyed = hs::Val::object({{"id", hs::Val::int_(1)}});
    hs::Val out = hs::orm_update_many(&user_model(), &b, hs::Val::list({keyed}));
    CHECK_EQ(out.arr.size(), size_t(1), "kept");
    CHECK_EQ(b.calls(), size_t(0), "with no statement: there is nothing to write");
}

// ---------------------------------------------------------------------------
// delete_many
// ---------------------------------------------------------------------------

void deleted_rows_are_counted() {
    hs::SqliteDb db(":memory:");
    db.run(USER_DDL, {});
    (void)hs::orm_create_many(&user_model(), &db,
                              hs::Val::list({user("ann", 30), user("bo", 17), user("cy", 44)}));
    CHECK_EQ(hs::orm_delete_many(&user_model(), &db, hs::Val::list({hs::Val::int_(1), hs::Val::int_(3)})).iv,
             int64_t(2), "two gone");
    CHECK_EQ(user_count(db), int64_t(1), "one left");
    CHECK_EQ(hs::orm_delete_many(&user_model(), &db, hs::Val::list({hs::Val::int_(9)})).iv, int64_t(0),
             "a missing key deletes nothing");
    CHECK_EQ(hs::orm_delete_many(&user_model(), &db, hs::Val::list({})).iv, int64_t(0),
             "and neither does an empty list");
}

void a_long_id_list_is_cut_into_chunks() {
    FakeBackend b;
    std::vector<hs::Val> ids;
    for (int i = 1; i <= 1000; i++) ids.push_back(hs::Val::int_(i));
    (void)hs::orm_delete_many(&user_model(), &b, hs::Val::list(ids));
    CHECK_EQ(b.calls(), size_t(2), "1000 keys in chunks of 900");
    CHECK_EQ(b.params[0].size(), size_t(900), "first chunk full");
    CHECK_EQ(b.params[1].size(), size_t(100), "second chunk the rest");
    CHECK_EQ(b.begins, 1, "still one transaction");
}

// ---------------------------------------------------------------------------
// find_many
// ---------------------------------------------------------------------------

void found_rows_come_back_in_key_order() {
    hs::SqliteDb db(":memory:");
    db.run(USER_DDL, {});
    (void)hs::orm_create_many(&user_model(), &db,
                              hs::Val::list({user("ann", 30), user("bo", 17), user("cy", 44)}));
    hs::Val rows = hs::orm_find_many(
        &user_model(), &db, hs::Val::list({hs::Val::int_(3), hs::Val::int_(1), hs::Val::int_(9)}));
    CHECK_EQ(rows.arr.size(), size_t(2), "two of three keys have rows");
    CHECK_EQ(rows.arr[0].find("name")->sv, std::string("cy"), "in the keys' order, not the table's");
    CHECK_EQ(rows.arr[1].find("name")->sv, std::string("ann"), "in the keys' order, not the table's");
    CHECK_EQ(rows.arr[0].find("age")->iv, int64_t(44), "decoded through the model");
}

void an_empty_key_list_reads_nothing() {
    FakeBackend b;
    CHECK_EQ(hs::orm_find_many(&user_model(), &b, hs::Val::list({})).arr.size(), size_t(0), "empty");
    CHECK_EQ(b.calls(), size_t(0), "without a statement");
}

void a_long_key_list_is_read_in_chunks() {
    FakeBackend b;
    std::vector<hs::Val> ids;
    for (int i = 1; i <= 1000; i++) ids.push_back(hs::Val::int_(i));
    (void)hs::orm_find_many(&user_model(), &b, hs::Val::list(ids));
    CHECK_EQ(b.calls(), size_t(2), "two statements");
    CHECK(b.last_sql().find("IN (") != std::string::npos, "an IN query");
}

}  // namespace
}  // namespace qa

int main() {
    qa::created_records_come_back_with_keys_in_order();
    qa::an_empty_batch_inserts_nothing_and_opens_nothing();
    qa::one_batch_is_one_transaction();
    qa::a_bad_record_fails_the_whole_batch();
    qa::a_non_object_is_not_a_record();

    qa::updated_records_come_back_updated();
    qa::alike_records_share_one_statement();
    qa::unalike_records_do_not_share();
    qa::a_record_without_a_key_cannot_be_updated();
    qa::a_key_only_record_is_left_alone();

    qa::deleted_rows_are_counted();
    qa::a_long_id_list_is_cut_into_chunks();

    qa::found_rows_come_back_in_key_order();
    qa::an_empty_key_list_reads_nothing();
    qa::a_long_key_list_is_read_in_chunks();

    return qa::report("batch");
}
