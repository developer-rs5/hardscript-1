// Relationship reads (M5.3.4).
//
// A relationship is a query whose key comes off a record rather than off the
// source, so the interesting part is which columns get compared. Getting that
// wrong does not fail loudly: it returns the wrong rows. So every kind is
// pinned to an exact statement, and the ways a parent can fail to carry its
// key are checked too.
#include "test_support.hpp"

namespace {

/// `model User { id, email, posts : Post @has_many }` and
/// `model Post { id, title, user_id, user : User @belongs_to }`.
const hs::OrmModel& user_model() {
    static hs::OrmModel m("User", "user", "id", {"id", "email"}, {hs::OrmKind::Int, hs::OrmKind::Text}, {}, "");
    return m;
}
const hs::OrmModel& post_model() {
    static hs::OrmModel m("Post", "post", "id", {"id", "title", "user_id"},
                          {hs::OrmKind::Int, hs::OrmKind::Text, hs::OrmKind::Int}, {}, "");
    return m;
}

/// `model Tag { id, name }`, the far side of a `many_to_many`.
const hs::OrmModel& tag_model() {
    static hs::OrmModel m("Tag", "tag", "id", {"id", "name"}, {hs::OrmKind::Int, hs::OrmKind::Text}, {}, "");
    return m;
}

hs::Val obj(std::vector<std::pair<std::string, hs::Val>> kvs) { return hs::Val::object(std::move(kvs)); }

/// A query lists the model's own columns rather than `*`, so that a column
/// added to the model is a column the rows carry.
std::string post_sel() { return "SELECT \"id\", \"title\", \"user_id\" FROM \"post\""; }
std::string user_sel() { return "SELECT \"id\", \"email\" FROM \"user\""; }
hs::Val user(int64_t id) { return obj({{"id", hs::Val::int_(id)}, {"email", hs::Val::text("a@b.c")}}); }
hs::Val post(int64_t id) { return obj({{"id", hs::Val::int_(id)}, {"title", hs::Val::text("hi")}}); }

void a_has_many_matches_the_child_key_against_the_parent_key() {
    qa::FakeBackend b;
    b.canned.columns = {"id", "title", "user_id"};
    b.canned.rows = {{hs::Val::int_(2), hs::Val::text("hi"), hs::Val::int_(7)}};
    const hs::Val out = hs::OrmQuery(&post_model()).of_record(user(7), "id", "user_id").all(&b);
    CHECK_EQ(b.last_sql(), post_sel() + " WHERE \"user_id\" = ?",
             "the child's key column against the parent's");
    CHECK_EQ(b.params[0].size(), size_t(1), "one value");
    if (!b.params[0].empty()) CHECK_EQ(b.params[0][0].iv, int64_t(7), "read off the record");
    const hs::Val* title = out.is_arr() && !out.arr.empty() ? out.arr[0].find("title") : nullptr;
    CHECK(title != nullptr, "and the rows come back");
    if (title) CHECK_EQ(title->sv, std::string("hi"), "with their values");
}

void a_belongs_to_matches_the_other_way_round() {
    qa::FakeBackend b;
    b.canned.columns = {"id", "email"};
    b.canned.rows = {{hs::Val::int_(7), hs::Val::text("a@b.c")}};
    (void)hs::OrmQuery(&user_model()).of_record(obj({{"id", hs::Val::int_(2)}, {"user_id", hs::Val::int_(7)}}), "user_id", "id").first(&b);
    CHECK_EQ(b.last_sql(), user_sel() + " WHERE \"id\" = ? LIMIT 1",
             "the parent's key against the child's foreign key");
}

void a_belongs_to_binds_the_foreign_key_the_record_carries() {
    qa::FakeBackend b;
    const hs::Val p = obj({{"id", hs::Val::int_(2)}, {"user_id", hs::Val::int_(7)}});
    (void)hs::OrmQuery(&user_model()).of_record(p, "user_id", "id").first(&b);
    CHECK_EQ(b.params[0][0].iv, int64_t(7), "the key the child row carries");
}

void a_relationship_reads_its_own_key_not_the_parents() {
    // The two directions use the same machinery with the columns swapped, so
    // this pins that nothing is hard-coded to one shape.
    qa::FakeBackend b;
    (void)hs::OrmQuery(&user_model()).of_record(obj({{"user_id", hs::Val::text("k")}}), "user_id", "id").first(&b);
    CHECK_EQ(b.last_sql(), user_sel() + " WHERE \"id\" = ? LIMIT 1", "the target's key column");
    CHECK_EQ(b.params[0][0].sv, std::string("k"), "and a string key works as well as a number");
}

void a_relationship_with_a_where_binds_both_values() {
    qa::FakeBackend b;
    (void)hs::OrmQuery(&post_model())
        .of_record(user(7), "id", "user_id")
        .where_eq("title", hs::Val::text("hi"))
        .first(&b);
    CHECK_EQ(b.last_sql(), post_sel() + " WHERE \"user_id\" = ? AND \"title\" = ? LIMIT 1",
             "the relationship's predicate comes first, so its value binds first");
    CHECK_EQ(b.params[0].size(), size_t(2), "both values");
    if (b.params[0].size() == 2) {
        CHECK_EQ(b.params[0][0].iv, int64_t(7), "the parent's key");
        CHECK_EQ(b.params[0][1].sv, std::string("hi"), "then the caller's value");
    }
}

void a_relationship_takes_order_and_limit() {
    qa::FakeBackend b;
    (void)hs::OrmQuery(&post_model())
        .of_record(user(7), "id", "user_id")
        .order_by("title", true)
        .limit(3)
        .all(&b);
    CHECK_EQ(b.last_sql(), post_sel() + " WHERE \"user_id\" = ? ORDER BY \"title\" DESC LIMIT 3",
             "a relationship is an ordinary query once its key is bound");
}

void a_record_without_the_key_column_matches_nothing() {
    // A record that came from a narrower SELECT, or from a hand-built map, has
    // no key to compare. That is an empty result, not a wrong one: binding the
    // nil and comparing `= NULL` is never true, which is the honest answer.
    qa::FakeBackend b;
    (void)hs::OrmQuery(&post_model()).of_record(obj({{"title", hs::Val::text("hi")}}), "id", "user_id").all(&b);
    CHECK_EQ(b.last_sql(), post_sel() + " WHERE 1 = 0",
             "a false predicate, so no key can match by accident");
    CHECK_EQ(b.params[0].size(), size_t(0), "and nothing is bound for it");
}

void a_join_is_emitted_between_the_table_and_the_where() {
    qa::FakeBackend b;
    (void)hs::OrmQuery(&post_model())
        .of_record(user(7), "id", "user_id", "INNER JOIN \"tag_map\" ON \"tag_map\".\"post_id\" = \"post\".\"id\"")
        .all(&b);
    CHECK_EQ(b.last_sql(),
             post_sel() + " INNER JOIN \"tag_map\" ON \"tag_map\".\"post_id\" = \"post\".\"id\" "
                          "WHERE \"user_id\" = ?",
             "a join is part of the FROM, and the key still compares");
}

void a_qualified_column_in_the_join_is_quoted_per_part() {
    qa::FakeBackend b;
    (void)hs::OrmQuery(&post_model())
        .of_record(user(7), "id", "post_tag.post_id", "INNER JOIN \"post_tag\" ON 1 = 1")
        .all(&b);
    CHECK_EQ(b.last_sql(),
             post_sel() + " INNER JOIN \"post_tag\" ON 1 = 1 WHERE \"post_tag\".\"post_id\" = ?",
             "a qualified name is two identifiers, not one name with a dot in it");
}

void a_qualified_order_column_is_quoted_the_same_way() {
    qa::FakeBackend b;
    (void)hs::OrmQuery(&post_model())
        .of_record(user(7), "id", "user_id", "INNER JOIN \"u\" ON 1 = 1")
        .order_by("u.name", false)
        .all(&b);
    CHECK(b.last_sql().find("ORDER BY \"u\".\"name\"") != std::string::npos, "quoted per part as well");
}

void a_relationship_count_joins_the_same_way() {
    qa::FakeBackend b;
    b.canned.columns = {"c"};
    b.canned.rows = {{hs::Val::int_(4)}};
    const hs::Val n = hs::OrmQuery(&post_model()).of_record(user(7), "id", "user_id").count(&b);
    CHECK_EQ(b.last_sql(), std::string("SELECT COUNT(*) FROM \"post\" WHERE \"user_id\" = ?"),
             "a count describes the same set a page of rows would");
    CHECK(n.is_int() && n.iv == 4, "and reads the number back");
}

void a_relationship_exists_is_a_count_of_one() {
    qa::FakeBackend b;
    b.canned.columns = {"id", "title", "user_id"};
    b.canned.rows = {{hs::Val::int_(2), hs::Val::text("hi"), hs::Val::int_(7)}};
    const hs::Val yes = hs::OrmQuery(&post_model()).of_record(user(7), "id", "user_id").exists(&b);
    CHECK(yes.is_bool() && yes.bv, "true when a row matched");
    CHECK(b.last_sql().find("LIMIT 1") != std::string::npos, "and it stops at one row");

    qa::FakeBackend none;
    const hs::Val no = hs::OrmQuery(&post_model()).of_record(user(7), "id", "user_id").exists(&none);
    CHECK(no.is_bool() && !no.bv, "false when none did");
}

void a_relationship_first_limits_to_one_row() {
    qa::FakeBackend b;
    (void)hs::OrmQuery(&post_model()).of_record(user(7), "id", "user_id").first(&b);
    CHECK_EQ(b.last_sql(), post_sel() + " WHERE \"user_id\" = ? LIMIT 1",
             "one row, and the statement says so");
}

void placeholders_number_from_the_relationship_value() {
    qa::FakeBackend pg;
    pg.d = hs::DbDialect::Postgres;
    (void)hs::OrmQuery(&post_model())
        .of_record(user(7), "id", "user_id")
        .where_gt("id", hs::Val::int_(3))
        .all(&pg);
    CHECK_EQ(pg.last_sql(), post_sel() + " WHERE \"user_id\" = $1 AND \"id\" > $2",
             "the relationship's value is the first bound value, on either dialect");
}

void a_relationship_carries_no_value_into_the_sql_text() {
    // The SQL is fixed by the schema and the plan; the record only supplies a
    // value. So the same query text is produced whatever the record holds.
    qa::FakeBackend a;
    (void)hs::OrmQuery(&post_model()).of_record(user(7), "id", "user_id").all(&a);
    qa::FakeBackend c;
    (void)hs::OrmQuery(&post_model()).of_record(user(99), "id", "user_id").all(&c);
    CHECK_EQ(a.last_sql(), c.last_sql(), "the statement does not depend on the value");
    CHECK_EQ(a.params[0][0].iv, int64_t(7), "only the bound value does");
    CHECK_EQ(c.params[0][0].iv, int64_t(99), "for this record");
}

void a_two_step_relationship_keeps_its_own_join() {
    // Reading a relationship off a relationship: the record is itself the
    // result of another read, so its key is the target's primary key.
    qa::FakeBackend b;
    (void)hs::OrmQuery(&user_model())
        .of_record(post(2), "id", "post_id", "INNER JOIN \"post\" ON \"post\".\"id\" = \"user\".\"id\"")
        .all(&b);
    CHECK_EQ(b.last_sql(),
             user_sel() + " INNER JOIN \"post\" ON \"post\".\"id\" = \"user\".\"id\" WHERE \"post_id\" = ?",
             "a chain of reads composes");
}

}  // namespace

