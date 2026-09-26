// The write path's statement assembly and results (M5.3.3).
//
// A write is the half of the ORM where a mistake is loudest: a wrong column
// list silently drops a value, and a key that gets rewritten moves a row. So
// the statements are pinned exactly, and the interesting cases -- a generated
// key, an upsert that has to insert, a save of something never stored -- each
// get their own check.
#include "test_support.hpp"

namespace {

/// A `User` with the metadata codegen emits: `id`, `age` and `updated` are the
/// database's to fill -- a key, a default and a timestamp -- and `updated` is
/// also the column `touch` writes. `email` and `name` have no default, so a
/// literal that is going to be inserted has to carry both.
const hs::OrmModel& crud_model() {
    static hs::OrmModel m("User", "user", "id", {"id", "email", "name", "age", "updated"},
                           {hs::OrmKind::Int, hs::OrmKind::Text, hs::OrmKind::Text, hs::OrmKind::Int,
                            hs::OrmKind::Time},
                           {"id", "age", "updated"}, "updated");
    return m;
}

hs::Val obj(std::vector<std::pair<std::string, hs::Val>> kvs) {
    return hs::Val::object(std::move(kvs));
}

/// A literal complete enough to insert.
hs::Val full() {
    return obj({{"email", hs::Val::text("a@b.c")}, {"name", hs::Val::text("Ada")}});
}

void create_lists_only_the_columns_it_has_values_for() {
    qa::FakeBackend b;
    b.canned.last_id = 12;
    const hs::Val rec = hs::orm_create(&crud_model(), &b, full());
    CHECK_EQ(b.last_sql(), std::string("INSERT INTO \"user\" (\"email\", \"name\") VALUES (?, ?)"),
             "the key and the defaulted age are left to the database");
    CHECK_EQ(b.params[0].size(), size_t(2), "one bound value per listed column");
    if (!b.params[0].empty()) CHECK_EQ(b.params[0][0].sv, std::string("a@b.c"), "the first value");
    const hs::Val* id = rec.find("id");
    CHECK(id != nullptr, "the created record has a key");
    if (id) CHECK_EQ(id->iv, int64_t(12), "read back from the database");
}

void a_key_the_record_carried_is_not_replaced() {
    // An upsert that inserts writes the key it was given, so the record it
    // returns has to carry that same key rather than whatever the backend
    // reported.
    qa::FakeBackend b;
    b.canned.last_id = 99;
    const hs::Val out =
        hs::orm_create(&crud_model(), &b, obj({{"id", hs::Val::int_(5)},
                                               {"email", hs::Val::text("a@b.c")},
                                               {"name", hs::Val::text("Ada")}}));
    const hs::Val* id = out.find("id");
    CHECK(id != nullptr, "the key is still there");
    if (id) CHECK_EQ(id->iv, int64_t(5), "and it is the one that was written");
}

void create_refuses_a_row_that_is_missing_a_value() {
    // The compiler catches this, but the runtime is also reachable from a
    // hand-written binding, so it must not build a statement that cannot
    // succeed.
    qa::FakeBackend b;
    bool threw = false;
    try {
        (void)hs::orm_create(&crud_model(), &b, obj({{"email", hs::Val::text("a@b.c")}}));
    } catch (const std::exception& e) {
        threw = true;
        CHECK(std::string(e.what()).find("no default") != std::string::npos, "and says why");
    }
    CHECK(threw, "a create with an unset required column is refused");
    CHECK_EQ(b.calls(), size_t(0), "and nothing was sent to the database");
}

void save_updates_every_column_the_record_carries() {
    qa::FakeBackend b;
    b.canned.affected = 1;
    const hs::Val rec =
        obj({{"id", hs::Val::int_(7)}, {"email", hs::Val::text("new@b.c")}, {"name", hs::Val::text("Ada")}});
    (void)hs::orm_save(&crud_model(), &b, rec);
    CHECK_EQ(b.last_sql(), std::string("UPDATE \"user\" SET \"email\" = ?, \"name\" = ? WHERE \"id\" = ?"),
             "only the fields present are written");
    CHECK_EQ(b.params[0].size(), size_t(3), "the key is bound after the values");
    if (b.params[0].size() == 3) CHECK_EQ(b.params[0][2].iv, int64_t(7), "as the WHERE");
}

void save_never_reassigns_the_key() {
    // A record whose only field is its key has nothing to write, so no
    // statement goes out at all rather than an `UPDATE` with an empty SET.
    qa::FakeBackend b;
    (void)hs::orm_save(&crud_model(), &b, obj({{"id", hs::Val::int_(7)}}));
    CHECK_EQ(b.calls(), size_t(0), "a save with nothing to change is not a statement");
}

void save_of_an_unstored_record_inserts_it() {
    // A record without a key has never been in a table, so saving it inserts.
    qa::FakeBackend b;
    b.canned.last_id = 3;
    (void)hs::orm_save(&crud_model(), &b, full());
    CHECK(b.last_sql().find("INSERT INTO") != std::string::npos, "no key means an insert");
}

void touch_writes_the_timestamp_column() {
    qa::FakeBackend b;
    (void)hs::orm_touch(&crud_model(), &b, obj({{"id", hs::Val::int_(7)}}));
    CHECK_EQ(b.last_sql(),
             std::string("UPDATE \"user\" SET \"updated\" = CURRENT_TIMESTAMP WHERE \"id\" = ?"),
             "sqlite sets the time itself");
    CHECK_EQ(b.params[0].size(), size_t(1), "and only the key is bound");

    qa::FakeBackend pg;
    pg.d = hs::DbDialect::Postgres;
    (void)hs::orm_touch(&crud_model(), &pg, obj({{"id", hs::Val::int_(7)}}));
    CHECK(pg.last_sql().find("NOW()") != std::string::npos, "postgres uses its own clock");
}

void touch_needs_a_record_it_can_identify() {
    qa::FakeBackend b;
    bool threw = false;
    try {
        (void)hs::orm_touch(&crud_model(), &b, obj({{"name", hs::Val::text("Ada")}}));
    } catch (const std::exception& e) {
        threw = true;
        CHECK(std::string(e.what()).find("no id") != std::string::npos, "naming the missing column");
    }
    CHECK(threw, "a record with no key cannot be touched");
}

void touch_says_so_when_the_model_has_no_timestamp() {
    static const hs::OrmModel plain("Plain", "plain", "id", {"id", "name"},
                                    {hs::OrmKind::Int, hs::OrmKind::Text}, {"id"}, "");
    qa::FakeBackend b;
    bool threw = false;
    try {
        (void)hs::orm_touch(&plain, &b, obj({{"id", hs::Val::int_(1)}}));
    } catch (const std::exception& e) {
        threw = true;
        CHECK(std::string(e.what()).find("@updated_at") != std::string::npos, "naming the attribute to add");
    }
    CHECK(threw, "touch needs somewhere to write the time");
}

void delete_reports_whether_a_row_went_away() {
    qa::FakeBackend b;
    b.canned.affected = 1;
    const hs::Val hit = hs::orm_delete_key(&crud_model(), &b, hs::Val::int_(4));
    CHECK_EQ(b.last_sql(), std::string("DELETE FROM \"user\" WHERE \"id\" = ?"), "delete by key");
    CHECK(hit.is_bool() && hit.bv, "a row went away");

    qa::FakeBackend gone;
    gone.canned.affected = 0;
    const hs::Val miss = hs::orm_delete_key(&crud_model(), &gone, hs::Val::int_(4));
    CHECK(miss.is_bool() && !miss.bv, "deleting something already gone is false, not an error");
}

void a_record_can_delete_itself() {
    qa::FakeBackend b;
    b.canned.affected = 1;
    const hs::Val r = hs::orm_delete(&crud_model(), &b, obj({{"id", hs::Val::int_(4)}}));
    CHECK_EQ(b.last_sql(), std::string("DELETE FROM \"user\" WHERE \"id\" = ?"), "the record's own key");
    CHECK(r.is_bool() && r.bv, "and it reports the outcome");
}

void upsert_updates_when_the_row_is_there() {
    qa::FakeBackend b;
    b.canned.affected = 1;
    const hs::Val out = hs::orm_upsert(
        &crud_model(), &b, obj({{"id", hs::Val::int_(1)}, {"name", hs::Val::text("Ada")}}));
    CHECK_EQ(b.calls(), size_t(1), "one statement, not two");
    CHECK(b.last_sql().find("UPDATE") != std::string::npos, "an update when the row exists");
    CHECK(b.last_sql().find("\"id\" = ?") != std::string::npos, "the key is the WHERE");
    CHECK(out.is_obj(), "and the record comes back");
}

void upsert_inserts_when_it_is_not() {
    // This is the reason an upsert takes an explicit key: the update misses,
    // and the insert has to write that same key rather than let a sequence
    // pick a different one.
    qa::FakeBackend b;
    b.canned.affected = 0;
    b.canned.last_id = 1;
    (void)hs::orm_upsert(
        &crud_model(), &b,
        obj({{"id", hs::Val::int_(1)}, {"email", hs::Val::text("a@b.c")}, {"name", hs::Val::text("Ada")}}));
    CHECK_EQ(b.calls(), size_t(2), "an update that misses, then an insert");
    CHECK(b.sqls[0].find("UPDATE") != std::string::npos, "first the update");
    CHECK(b.sqls[1].find("INSERT INTO") != std::string::npos, "then the insert");
    CHECK(b.sqls[1].find("\"id\"") != std::string::npos, "which writes the key it was given");
}

void upsert_without_a_key_inserts() {
    qa::FakeBackend b;
    b.canned.last_id = 5;
    (void)hs::orm_upsert(&crud_model(), &b, full());
    CHECK_EQ(b.calls(), size_t(1), "one insert");
    CHECK(b.sqls[0].find("\"id\"") == std::string::npos, "and no key column");
}

void find_or_create_reads_before_it_writes() {
    qa::FakeBackend b;
    b.canned.columns = {"id", "email", "name", "age", "updated"};
    b.canned.rows = {
        {hs::Val::int_(1), hs::Val::text("a@b.c"), hs::Val::text("Ada"), hs::Val::int_(18), hs::Val::nil()}};
    const hs::Val found = hs::orm_find_or_create(&crud_model(), &b, full());
    CHECK_EQ(b.calls(), size_t(1), "one select");
    CHECK(b.sqls[0].find("SELECT") != std::string::npos, "which is a select");
    CHECK(b.sqls[0].find("\"email\" = ?") != std::string::npos, "matching the attribute");
    const hs::Val* id = found.find("id");
    if (id) CHECK_EQ(id->iv, int64_t(1), "and the existing row is returned");
}

void find_or_create_writes_when_nothing_matched() {
    qa::FakeBackend b;
    b.canned.last_id = 8;
    const hs::Val made = hs::orm_find_or_create(&crud_model(), &b, full());
    CHECK_EQ(b.calls(), size_t(2), "a select, then an insert");
    CHECK(b.sqls[1].find("INSERT INTO") != std::string::npos, "the insert");
    const hs::Val* id = made.find("id");
    if (id) CHECK_EQ(id->iv, int64_t(8), "and its key is read back");
}

void find_or_create_matches_every_attribute_it_was_given() {
    qa::FakeBackend b;
    (void)hs::orm_find_or_create(&crud_model(), &b, full());
    CHECK(b.sqls[0].find("\"email\" = ? AND \"name\" = ?") != std::string::npos,
          "a conjunction, so the row is one the caller would have found");
}

void find_or_create_rejects_an_attribute_that_is_not_a_column() {
    qa::FakeBackend b;
    bool threw = false;
    try {
        (void)hs::orm_find_or_create(&crud_model(), &b, obj({{"nope", hs::Val::int_(1)}}));
    } catch (const std::exception& e) {
        threw = true;
        CHECK(std::string(e.what()).find("no field nope") != std::string::npos, "naming the field");
    }
    CHECK(threw, "an unknown attribute is an error, not a query");
}

void every_written_column_comes_from_the_schema() {
    // A value can never introduce a column name: the record decides which
    // columns are present, the model decides what they are called.
    qa::FakeBackend b;
    const hs::Val rec = obj({{"id", hs::Val::int_(1)},
                             {"email", hs::Val::text("a@b.c")},
                             {"'; DROP TABLE user; --", hs::Val::text("x")}});
    (void)hs::orm_save(&crud_model(), &b, rec);
    CHECK(b.last_sql().find("DROP TABLE") == std::string::npos, "an unknown field is never written");
    CHECK_EQ(b.last_sql(), std::string("UPDATE \"user\" SET \"email\" = ? WHERE \"id\" = ?"),
             "only the model's own columns are named");
}

void placeholders_number_per_dialect_on_writes_too() {
    qa::FakeBackend pg;
    pg.d = hs::DbDialect::Postgres;
    pg.canned.affected = 1;
    (void)hs::orm_save(&crud_model(), &pg,
                       obj({{"id", hs::Val::int_(1)}, {"email", hs::Val::text("a@b.c")}}));
    CHECK_EQ(pg.last_sql(), std::string("UPDATE \"user\" SET \"email\" = $1 WHERE \"id\" = $2"),
             "postgres numbers from one");
}

void a_backend_failure_during_a_write_reaches_the_caller() {
    qa::FakeBackend b;
    b.throw_on_run = true;
    bool threw = false;
    try {
        (void)hs::orm_create(&crud_model(), &b, full());
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "a failed insert throws rather than returning a record that was never written");
}

}  // namespace

int main() {
    // A trace of which step throws, without disturbing the assertions.
    create_lists_only_the_columns_it_has_values_for();
    a_key_the_record_carried_is_not_replaced();
    create_refuses_a_row_that_is_missing_a_value();
    save_updates_every_column_the_record_carries();
    save_never_reassigns_the_key();
    save_of_an_unstored_record_inserts_it();
    touch_writes_the_timestamp_column();
    touch_needs_a_record_it_can_identify();
    touch_says_so_when_the_model_has_no_timestamp();
    delete_reports_whether_a_row_went_away();
    a_record_can_delete_itself();
    upsert_updates_when_the_row_is_there();
    upsert_inserts_when_it_is_not();
    upsert_without_a_key_inserts();
    find_or_create_reads_before_it_writes();
    find_or_create_writes_when_nothing_matched();
    find_or_create_matches_every_attribute_it_was_given();
    find_or_create_rejects_an_attribute_that_is_not_a_column();
    every_written_column_comes_from_the_schema();
    placeholders_number_per_dialect_on_writes_too();
    a_backend_failure_during_a_write_reaches_the_caller();
    return qa::report("crud");
}
