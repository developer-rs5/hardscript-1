// qa/orm/transactions.cpp -- transactions and savepoints
//
// A transaction is a promise about failure: everything in the block lands, or
// nothing does. The `TxGuard` makes that promise in C++ (commit on a clean
// exit, rollback on an exception); the backends make it in SQL (BEGIN against
// an empty connection, SAVEPOINT against an open one). The fake backend below
// checks the promise protocol; real SQLite checks the SQL keeps it.

#include "hs_runtime_orm.hpp"
#include "hs_runtime_sqlite.hpp"

#include "test_support.hpp"

namespace qa {
namespace {

// ---------------------------------------------------------------------------
// The guard, against a fake
// ---------------------------------------------------------------------------

void a_clean_exit_commits() {
    FakeBackend b;
    {
        hs::TxGuard g(&b);
        CHECK_EQ(b.tx_depth(), 1, "the guard opened a transaction");
        CHECK_EQ(b.begins, 1, "once");
    }
    CHECK_EQ(b.commits, 1, "and committed it on the way out");
    CHECK_EQ(b.tx_depth(), 0, "leaving nothing open");
}

void an_exception_rolls_back() {
    FakeBackend b;
    bool threw = false;
    try {
        hs::TxGuard g(&b);
        throw std::runtime_error("boom");
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "the error still propagates");
    CHECK_EQ(b.rollbacks, 1, "after rolling the transaction back");
    CHECK_EQ(b.commits, 0, "and committing nothing");
    CHECK_EQ(b.tx_depth(), 0, "leaving nothing open");
}

void a_nested_block_marks_a_savepoint_instead_of_beginning() {
    FakeBackend b;
    {
        hs::TxGuard outer(&b);
        {
            hs::TxGuard inner(&b);
            CHECK_EQ(b.begins, 1, "one BEGIN for both blocks");
            CHECK_EQ(b.savepoints.size(), size_t(1), "and one savepoint for the inner one");
            CHECK_EQ(b.savepoints[0], std::string("sp:hs_sp_1"), "with a minted name");
        }
        CHECK_EQ(b.savepoints.size(), size_t(2), "a clean inner exit releases its mark");
        CHECK_EQ(b.savepoints[1], std::string("rel:hs_sp_1"), "the same mark it made");
    }
    CHECK_EQ(b.commits, 1, "the outer block commits once");
}

void a_failing_inner_block_undoes_only_itself() {
    FakeBackend b;
    {
        hs::TxGuard outer(&b);
        bool threw = false;
        try {
            hs::TxGuard inner(&b);
            throw std::runtime_error("inner");
        } catch (const std::exception&) {
            threw = true;
        }
        CHECK(threw, "the inner error propagates");
        CHECK_EQ(b.savepoints.size(), size_t(3), "rolled back to the mark, then forgot it");
        CHECK_EQ(b.savepoints[1], std::string("to:hs_sp_1"), "in that order");
        CHECK_EQ(b.savepoints[2], std::string("rel:hs_sp_1"), "in that order");
        CHECK_EQ(b.rollbacks, 0, "while the outer transaction never rolled back");
        CHECK_EQ(b.commits, 0, "and never committed early either");
    }
    CHECK_EQ(b.commits, 1, "the outer block still commits");
}

void three_levels_nest_in_order() {
    FakeBackend b;
    {
        hs::TxGuard a(&b);
        {
            hs::TxGuard c(&b);
            {
                hs::TxGuard d(&b);
            }
        }
    }
    std::string got;
    for (auto& s : b.savepoints) {
        if (!got.empty()) got += ",";
        got += s;
    }
    CHECK_EQ(got, std::string("sp:hs_sp_1,sp:hs_sp_2,rel:hs_sp_2,rel:hs_sp_1"),
             "marks unwind innermost first");
}

// ---------------------------------------------------------------------------
// Named savepoints, against a fake
// ---------------------------------------------------------------------------

void a_name_has_to_be_an_identifier_before_it_reaches_sql() {
    FakeBackend b;
    b.begin();
    for (const char* bad : {"", "has space", "semi;colon", "quote\"", "9lives", "dash-ed"}) {
        bool threw = false;
        try {
            hs::db_savepoint(&b, hs::Val::text(bad));
        } catch (const std::exception&) {
            threw = true;
        }
        CHECK(threw, std::string("rejected: ") + bad);
    }
    CHECK(b.open_sps.empty(), "and nothing was marked");
    bool threw = false;
    try {
        hs::db_savepoint(&b, hs::Val::int_(1));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "a non-string is not a name either");
}

void a_savepoint_needs_an_open_transaction() {
    FakeBackend b;
    bool threw = false;
    try {
        hs::db_savepoint(&b, hs::Val::text("s"));
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("open transaction") != std::string::npos;
    }
    CHECK(threw, "and it says what is missing");
    threw = false;
    try {
        hs::db_rollback_to(&b, hs::Val::text("s"));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "for rolling back too");
}

void rolling_back_to_nothing_is_an_error_not_a_silence() {
    FakeBackend b;
    b.begin();
    bool threw = false;
    try {
        hs::db_rollback_to(&b, hs::Val::text("never"));
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("never") != std::string::npos;
    }
    CHECK(threw, "and it names the mark that was never made");
}

// ---------------------------------------------------------------------------
// Real SQLite
// ---------------------------------------------------------------------------

const char* USER_DDL =
    "CREATE TABLE \"user\" (\"id\" INTEGER PRIMARY KEY AUTOINCREMENT, \"name\" TEXT NOT NULL)";

int64_t user_count(hs::SqliteDb& db) {
    return db.run("SELECT COUNT(*) FROM \"user\"", {}).rows[0][0].iv;
}

void committed_writes_stay() {
    hs::SqliteDb db(":memory:");
    db.run(USER_DDL, {});
    {
        hs::TxGuard g(&db);
        db.run("INSERT INTO \"user\" (\"name\") VALUES (?)", {hs::Val::text("ann")});
    }
    CHECK_EQ(user_count(db), int64_t(1), "one row");
    CHECK_EQ(db.tx_depth(), 0, "and nothing left open");
}

void a_thrown_error_undoes_the_block() {
    hs::SqliteDb db(":memory:");
    db.run(USER_DDL, {});
    try {
        hs::TxGuard g(&db);
        db.run("INSERT INTO \"user\" (\"name\") VALUES (?)", {hs::Val::text("ann")});
        db.run("INSERT INTO \"user\" (\"name\") VALUES (?)", {hs::Val::text("bo")});
        throw std::runtime_error("late failure");
    } catch (const std::exception&) {
    }
    CHECK_EQ(user_count(db), int64_t(0), "both inserts went with the error");
    CHECK_EQ(db.tx_depth(), 0, "and the connection is usable again");
    // Usable again means usable: the next transaction commits fine.
    {
        hs::TxGuard g(&db);
        db.run("INSERT INTO \"user\" (\"name\") VALUES (?)", {hs::Val::text("cy")});
    }
    CHECK_EQ(user_count(db), int64_t(1), "afterwards");
}

void an_inner_failure_keeps_the_outer_writes() {
    hs::SqliteDb db(":memory:");
    db.run(USER_DDL, {});
    {
        hs::TxGuard outer(&db);
        db.run("INSERT INTO \"user\" (\"name\") VALUES (?)", {hs::Val::text("ann")});
        try {
            hs::TxGuard inner(&db);
            db.run("INSERT INTO \"user\" (\"name\") VALUES (?)", {hs::Val::text("bo")});
            throw std::runtime_error("inner failure");
        } catch (const std::exception&) {
        }
        CHECK_EQ(user_count(db), int64_t(1), "the outer row is still there, the inner one is not");
    }
    CHECK_EQ(user_count(db), int64_t(1), "and it committed");
}

void a_named_savepoint_round_trips() {
    hs::SqliteDb db(":memory:");
    db.run(USER_DDL, {});
    {
        hs::TxGuard g(&db);
        hs::db_savepoint(&db, hs::Val::text("before"));
        db.run("INSERT INTO \"user\" (\"name\") VALUES (?)", {hs::Val::text("ann")});
        CHECK_EQ(user_count(db), int64_t(1), "written");
        hs::db_rollback_to(&db, hs::Val::text("before"));
        CHECK_EQ(user_count(db), int64_t(0), "then unwound to the mark");
        // The mark survives the rollback, so the stretch can be retried.
        db.run("INSERT INTO \"user\" (\"name\") VALUES (?)", {hs::Val::text("bo")});
        CHECK_EQ(user_count(db), int64_t(1), "retried");
    }
    CHECK_EQ(user_count(db), int64_t(1), "and committed");
}

void savepoint_marks_stack_like_the_database_stacks_them() {
    hs::SqliteDb db(":memory:");
    db.run(USER_DDL, {});
    {
        hs::TxGuard g(&db);
        hs::db_savepoint(&db, hs::Val::text("s"));
        hs::db_savepoint(&db, hs::Val::text("s"));
        db.run("INSERT INTO \"user\" (\"name\") VALUES (?)", {hs::Val::text("ann")});
        hs::db_rollback_to(&db, hs::Val::text("s"));
        CHECK_EQ(user_count(db), int64_t(0), "the most recent mark is the one that counts");
        db.release_savepoint("s");
        db.release_savepoint("s");
        bool threw = false;
        try {
            db.release_savepoint("s");
        } catch (const std::exception&) {
            threw = true;
        }
        CHECK(threw, "releasing past the stack is an error");
    }
}

void a_savepoint_outside_a_transaction_is_refused() {
    hs::SqliteDb db(":memory:");
    bool threw = false;
    try {
        db.savepoint("s");
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "the backend refuses what the language forbids");
}

void savepoints_do_not_move_the_depth_counter() {
    hs::SqliteDb db(":memory:");
    {
        hs::TxGuard g(&db);
        CHECK_EQ(db.tx_depth(), 1, "one transaction");
        hs::db_savepoint(&db, hs::Val::text("s"));
        CHECK_EQ(db.tx_depth(), 1, "a mark is not a transaction");
    }
    CHECK_EQ(db.tx_depth(), 0, "closed");
}

}  // namespace
}  // namespace qa

int main() {
    qa::a_clean_exit_commits();
    qa::an_exception_rolls_back();
    qa::a_nested_block_marks_a_savepoint_instead_of_beginning();
    qa::a_failing_inner_block_undoes_only_itself();
    qa::three_levels_nest_in_order();

    qa::a_name_has_to_be_an_identifier_before_it_reaches_sql();
    qa::a_savepoint_needs_an_open_transaction();
    qa::rolling_back_to_nothing_is_an_error_not_a_silence();

    qa::committed_writes_stay();
    qa::a_thrown_error_undoes_the_block();
    qa::an_inner_failure_keeps_the_outer_writes();
    qa::a_named_savepoint_round_trips();
    qa::savepoint_marks_stack_like_the_database_stacks_them();
    qa::a_savepoint_outside_a_transaction_is_refused();
    qa::savepoints_do_not_move_the_depth_counter();

    return qa::report("transactions");
}