// ---- the value a relationship binds ----------------------------------------

void a_join_selects_the_target_columns_and_not_the_join_tables() {
    // `SELECT *` across a join would return the junction's columns too, and
    // two `id`s would land in one row with the second one winning. The columns
    // listed are the target's, whatever the join brought in.
    qa::FakeBackend b;
    const std::string join = "INNER JOIN \"post_tag\" ON \"post_tag\".\"tag_id\" = \"tag\".\"id\"";
    (void)hs::OrmQuery(&tag_model()).of_record(post(3), "id", "post_tag.post_id", join).all(&b);
    CHECK_EQ(b.last_sql(),
             "SELECT \"id\", \"name\" FROM \"tag\" " + join + " WHERE \"post_tag\".\"post_id\" = ?",
             "the target's own columns, and the junction's only in the predicate");
}

void a_numeric_key_binds_as_a_number() {
    // Whatever the column on the far side is declared as, the value comes off a
    // record, so the record's own type is what has to be sent. An `id : Int`
    // compared as text is what makes Postgres refuse a query that SQLite
    // quietly accepts.
    qa::FakeBackend b;
    (void)hs::OrmQuery(&post_model()).of_record(user(7), "id", "user_id").all(&b);
    CHECK_EQ(b.params[0].size(), size_t(1), "one value");
    CHECK(b.params[0][0].is_int(), "bound as the number it is, not as its text");
    CHECK_EQ(b.params[0][0].iv, int64_t(7), "and it is the record's own value");
}

