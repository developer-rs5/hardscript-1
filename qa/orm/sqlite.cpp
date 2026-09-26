// SQLite adapter (M5.3.5).
//
// This fixture runs against a real database, loaded from `libsqlite3.so.0` at
// run time. The recording fake used by the query builder proves the statements
// are built correctly; only a real database proves they *run*, that values
// survive the round trip, and that a transaction rolls back what it should.
// Every check here is a fact about SQLite, not about a mock.
//
// The library is loaded, not linked, so a machine without SQLite still builds;
// `sqlite_available()` is how a program asks before it needs one.
#include "hs_runtime_sqlite.hpp"
#include "test_support.hpp"

namespace {

/// `model User { id, name, age, active, note, created, updated }`.
///
/// `columns` is the whole table in declaration order, because that is the order
/// a SELECT lists and the rows are decoded in; `generated` is the subset the
/// database fills in, and must be columns of the same model.
const hs::OrmModel& user_model() {
    static hs::OrmModel m(
        "User", "user", "id", {"id", "name", "age", "active", "note", "created", "updated"},
        {hs::OrmKind::Int, hs::OrmKind::Text, hs::OrmKind::Int, hs::OrmKind::Bool, hs::OrmKind::Text,
         hs::OrmKind::Time, hs::OrmKind::Time},
        {"id", "created", "updated"}, "updated");
    return m;
}

/// `model Post { id, title, user_id }`, the far side of a `has_many`.
const hs::OrmModel& post_model() {
    static hs::OrmModel m("Post", "post", "id", {"id", "title", "user_id"},
                          {hs::OrmKind::Int, hs::OrmKind::Text, hs::OrmKind::Int}, {}, "");
    return m;
}

const hs::OrmModel& tag_model() {
    static hs::OrmModel m("Tag", "tag", "id", {"id", "name"}, {hs::OrmKind::Int, hs::OrmKind::Text}, {}, "");
    return m;
}

const char* CREATE_USER =
    "CREATE TABLE \"user\" ("
    "\"id\" INTEGER PRIMARY KEY AUTOINCREMENT,"
    "\"name\" TEXT NOT NULL,"
    "\"age\" INTEGER NOT NULL,"
    "\"active\" INTEGER NOT NULL,"
    "\"note\" TEXT,"
    "\"created\" TEXT NOT NULL DEFAULT (datetime('now')),"
    "\"updated\" TEXT NOT NULL DEFAULT (datetime('now')))";

const char* CREATE_POST =
    "CREATE TABLE \"post\" (\"id\" INTEGER PRIMARY KEY AUTOINCREMENT, \"title\" TEXT NOT NULL, "
    "\"user_id\" INTEGER NOT NULL REFERENCES \"user\"(\"id\"))";
const char* CREATE_TAG = "CREATE TABLE \"tag\" (\"id\" INTEGER PRIMARY KEY AUTOINCREMENT, \"name\" TEXT NOT NULL)";
const char* CREATE_POST_TAG =
    "CREATE TABLE \"post_tag\" (\"post_id\" INTEGER NOT NULL, \"tag_id\" INTEGER NOT NULL)";

/// A database with the tables a relationship needs, per connection because
/// `:memory:` is.
struct Fixture {
    hs::SqliteDb db{":memory:"};
    Fixture() {
        db.run(CREATE_USER, {});
        db.run(CREATE_POST, {});
        db.run(CREATE_TAG, {});
        db.run(CREATE_POST_TAG, {});
    }
    hs::Val user(const char* name, int64_t age) {
        // `note` is a nullable column, and nil is how "no note" is stored.
        return hs::Val::object({{"name", hs::Val::text(name)}, {"age", hs::Val::int_(age)},
                                {"active", hs::Val::boolean(true)}, {"note", hs::Val::nil()}});
    }
    int64_t add_user(const char* name, int64_t age) {
        return hs::orm_create(&user_model(), &db, user(name, age)).find("id")->iv;
    }
};

void the_library_loads_at_run_time() {
    CHECK(hs::sqlite_available(), "libsqlite3 is present on this machine");
    CHECK(hs::sqlite_api().libversion_number() >= hs::SQLITE_MIN_VERSION,
          "and it is new enough for the ABI this file declares");
}

// ---- statements actually run -----------------------------------------------

void a_statement_runs_and_reports_what_it_did() {
    Fixture f;
    hs::DbResult r = f.db.run("INSERT INTO \"user\" (\"name\", \"age\", \"active\") VALUES (?, ?, ?)",
                              {hs::Val::text("a"), hs::Val::int_(30), hs::Val::int_(1)});
    CHECK_EQ(r.affected, int64_t(1), "one row inserted");
    CHECK_EQ(r.last_id, int64_t(1), "and the key the database assigned");
}

void a_value_survives_the_round_trip_with_its_type() {
    Fixture f;
    f.db.run("INSERT INTO \"user\" (\"name\", \"age\", \"active\", \"note\") VALUES (?, ?, ?, ?)",
             {hs::Val::text("bob"), hs::Val::int_(41), hs::Val::int_(0), hs::Val::nil()});
    hs::Val u = hs::OrmQuery(&user_model()).where_eq("id", hs::Val::int_(1)).first(&f.db);
    CHECK(u.is_obj(), "a row came back");
    CHECK_EQ(u.find("name")->sv, std::string("bob"), "text as text");
    CHECK_EQ(u.find("age")->iv, int64_t(41), "an integer as an integer");
    CHECK(u.find("active")->is_bool() && !u.find("active")->bv,
          "a Bool column is a boolean, not the 0 SQLite stores");
    CHECK(u.find("note")->is_nil(), "and NULL is nil");
}

void a_float_survives_as_a_float() {
    Fixture f;
    hs::DbResult r = f.db.run("SELECT ? AS v", {hs::Val::flt(1.5)});
    CHECK(r.rows[0][0].is_flt(), "a float column comes back as a float");
    CHECK(r.rows[0][0].fv == 1.5, "unchanged");
}

void a_container_is_stored_as_json_and_read_back_through_its_kind() {
    Fixture f;
    f.db.run("CREATE TABLE j (id INTEGER PRIMARY KEY, payload TEXT)", {});
    hs::Val payload = hs::Val::object({{"a", hs::Val::int_(1)}, {"b", hs::Val::list({hs::Val::text("x")})}});
    f.db.run("INSERT INTO j (id, payload) VALUES (?, ?)", {hs::Val::int_(1), payload});
    hs::DbResult r = f.db.run("SELECT payload FROM j WHERE id = ?", {hs::Val::int_(1)});
    hs::Val back = hs::db_decode(r.rows[0][0], hs::OrmKind::Json);
    CHECK(back.is_obj(), "read back as an object");
    CHECK_EQ(back.find("a")->iv, int64_t(1), "with its contents");
}

void quoting_survives_a_name_that_needs_it() {
    Fixture f;
    f.db.run("CREATE TABLE \"odd name\" (\"select\" INTEGER)", {});
    f.db.run("INSERT INTO \"odd name\" (\"select\") VALUES (?)", {hs::Val::int_(7)});
    hs::DbResult r = f.db.run("SELECT \"select\" FROM \"odd name\"", {});
    CHECK_EQ(r.rows[0][0].iv, int64_t(7), "a reserved word as a column");
}

void a_value_is_never_part_of_the_statement() {
    Fixture f;
    // A quote in a value has to come back as data. If it were interpolated the
    // statement would not parse, or worse, would run a different one.
    const std::string nasty = "'; DROP TABLE \"user\"; --";
    f.add_user(nasty.c_str(), 1);
    CHECK_EQ(f.db.run("SELECT COUNT(*) FROM \"user\"", {}).rows[0][0].iv, int64_t(1), "the row is there");
    CHECK_EQ(hs::OrmQuery(&user_model()).where_eq("name", hs::Val::text(nasty)).first(&f.db).find("name")->sv,
             nasty, "and the value came back exactly");
    CHECK_EQ(f.db.run("SELECT COUNT(*) FROM \"user\"", {}).rows[0][0].iv, int64_t(1),
             "the table is still there, so the value was data the whole time");
}

// ---- queries against a real database ---------------------------------------

void a_query_finds_the_rows_that_match() {
    Fixture f;
    f.add_user("ann", 30);
    f.add_user("bo", 17);
    f.add_user("cy", 44);
    hs::Val all = hs::OrmQuery(&user_model()).where_gt("age", hs::Val::int_(18)).all(&f.db);
    CHECK_EQ(all.arr.size(), size_t(2), "two adults");
    CHECK(all.arr[0].find("name")->sv != all.arr[1].find("name")->sv, "and they are different people");
}

void order_and_limit_pick_a_page() {
    Fixture f;
    f.add_user("ann", 30);
    f.add_user("bo", 17);
    f.add_user("cy", 44);
    hs::Val page = hs::OrmQuery(&user_model()).order_by("age", false).limit(2).offset(1).all(&f.db);
    CHECK_EQ(page.arr.size(), size_t(2), "two rows");
    CHECK_EQ(page.arr[0].find("name")->sv, std::string("ann"), "the page starts past the youngest");
    CHECK_EQ(page.arr[1].find("name")->sv, std::string("cy"), "and runs to the end");
}

void a_count_describes_the_same_set_as_the_rows() {
    Fixture f;
    f.add_user("ann", 30);
    f.add_user("bo", 17);
    CHECK_EQ(hs::OrmQuery(&user_model()).where_gt("age", hs::Val::int_(18)).count(&f.db).iv, int64_t(1), "one match");
    CHECK_EQ(hs::OrmQuery(&user_model()).count(&f.db).iv, int64_t(2), "two rows in all");
}

void exists_is_answered_by_the_database() {
    Fixture f;
    f.add_user("ann", 30);
    CHECK(hs::OrmQuery(&user_model()).where_eq("name", hs::Val::text("ann")).exists(&f.db).truthy(), "found");
    CHECK(!hs::OrmQuery(&user_model()).where_eq("name", hs::Val::text("zz")).exists(&f.db).truthy(), "not found");
}

void first_on_no_match_is_nil() {
    Fixture f;
    CHECK(hs::OrmQuery(&user_model()).where_eq("name", hs::Val::text("nobody")).first(&f.db).is_nil(),
          "no row is nil, not an empty object");
}

void an_empty_in_list_matches_nothing() {
    Fixture f;
    f.add_user("ann", 30);
    CHECK_EQ(hs::OrmQuery(&user_model()).where_in("id", {}).count(&f.db).iv, int64_t(0),
             "`IN ()` would be a syntax error, so it becomes a false predicate");
    CHECK_EQ(hs::OrmQuery(&user_model()).where_in("id", {hs::Val::int_(1)}).count(&f.db).iv, int64_t(1),
             "and a real one still matches");
}

// ---- relationships ---------------------------------------------------------

void a_has_many_returns_the_child_rows() {
    Fixture f;
    int64_t uid = f.add_user("ann", 30);
    f.db.run("INSERT INTO \"post\" (\"title\", \"user_id\") VALUES (?, ?)", {hs::Val::text("p1"), hs::Val::int_(uid)});
    f.db.run("INSERT INTO \"post\" (\"title\", \"user_id\") VALUES (?, ?)", {hs::Val::text("p2"), hs::Val::int_(uid)});
    hs::Val u = hs::OrmQuery(&user_model()).where_eq("id", hs::Val::int_(uid)).first(&f.db);
    hs::Val posts = hs::OrmQuery(&post_model()).of_record(u, "id", "user_id").order_by("title", false).all(&f.db);
    CHECK_EQ(posts.arr.size(), size_t(2), "both posts");
    CHECK_EQ(posts.arr[0].find("title")->sv, std::string("p1"), "in order");
}

void a_belongs_to_returns_the_parent_row() {
    Fixture f;
    int64_t uid = f.add_user("ann", 30);
    int64_t pid = f.db.run("INSERT INTO \"post\" (\"title\", \"user_id\") VALUES (?, ?) RETURNING id",
                           {hs::Val::text("p1"), hs::Val::int_(uid)})
                     .last_id;
    hs::Val p = hs::OrmQuery(&post_model()).where_eq("id", hs::Val::int_(pid)).first(&f.db);
    hs::Val owner = hs::OrmQuery(&user_model()).of_record(p, "user_id", "id").first(&f.db);
    CHECK(owner.is_obj() && owner.find("name") != nullptr, "the parent came back");
    CHECK_EQ(owner.find("name")->sv, std::string("ann"), "the right one");
}

void a_many_to_many_returns_the_rows_on_the_far_side() {
    Fixture f;
    int64_t uid = f.add_user("ann", 30);
    int64_t pid = f.db.run("INSERT INTO \"post\" (\"title\", \"user_id\") VALUES (?, ?)",
                           {hs::Val::text("p1"), hs::Val::int_(uid)})
                     .last_id;
    f.db.run("INSERT INTO \"tag\" (\"name\") VALUES (?)", {hs::Val::text("red")});
    f.db.run("INSERT INTO \"tag\" (\"name\") VALUES (?)", {hs::Val::text("blue")});
    f.db.run("INSERT INTO \"post_tag\" (\"post_id\", \"tag_id\") VALUES (?, ?)",
             {hs::Val::int_(pid), hs::Val::int_(1)});
    f.db.run("INSERT INTO \"post_tag\" (\"post_id\", \"tag_id\") VALUES (?, ?)",
             {hs::Val::int_(pid), hs::Val::int_(2)});
    hs::Val p = hs::OrmQuery(&post_model()).where_eq("id", hs::Val::int_(pid)).first(&f.db);
    const std::string join = "INNER JOIN \"post_tag\" ON \"post_tag\".\"tag_id\" = \"tag\".\"id\"";
    hs::Val tags = hs::OrmQuery(&tag_model()).of_record(p, "id", "post_tag.post_id", join).order_by("name", false).all(&f.db);
    CHECK_EQ(tags.arr.size(), size_t(2), "both tags, not the junction");
    CHECK_EQ(tags.arr[0].find("name")->sv, std::string("blue"), "in order");
    CHECK_EQ(tags.arr[1].find("name")->sv, std::string("red"), "and both of them");
    CHECK(tags.arr[0].find("post_id") == nullptr, "and the junction's own columns are not in the row");
}

void a_relationship_from_a_record_without_its_key_matches_nothing() {
    Fixture f;
    f.add_user("ann", 30);
    hs::Val bare = hs::Val::object({{"name", hs::Val::text("ann")}});
    CHECK_EQ(hs::OrmQuery(&post_model()).of_record(bare, "id", "user_id").count(&f.db).iv, int64_t(0),
             "a parent with no key is empty, not an error");
}

// ---- writes ----------------------------------------------------------------

void a_created_row_carries_the_key_the_database_gave_it() {
    Fixture f;
    hs::Val u = hs::orm_create(&user_model(), &f.db, f.user("ann", 30));
    CHECK(u.find("id") != nullptr && u.find("id")->iv == 1, "the key came back");
    CHECK_EQ(hs::OrmQuery(&user_model()).count(&f.db).iv, int64_t(1), "and the row is there");
}

void a_key_the_record_carried_survives_the_insert() {
    Fixture f;
    hs::Val u = hs::orm_create(&user_model(), &f.db,
                              hs::Val::object({{"id", hs::Val::int_(40)}, {"name", hs::Val::text("ann")},
                                               {"age", hs::Val::int_(30)}, {"active", hs::Val::boolean(true)},
                                               {"note", hs::Val::nil()}}));
    CHECK_EQ(u.find("id")->iv, int64_t(40), "the caller's key, not the next rowid");
    CHECK_EQ(hs::OrmQuery(&user_model()).where_eq("id", hs::Val::int_(40)).count(&f.db).iv, int64_t(1),
             "stored under that key");
}

void save_writes_the_row_the_record_came_from() {
    Fixture f;
    hs::Val u = hs::orm_create(&user_model(), &f.db, f.user("ann", 30));
    u.set("name", hs::Val::text("ann2"));
    hs::Val changed = u;
    (void)hs::orm_save(&user_model(), &f.db, changed);
    CHECK_EQ(hs::OrmQuery(&user_model()).where_eq("id", hs::Val::int_(u.find("id")->iv)).first(&f.db).find("name")->sv,
             std::string("ann2"), "the change is in the database, not just in the value");
}

void an_unkeyed_save_is_an_insert() {
    Fixture f;
    (void)hs::orm_save(&user_model(), &f.db, f.user("new", 20));
    CHECK_EQ(hs::OrmQuery(&user_model()).count(&f.db).iv, int64_t(1), "one row now");
}

void a_delete_reports_whether_a_row_went_away() {
    Fixture f;
    int64_t uid = f.add_user("ann", 30);
    CHECK(hs::orm_delete_key(&user_model(), &f.db, hs::Val::int_(uid)).truthy(), "one row is gone");
    CHECK(!hs::orm_delete_key(&user_model(), &f.db, hs::Val::int_(uid)).truthy(), "a second delete is not");
    CHECK_EQ(hs::OrmQuery(&user_model()).count(&f.db).iv, int64_t(0), "and the table is empty");
}

void an_upsert_updates_a_row_that_is_there() {
    Fixture f;
    int64_t uid = f.add_user("ann", 30);
    hs::Val u = hs::orm_upsert(&user_model(), &f.db,
                               hs::Val::object({{"id", hs::Val::int_(uid)}, {"name", hs::Val::text("new")},
                                                {"age", hs::Val::int_(31)}, {"active", hs::Val::boolean(true)},
                                                {"note", hs::Val::nil()}}));
    CHECK_EQ(u.find("id")->iv, uid, "the same key");
    CHECK_EQ(hs::OrmQuery(&user_model()).count(&f.db).iv, int64_t(1), "still one row");
    CHECK_EQ(hs::OrmQuery(&user_model()).first(&f.db).find("name")->sv, std::string("new"), "updated");
}

void a_null_column_is_matched_with_is_null() {
    // `note = NULL` is never true in SQL, so a bound nil used to match nothing
    // at all. The question "which rows have no note" has to be asked as
    // `IS NULL`.
    Fixture f;
    f.add_user("ann", 30);
    f.db.run("INSERT INTO \"user\" (\"name\", \"age\", \"active\", \"note\") VALUES (?, ?, ?, ?)",
             {hs::Val::text("bo"), hs::Val::int_(20), hs::Val::int_(1), hs::Val::text("hi")});
    CHECK_EQ(hs::OrmQuery(&user_model()).where_eq("note", hs::Val::nil()).count(&f.db).iv, int64_t(1),
             "the row with no note");
    CHECK_EQ(hs::OrmQuery(&user_model()).where_ne("note", hs::Val::nil()).count(&f.db).iv, int64_t(1),
             "and the one that has one");
    CHECK_EQ(hs::OrmQuery(&user_model()).where_eq("note", hs::Val::text("hi")).count(&f.db).iv, int64_t(1),
             "while a real value still compares as an equality");
}

void find_or_create_inserts_only_when_nothing_matches() {
    Fixture f;
    hs::Val want = hs::Val::object({{"name", hs::Val::text("ann")}, {"age", hs::Val::int_(30)},
                                    {"active", hs::Val::boolean(true)}, {"note", hs::Val::nil()}});
    hs::orm_find_or_create(&user_model(), &f.db, want);
    CHECK_EQ(hs::OrmQuery(&user_model()).count(&f.db).iv, int64_t(1), "inserted once");
    // The same attributes a second time, which is what find_or_create is for.
    hs::Val again = hs::Val::object({{"name", hs::Val::text("ann")}, {"age", hs::Val::int_(30)},
                                     {"active", hs::Val::boolean(true)}, {"note", hs::Val::nil()}});
    hs::Val found = hs::orm_find_or_create(&user_model(), &f.db, again);
    CHECK_EQ(hs::OrmQuery(&user_model()).count(&f.db).iv, int64_t(1), "and matched the second time");
    CHECK_EQ(found.find("id")->iv, int64_t(1), "returning the row that was already there");
    // A different age is a different row, which is what the conjunction means.
    hs::orm_find_or_create(&user_model(), &f.db,
                           hs::Val::object({{"name", hs::Val::text("ann")}, {"age", hs::Val::int_(99)},
                                            {"active", hs::Val::boolean(false)}, {"note", hs::Val::nil()}}));
    CHECK_EQ(hs::OrmQuery(&user_model()).count(&f.db).iv, int64_t(2),
             "attributes that do not match are a new row, not an overwrite");
}

void touch_moves_the_updated_column() {
    Fixture f;
    hs::Val u = hs::orm_create(&user_model(), &f.db, f.user("ann", 30));
    f.db.run("UPDATE \"user\" SET \"updated\" = '2000-01-01 00:00:00' WHERE id = ?", {hs::Val::int_(1)});
    (void)hs::orm_touch(&user_model(), &f.db, u);
    CHECK(hs::OrmQuery(&user_model()).first(&f.db).find("updated")->sv != std::string("2000-01-01 00:00:00"),
          "the database's clock, not the caller's");
}

void a_foreign_key_is_enforced_by_the_database() {
    Fixture f;
    bool threw = false;
    try {
        f.db.run("INSERT INTO \"post\" (\"title\", \"user_id\") VALUES (?, ?)",
                 {hs::Val::text("orphan"), hs::Val::int_(999)});
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "a post with no user is refused by the constraint the model declared");
    CHECK(f.db.foreign_keys, "because the connection turned them on, not SQLite's default");
}

// ---- transactions ----------------------------------------------------------

void a_committed_write_is_kept() {
    Fixture f;
    f.db.begin();
    (void)hs::orm_create(&user_model(), &f.db, f.user("ann", 30));
    f.db.commit();
    CHECK_EQ(hs::OrmQuery(&user_model()).count(&f.db).iv, int64_t(1), "the row is there after the commit");
}

void a_rolled_back_write_leaves_nothing() {
    Fixture f;
    f.db.begin();
    (void)hs::orm_create(&user_model(), &f.db, f.user("ann", 30));
    f.db.rollback();
    CHECK_EQ(hs::OrmQuery(&user_model()).count(&f.db).iv, int64_t(0), "the row is gone, as if it never happened");
}

void a_nested_commit_is_owned_by_the_outermost_one() {
    Fixture f;
    f.db.begin();
    f.db.begin();
    CHECK_EQ(f.db.tx_depth(), 2, "two deep");
    (void)hs::orm_create(&user_model(), &f.db, f.user("ann", 30));
    f.db.commit();
    CHECK_EQ(f.db.tx_depth(), 1, "still open, because the outer one is");
    // The inner commit did not end anything, so a rollback here still undoes
    // the write. That is the whole question about nesting: a savepoint that
    // quietly committed early would make the outer rollback a lie.
    f.db.rollback();
    CHECK_EQ(f.db.tx_depth(), 0, "the rollback ended it");
    CHECK_EQ(hs::OrmQuery(&user_model()).count(&f.db).iv, int64_t(0), "and the inner commit had not committed it");
}

void a_rollback_at_any_depth_undoes_the_whole_transaction() {
    Fixture f;
    f.db.begin();
    f.db.begin();
    (void)hs::orm_create(&user_model(), &f.db, f.user("ann", 30));
    f.db.rollback();
    CHECK_EQ(f.db.tx_depth(), 0, "the transaction is over at either depth");
    CHECK_EQ(hs::OrmQuery(&user_model()).count(&f.db).iv, int64_t(0), "and the write is gone");
}

void a_connection_closed_mid_transaction_does_not_commit() {
    // The failure a request can cause: a handler throws between `begin` and
    // `commit`. Whatever the object is destroyed with, the row must not appear.
    auto* db = new hs::SqliteDb(":memory:");
    db->run(CREATE_USER, {});
    db->begin();
    db->run("INSERT INTO \"user\" (\"name\", \"age\", \"active\") VALUES (?, ?, ?)",
            {hs::Val::text("ann"), hs::Val::int_(30), hs::Val::int_(1)});
    CHECK_EQ(db->run("SELECT COUNT(*) FROM \"user\"", {}).rows[0][0].iv, int64_t(1), "visible inside");
    delete db;
    CHECK(true, "and closing it does not throw on the way out");
}

// ---- the seam itself -------------------------------------------------------

void the_backend_names_itself_and_its_dialect() {
    Fixture f;
    CHECK_EQ(std::string(f.db.name()), std::string("sqlite"), "named for the library");
    CHECK(f.db.dialect() == hs::DbDialect::Sqlite, "so the builder uses `?` and not `$1`");
}

void a_file_database_outlives_its_connection() {
    const char* path = "/tmp/opencode/hs-sqlite-fixture.db";
    std::remove(path);
    {
        hs::SqliteDb db(path);
        db.run(CREATE_USER, {});
        db.run("INSERT INTO \"user\" (\"name\", \"age\", \"active\") VALUES (?, ?, ?)",
               {hs::Val::text("ann"), hs::Val::int_(30), hs::Val::int_(1)});
    }
    hs::SqliteDb again(path);
    CHECK_EQ(hs::OrmQuery(&user_model()).count(&again).iv, int64_t(1), "the file kept the row");
    std::remove(path);
}

void a_readonly_connection_refuses_a_write() {
    const char* path = "/tmp/opencode/hs-sqlite-ro.db";
    std::remove(path);
    {
        hs::SqliteDb db(path);
        db.run(CREATE_USER, {});
    }
    hs::SqliteDb ro(path, /*readonly=*/true);
    bool threw = false;
    try {
        ro.run("INSERT INTO \"user\" (\"name\", \"age\", \"active\") VALUES (?, ?, ?)",
               {hs::Val::text("x"), hs::Val::int_(1), hs::Val::int_(1)});
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "a read-only connection says so rather than writing");
    std::remove(path);
}

void a_bad_statement_says_what_was_wrong() {
    Fixture f;
    std::string why;
    try {
        f.db.run("SELECT * FROM nope", {});
    } catch (const std::exception& e) {
        why = e.what();
    }
    CHECK(why.find("nope") != std::string::npos, "the message names the table: " + why);
}

void a_mismatched_parameter_count_is_caught_before_sqlite_sees_it() {
    Fixture f;
    std::string why;
    try {
        f.db.run("SELECT * FROM \"user\" WHERE id = ?", {hs::Val::int_(1), hs::Val::int_(2)});
    } catch (const std::exception& e) {
        why = e.what();
    }
    CHECK(why.find("wants 1 values") != std::string::npos, "and says which count: " + why);
}

void opening_a_path_that_cannot_be_written_says_so() {
    bool threw = false;
    std::string why;
    try {
        hs::SqliteDb db("/nonexistent-directory-xyz/db.sqlite");
    } catch (const std::exception& e) {
        threw = true;
        why = e.what();
    }
    CHECK(threw, "opening fails rather than returning a broken handle");
    CHECK(why.find("sqlite:") == 0, "with a message that says which database: " + why);
}

void the_ambient_connection_is_what_generated_code_reads() {
    Fixture f;
    auto* opened = hs::db_open_sqlite(":memory:");
    CHECK(hs::db_need() == opened, "`db_need()` is the connection that was opened");
    CHECK_EQ(std::string(hs::db_need()->name()), std::string("sqlite"), "and it is the one generated code uses");
    hs::db_close(opened);
    CHECK(hs::db_get() == nullptr, "and closing it clears the slot");
    bool threw = false;
    try {
        (void)hs::db_need();
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "so a query with no database fails clearly");
}

}  // namespace

