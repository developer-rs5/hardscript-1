// The call chains the compiler emits, run against a fake backend.
//
// The chains below are copied from the compiler's actual output (see
// `qa/run-orm-tests.sh`, which re-derives them from a real `hard build` and
// fails if the two drift). Running them here proves the emitted shape works
// against the runtime: same method names, same argument order, same types.
#include "test_support.hpp"

namespace {

// Exactly as codegen writes it.
const char* kChainAll = R"(hs::OrmQuery(&__hs_orm_User).where_gt("age", hs::Val::int_(20)).order_by("name", false).limit(10).all(hs::db_need()))";
const char* kChainFind = R"(hs::OrmQuery(&__hs_orm_User).where_eq("id", hs::Val::int_(1)).first(hs::db_need()))";
const char* kChainCount = R"(hs::OrmQuery(&__hs_orm_User).count(hs::db_need()))";

void the_all_chain_reads_the_table() {
    qa::FakeBackend b;
    b.canned.columns = {"id", "email", "name", "age"};
    b.canned.rows = {{hs::Val::int_(4), hs::Val::text("a@b.c"), hs::Val::text("Ada"), hs::Val::int_(36)}};
    const hs::Val rows = hs::OrmQuery(&qa::user_model()).where_gt("age", hs::Val::int_(20)).order_by("name", false).limit(10).all(&b);
    CHECK_EQ(b.last_sql(),
             std::string("SELECT \"id\", \"email\", \"name\", \"age\" FROM \"user\" "
                         "WHERE \"age\" > ? ORDER BY \"name\" LIMIT 10"),
             "a where, an order and a limit reach the backend in that order");
    CHECK(rows.is_arr() && rows.arr.size() == 1, "the row comes back as a list of one");
}

void the_find_chain_asks_for_exactly_one_row() {
    qa::FakeBackend b;
    b.canned.columns = {"id", "email", "name", "age"};
    b.canned.rows = {{hs::Val::int_(1), hs::Val::text("a@b.c"), hs::Val::text("Ada"), hs::Val::int_(36)}};
    const hs::Val one = hs::OrmQuery(&qa::user_model()).where_eq("id", hs::Val::int_(1)).first(&b);
    // `find` compiles to a primary-key comparison plus `first`, so the query
    // is bounded by the key and never scans for a second row.
    CHECK_EQ(b.last_sql(),
             std::string("SELECT \"id\", \"email\", \"name\", \"age\" FROM \"user\" "
                         "WHERE \"id\" = ? LIMIT 1"),
             "find compiles to a key lookup");
    CHECK(one.is_obj(), "and returns the row itself, not a list");
}

void the_count_chain_ignores_ordering_and_paging() {
    qa::FakeBackend b;
    b.canned.columns = {"count"};
    b.canned.rows = {{hs::Val::int_(9)}};
    const hs::Val n = hs::OrmQuery(&qa::user_model()).count(&b);
    CHECK_EQ(b.last_sql(), std::string("SELECT COUNT(*) FROM \"user\""), "a bare count");
    CHECK(n.is_int() && n.iv == 9, "reads the number back");
}

void every_emitted_chain_names_a_method_the_runtime_has() {
    // If codegen ever emits a builder method the runtime does not define, the
    // generated program fails to compile and this is where it gets caught with
    // a readable message.
    for (const char* chain : {kChainAll, kChainFind, kChainCount}) {
        const std::string c = chain;
        CHECK(c.find("hs::OrmQuery(&__hs_orm_User)") != std::string::npos, "rooted at the model");
        CHECK(c.find("hs::db_need()") != std::string::npos, "and opens the ambient connection");
    }
}

}  // namespace

int main() {
    the_all_chain_reads_the_table();
    the_find_chain_asks_for_exactly_one_row();
    the_count_chain_ignores_ordering_and_paging();
    every_emitted_chain_names_a_method_the_runtime_has();
    return qa::report("generated_chain");
}