void a_text_key_binds_as_text() {
    qa::FakeBackend b;
    (void)hs::OrmQuery(&post_model()).of_record(obj({{"code", hs::Val::text("ab")}}), "code", "code").all(&b);
    CHECK(b.params[0][0].is_str(), "a text key stays text");
    CHECK_EQ(b.params[0][0].sv, std::string("ab"), "unchanged");
}

void a_float_key_binds_as_a_number() {
    qa::FakeBackend b;
    (void)hs::OrmQuery(&post_model()).of_record(obj({{"ratio", hs::Val::flt(1.5)}}), "ratio", "ratio").all(&b);
    CHECK(b.params[0][0].is_flt() && b.params[0][0].fv == 1.5, "a float key stays the float it is");
}

int main() {
    a_has_many_matches_the_child_key_against_the_parent_key();
    a_belongs_to_matches_the_other_way_round();
    a_belongs_to_binds_the_foreign_key_the_record_carries();
    a_relationship_reads_its_own_key_not_the_parents();
    a_relationship_with_a_where_binds_both_values();
    a_relationship_takes_order_and_limit();
    a_record_without_the_key_column_matches_nothing();
    a_join_is_emitted_between_the_table_and_the_where();
    a_qualified_column_in_the_join_is_quoted_per_part();
    a_qualified_order_column_is_quoted_the_same_way();
    a_relationship_count_joins_the_same_way();
    a_relationship_exists_is_a_count_of_one();
    a_relationship_first_limits_to_one_row();
    placeholders_number_from_the_relationship_value();
    a_relationship_carries_no_value_into_the_sql_text();
    a_two_step_relationship_keeps_its_own_join();
    a_join_selects_the_target_columns_and_not_the_join_tables();
    a_numeric_key_binds_as_a_number();
    a_text_key_binds_as_text();
    a_float_key_binds_as_a_number();
    return qa::report("relations");
}

