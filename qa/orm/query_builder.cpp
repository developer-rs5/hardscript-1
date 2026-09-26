// The query builder's SQL assembly (M5.3.2).
//
// These assertions are on exact statement text. A query builder is the layer
// where quoting, placeholder numbering and clause order go wrong in ways that
// still "work" against a permissive database, so the text is pinned here.
#include "test_support.hpp"

#include <utility>
#include <vector>

namespace {

void selects_every_column_by_default() {
    qa::DialectOnly b(hs::DbDialect::Sqlite);
    hs::OrmQuery q(&qa::user_model());
    CHECK_EQ(q.sql(&b), std::string("SELECT \"id\", \"email\", \"name\", \"age\" FROM \"user\""),
             "all() lists the model's columns in registration order");
}

void a_where_clause_becomes_a_bound_condition() {
    qa::DialectOnly b(hs::DbDialect::Sqlite);
    hs::OrmQuery q(&qa::user_model());
    q.where_eq("email", hs::Val::text("a@b.c"));
    CHECK_EQ(q.sql(&b),
             std::string("SELECT \"id\", \"email\", \"name\", \"age\" FROM \"user\" "
                         "WHERE \"email\" = ?"),
             "a condition is appended after the table");
}

void conditions_are_anded_in_order() {
    qa::DialectOnly b(hs::DbDialect::Sqlite);
    hs::OrmQuery q(&qa::user_model());
    q.where_gt("age", hs::Val::int_(20)).where_eq("email", hs::Val::text("a@b.c"));
    CHECK_EQ(q.sql(&b),
             std::string("SELECT \"id\", \"email\", \"name\", \"age\" FROM \"user\" "
                         "WHERE \"age\" > ? AND \"email\" = ?"),
             "multiple conditions are ANDed in the order they were added");
}

void every_comparison_operator_has_its_own_sql() {
    qa::DialectOnly b(hs::DbDialect::Sqlite);
    // `<>` rather than `!=`, which SQL has no use for.
    const std::vector<std::pair<const char*, std::string>> cases = {
        {"where_eq", "="},   {"where_ne", "<>"}, {"where_lt", "<"},
        {"where_le", "<="},  {"where_gt", ">"},   {"where_ge", ">="},
    };
    for (const auto& c : cases) {
        hs::OrmQuery q(&qa::user_model());
        if (c.first == std::string("where_eq")) q.where_eq("age", hs::Val::int_(1));
        else if (c.first == std::string("where_ne")) q.where_ne("age", hs::Val::int_(1));
        else if (c.first == std::string("where_lt")) q.where_lt("age", hs::Val::int_(1));
        else if (c.first == std::string("where_le")) q.where_le("age", hs::Val::int_(1));
        else if (c.first == std::string("where_gt")) q.where_gt("age", hs::Val::int_(1));
        else q.where_ge("age", hs::Val::int_(1));
        std::string want = "SELECT \"id\", \"email\", \"name\", \"age\" FROM \"user\" "
                           "WHERE \"age\" " + c.second + " ?";
        CHECK_EQ(q.sql(&b), want, c.first);
    }
}

void like_differs_between_dialects() {
    qa::DialectOnly lite(hs::DbDialect::Sqlite);
    qa::DialectOnly pg(hs::DbDialect::Postgres);
    hs::OrmQuery a(&qa::user_model());
    a.where_like("name", hs::Val::text("%a%"));
    CHECK(a.sql(&lite).find(" LIKE ") != std::string::npos, "sqlite uses LIKE");
    hs::OrmQuery b(&qa::user_model());
    b.where_like("name", hs::Val::text("%a%"));
    CHECK(b.sql(&pg).find(" ILIKE ") != std::string::npos, "postgres uses ILIKE");
}

void in_expands_one_placeholder_per_value() {
    qa::DialectOnly b(hs::DbDialect::Sqlite);
    hs::OrmQuery q(&qa::user_model());
    q.where_in("id", hs::Val::list({hs::Val::int_(1), hs::Val::int_(2), hs::Val::int_(3)}));
    CHECK_EQ(q.sql(&b),
             std::string("SELECT \"id\", \"email\", \"name\", \"age\" FROM \"user\" "
                         "WHERE \"id\" IN (?, ?, ?)"),
             "each list element gets its own placeholder");
}

void an_empty_in_list_matches_nothing() {
    qa::DialectOnly b(hs::DbDialect::Sqlite);
    hs::OrmQuery q(&qa::user_model());
    q.where_in("id", hs::Val::list({}));
    // `IN ()` is a syntax error on both backends; the query has to become a
    // false predicate instead.
    CHECK_EQ(q.sql(&b),
             std::string("SELECT \"id\", \"email\", \"name\", \"age\" FROM \"user\" WHERE 1 = 0"),
             "an empty IN list is a query for nothing, not a syntax error");
    CHECK_EQ(q.bound_values().size(), size_t(0), "and it binds no values");
}

void a_single_value_in_is_one_element_membership() {
    qa::DialectOnly b(hs::DbDialect::Sqlite);
    hs::OrmQuery q(&qa::user_model());
    q.where_in("id", hs::Val::int_(7));
    CHECK_EQ(q.sql(&b),
             std::string("SELECT \"id\", \"email\", \"name\", \"age\" FROM \"user\" "
                         "WHERE \"id\" IN (?)"),
             "a scalar is treated as a one-element list");
}

void placeholders_are_numbered_for_postgres() {
    qa::DialectOnly b(hs::DbDialect::Postgres);
    hs::OrmQuery q(&qa::user_model());
    q.where_gt("age", hs::Val::int_(20)).where_in("id", hs::Val::list({hs::Val::int_(1), hs::Val::int_(2)}));
    CHECK_EQ(q.sql(&b),
             std::string("SELECT \"id\", \"email\", \"name\", \"age\" FROM \"user\" "
                         "WHERE \"age\" > $1 AND \"id\" IN ($2, $3)"),
             "postgres placeholders count up from one");
}

void order_by_is_directional_and_ordered() {
    qa::DialectOnly b(hs::DbDialect::Sqlite);
    hs::OrmQuery q(&qa::user_model());
    q.order_by("name", false).order_by("age", true);
    CHECK_EQ(q.sql(&b),
             std::string("SELECT \"id\", \"email\", \"name\", \"age\" FROM \"user\" "
                         "ORDER BY \"name\", \"age\" DESC"),
             "ascending is the default and DESC is explicit");
}

void limit_and_offset_keep_sqlite_happy() {
    qa::DialectOnly lite(hs::DbDialect::Sqlite);
    hs::OrmQuery a(&qa::user_model());
    a.limit(10).offset(20);
    CHECK(a.sql(&lite).find("LIMIT 10 OFFSET 20") != std::string::npos, "limit then offset");

    // SQLite rejects a bare OFFSET, so it needs a LIMIT of -1 in front.
    hs::OrmQuery b(&qa::user_model());
    b.offset(20);
    CHECK(b.sql(&lite).find("LIMIT -1 OFFSET 20") != std::string::npos, "sqlite offset needs a limit");

    qa::DialectOnly pg(hs::DbDialect::Postgres);
    hs::OrmQuery c(&qa::user_model());
    c.offset(20);
    CHECK_EQ(c.sql(&pg),
             std::string("SELECT \"id\", \"email\", \"name\", \"age\" FROM \"user\" OFFSET 20"),
             "postgres allows a bare offset");
}

void values_never_reach_the_statement_text() {
    qa::DialectOnly b(hs::DbDialect::Sqlite);
    hs::OrmQuery q(&qa::user_model());
    // A value that would end a statement if it were ever interpolated.
    q.where_eq("name", hs::Val::text("'; DROP TABLE user; --"));
    const std::string sql = q.sql(&b);
    CHECK(sql.find("DROP TABLE") == std::string::npos,
          "a value is bound, never interpolated, so it cannot end the statement");
    CHECK(sql.find("?") != std::string::npos, "and a placeholder stands in for it");
}

void bound_values_follow_placeholder_order() {
    hs::OrmQuery q(&qa::user_model());
    q.where_gt("age", hs::Val::int_(20))
        .where_eq("email", hs::Val::text("a@b.c"))
        .where_in("id", hs::Val::list({hs::Val::int_(7), hs::Val::int_(8)}));
    const std::vector<std::string> v = q.bound_values();
    CHECK_EQ(v.size(), size_t(4), "one bound value per placeholder");
    CHECK_EQ(v[0], std::string("20"), "first condition");
    CHECK_EQ(v[1], std::string("a@b.c"), "second condition");
    CHECK_EQ(v[2], std::string("7"), "first list element");
    CHECK_EQ(v[3], std::string("8"), "second list element");
}

void a_narrowed_projection_replaces_the_column_list() {
    hs::OrmQuery q(&qa::user_model());
    std::vector<std::string> bound;
    CHECK_EQ(q.build_select(hs::DbDialect::Sqlite, {"id", "name"}, bound),
             std::string("SELECT \"id\", \"name\" FROM \"user\""),
             "a narrowed projection is listed in the order given");
}

void count_counts_the_same_set_the_rows_come_from() {
    qa::FakeBackend b;
    b.canned.columns = {"count"};
    b.canned.rows = {{hs::Val::int_(42)}};
    hs::OrmQuery q(&qa::user_model());
    q.where_gt("age", hs::Val::int_(20));
    const hs::Val n = q.count(&b);
    CHECK_EQ(b.last_sql(), std::string("SELECT COUNT(*) FROM \"user\" WHERE \"age\" > ?"),
             "count reuses the predicates and ignores ordering and paging");
    CHECK(n.is_int() && n.iv == 42, "the count comes back as a number");
}

void count_of_an_empty_result_is_zero() {
    qa::FakeBackend b;
    const hs::Val n = hs::OrmQuery(&qa::user_model()).count(&b);
    CHECK(n.is_int() && n.iv == 0, "no rows means zero, not a null");
}

void count_reads_a_text_number_too() {
    // SQLite hands COUNT(*) back as an integer, but a text-affinity column or
    // a driver that stringifies would still have to be understood.
    qa::FakeBackend b;
    b.canned.columns = {"count"};
    b.canned.rows = {{hs::Val::text("7")}};
    const hs::Val n = hs::OrmQuery(&qa::user_model()).count(&b);
    CHECK(n.is_int() && n.iv == 7, "a numeric string is still a count");
}

void first_asks_for_one_row_and_reports_absence() {
    qa::FakeBackend b;
    b.canned.columns = {"id", "email", "name", "age"};
    b.canned.rows = {{hs::Val::int_(3), hs::Val::text("a@b.c"), hs::Val::text("Ada"), hs::Val::int_(36)}};
    hs::OrmQuery q(&qa::user_model());
    q.order_by("age", false);
    hs::Val one = q.first(&b);
    CHECK(b.last_sql().find("LIMIT 1") != std::string::npos, "first() is a LIMIT 1 query");
    CHECK(b.last_sql().find("ORDER BY") != std::string::npos, "and it keeps the ordering");
    CHECK(one.is_obj(), "a row is an object");
    if (one.is_obj()) {
        const std::string email = one.get("email").sv;
        const int64_t age = one.get("age").iv;
        CHECK_EQ(email, std::string("a@b.c"), "columns are keyed by name");
        CHECK_EQ(age, int64_t(36), "typed per the model's column kind");
    }

    qa::FakeBackend empty;
    CHECK(hs::OrmQuery(&qa::user_model()).first(&empty).is_nil(), "no row means nil, not an empty object");
}

void first_does_not_clobber_an_explicit_limit() {
    qa::FakeBackend b;
    b.canned.columns = {"id"};
    b.canned.rows = {{hs::Val::int_(1)}};
    hs::OrmQuery q(&qa::user_model());
    q.limit(5);
    q.first(&b);
    // The caller asked for five rows; taking the first is a C++ concern and
    // must not rewrite their limit into the statement.
    CHECK(b.last_sql().find("LIMIT 5") != std::string::npos, "an explicit limit is left alone");
}

void exists_stops_at_the_first_row() {
    qa::FakeBackend b;
    b.canned.columns = {"id"};
    b.canned.rows = {{hs::Val::int_(1)}};
    const hs::Val e = hs::OrmQuery(&qa::user_model()).where_eq("email", hs::Val::text("a@b.c")).exists(&b);
    CHECK(b.last_sql().find("LIMIT 1") != std::string::npos, "exists is a LIMIT 1 query");
    CHECK(b.last_sql().find("COUNT") == std::string::npos, "and not a count");
    CHECK(e.is_bool() && e.bv, "a matching row means true");

    qa::FakeBackend none;
    const hs::Val f = hs::OrmQuery(&qa::user_model()).exists(&none);
    CHECK(f.is_bool() && !f.bv, "no matching row means false");
}

void all_returns_one_object_per_row() {
    qa::FakeBackend b;
    b.canned.columns = {"id", "email", "name", "age"};
    b.canned.rows = {
        {hs::Val::int_(1), hs::Val::text("a@b.c"), hs::Val::text("Ada"), hs::Val::int_(36)},
        {hs::Val::int_(2), hs::Val::text("d@e.f"), hs::Val::text("Bob"), hs::Val::int_(41)},
    };
    hs::Val rows = hs::OrmQuery(&qa::user_model()).all(&b);
    CHECK(rows.is_arr(), "a result set is a list");
    if (rows.is_arr()) {
        CHECK_EQ(rows.arr.size(), size_t(2), "one entry per row");
        const std::string name = rows.arr[1].get("name").sv;
        CHECK_EQ(name, std::string("Bob"), "in the order the database gave");
    }
}

void all_of_nothing_is_an_empty_list() {
    qa::FakeBackend b;
    hs::Val rows = hs::OrmQuery(&qa::user_model()).all(&b);
    CHECK(rows.is_arr() && rows.arr.empty(), "no rows is an empty list, not a null");
}

void values_are_passed_to_the_backend_not_only_baked_into_text() {
    qa::FakeBackend b;
    hs::OrmQuery q(&qa::user_model());
    q.where_eq("email", hs::Val::text("a@b.c"));
    q.all(&b);
    CHECK_EQ(b.params.size(), size_t(1), "one parameter vector per statement");
    CHECK_EQ(b.params[0].size(), size_t(1), "one value for the one placeholder");
    if (!b.params[0].empty()) CHECK_EQ(b.params[0][0].sv, std::string("a@b.c"), "and it is the value asked for");
}

void a_backend_error_reaches_the_caller() {
    qa::FakeBackend b;
    b.throw_on_run = true;
    bool threw = false;
    try {
        hs::OrmQuery(&qa::user_model()).all(&b);
    } catch (const std::exception& e) {
        threw = true;
        CHECK(std::string(e.what()).find("run failed") != std::string::npos, "the backend's message survives");
    }
    CHECK(threw, "a failing query throws rather than returning an empty result");
}

void without_a_database_the_query_says_so() {
    qa::FakeBackend b;
    hs::db_set(&b);
    CHECK(hs::db_need() == &b, "the ambient connection is the one that was set");
    hs::db_set(nullptr);
    bool threw = false;
    try {
        hs::db_need();
    } catch (const std::exception& e) {
        threw = true;
        CHECK(std::string(e.what()).find("no database is open") != std::string::npos,
              "with a clear message");
    }
    CHECK(threw, "an unopened database is an error, not a crash");
}

void an_identifier_with_a_quote_in_it_is_escaped() {
    CHECK_EQ(hs::db_quote_ident("a\"b"), std::string("\"a\"\"b\""), "a double quote is doubled");
    CHECK_EQ(hs::db_quote_ident("plain"), std::string("\"plain\""), "ordinary names are quoted as-is");
}

void the_model_reports_its_columns() {
    const hs::OrmModel& m = qa::user_model();
    CHECK_EQ(m.idx("age"), 3, "a column's position");
    CHECK(m.has("email"), "a declared column is found");
    CHECK(!m.has("nope"), "an undeclared column is not");
    CHECK(m.kind_of("age") == hs::OrmKind::Int, "a column knows its kind");
    CHECK(m.kind_of("nope") == hs::OrmKind::Text, "an unknown column reads as text");
}

void a_registered_model_is_kept_alive() {
    auto& a = hs::orm_register_model("A", "a", "id", {"id"}, {hs::OrmKind::Int});
    auto& b = hs::orm_register_model("B", "b", "id", {"id"}, {hs::OrmKind::Int});
    CHECK(&a != &b, "each registration is a distinct model");
    CHECK_EQ(a.name, std::string("A"), "and keeps its name");
    CHECK_EQ(b.table, std::string("b"), "and its table");
}

}  // namespace