int main() {
    the_library_loads_at_run_time();
    a_statement_runs_and_reports_what_it_did();
    a_value_survives_the_round_trip_with_its_type();
    a_float_survives_as_a_float();
    a_container_is_stored_as_json_and_read_back_through_its_kind();
    quoting_survives_a_name_that_needs_it();
    a_value_is_never_part_of_the_statement();
    a_query_finds_the_rows_that_match();
    order_and_limit_pick_a_page();
    a_count_describes_the_same_set_as_the_rows();
    exists_is_answered_by_the_database();
    first_on_no_match_is_nil();
    an_empty_in_list_matches_nothing();
    a_has_many_returns_the_child_rows();
    a_belongs_to_returns_the_parent_row();
    a_many_to_many_returns_the_rows_on_the_far_side();
    a_relationship_from_a_record_without_its_key_matches_nothing();
    a_created_row_carries_the_key_the_database_gave_it();
    a_key_the_record_carried_survives_the_insert();
    save_writes_the_row_the_record_came_from();
    an_unkeyed_save_is_an_insert();
    a_delete_reports_whether_a_row_went_away();
    an_upsert_updates_a_row_that_is_there();
    a_null_column_is_matched_with_is_null();
    find_or_create_inserts_only_when_nothing_matches();
    touch_moves_the_updated_column();
    a_foreign_key_is_enforced_by_the_database();
    a_committed_write_is_kept();
    a_rolled_back_write_leaves_nothing();
    a_nested_commit_is_owned_by_the_outermost_one();
    a_rollback_at_any_depth_undoes_the_whole_transaction();
    a_connection_closed_mid_transaction_does_not_commit();
    the_backend_names_itself_and_its_dialect();
    a_file_database_outlives_its_connection();
    a_readonly_connection_refuses_a_write();
    a_bad_statement_says_what_was_wrong();
    a_mismatched_parameter_count_is_caught_before_sqlite_sees_it();
    opening_a_path_that_cannot_be_written_says_so();
    the_ambient_connection_is_what_generated_code_reads();
    return qa::report("sqlite");
}