int main() {
    selects_every_column_by_default();
    a_where_clause_becomes_a_bound_condition();
    conditions_are_anded_in_order();
    every_comparison_operator_has_its_own_sql();
    like_differs_between_dialects();
    in_expands_one_placeholder_per_value();
    an_empty_in_list_matches_nothing();
    a_single_value_in_is_one_element_membership();
    placeholders_are_numbered_for_postgres();
    order_by_is_directional_and_ordered();
    limit_and_offset_keep_sqlite_happy();
    values_never_reach_the_statement_text();
    bound_values_follow_placeholder_order();
    a_narrowed_projection_replaces_the_column_list();
    count_counts_the_same_set_the_rows_come_from();
    count_of_an_empty_result_is_zero();
    count_reads_a_text_number_too();
    first_asks_for_one_row_and_reports_absence();
    first_does_not_clobber_an_explicit_limit();
    exists_stops_at_the_first_row();
    all_returns_one_object_per_row();
    all_of_nothing_is_an_empty_list();
    values_are_passed_to_the_backend_not_only_baked_into_text();
    a_backend_error_reaches_the_caller();
    without_a_database_the_query_says_so();
    an_identifier_with_a_quote_in_it_is_escaped();
    the_model_reports_its_columns();
    a_registered_model_is_kept_alive();
    return qa::report("query_builder");
}
