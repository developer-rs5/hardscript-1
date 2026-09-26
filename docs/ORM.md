# HardScript ORM

The native ORM maps models to database tables and back. The compiler checks
every query and write against the schema before anything runs; the runtime
binds every value as a parameter, never interpolating one into SQL. Two
backends implement the same seam: SQLite and PostgreSQL.

## Opening the database

The ORM reads the database through one ambient connection per thread. The
host opens it; generated code never guesses:

```cpp
hs::SqliteDb* db = hs::db_open_sqlite("app.db");   // also sets it ambient
hs::PgDb* db = hs::db_open_pgsql("postgresql://user:pass@localhost/shop");
hs::db_close(db);
```

`hard migrate` and `hard seed` open their own connections per invocation.
Wiring the manifest's `[database]` section to server startup is a later
milestone; until then the embedding host owns the handle.

```toml
[database]
dialect = "sqlite"
path = "app.db"
```

`dialect` is `"sqlite"` or `"postgres"`. SQLite names a file in `path`;
PostgreSQL names a connection in `url`, either a `postgresql://` URL or
`key=value` pairs. A missing or unknown dialect is `HS0222`.

## Models

```hard
model User {
    id    : Int @primary @auto_increment
    email : Email
    name  : String
    age   : Int @default(18)
}
```

The table defaults to the lowercased model name (`User` → `user`);
`model Name = table { ... }` pins it. Field attributes:

| attribute | meaning |
|---|---|
| `@primary` | the table's key (`id` is implicit) |
| `@auto_increment` | the database assigns the key |
| `@unique`, `@index` | uniqueness, a standalone index |
| `@nullable` | the column may hold NULL |
| `@default(v)`, `@default(now())` | database-side default |
| `@foreign(t.c)` | references another table's column |
| `@belongs_to`, `@has_many`, `@has_one` | relationships |
| `@many_to_many @through(t)` | relationships through a join table |
| `@created_at`, `@updated_at` | timestamps the database fills in |

Source types map to storage per backend (`Int` → `INTEGER`, `Bool` →
`INTEGER` on SQLite and `BOOLEAN` on PostgreSQL, `Time` → text on SQLite
and `TIMESTAMPTZ` on PostgreSQL). A field the ORM cannot store is `HS0211`.

## CRUD

```hard
u <- User.create({ email: "a@b.c", name: "Ada" })
u <- User.find(1)
u <- User.upsert({ id: 1, name: "Ada" })
found <- User.find_or_create_by({ email: "a@b.c", name: "Ada" })
User.delete(1)
```

`create` returns the row with its generated key. `find` returns one row or
`nil`. `upsert` updates by key and inserts when the row is absent. A record
read back can be written in place:

```hard
u <- User.find(1)
u.save()
u.touch()
u.destroy()
```

`save` updates the row the record came from, or inserts when it has no key
yet. `touch` refreshes `@updated_at`. `destroy` and `delete` remove; `delete`
reports whether a row went away. A write that leaves out a column the
database will not fill in fails at compile time (`HS0225`); a write method
on something that is not a row is `HS0226`.

## Queries

```hard
adults <- User.where(age >= 18).order_by(age, desc).limit(10).all()
one    <- User.where(email == "a@b.c").first()
n      <- User.where(age >= 18).count()
yes    <- User.where(email == "a@b.c").exists()
teens  <- User.where_in(age, [13, 14, 15]).all()
close  <- User.where_like(name, "A%").all()
```

Equality is `==`, as everywhere else. `where` also takes `where(age, 20)`
and `where(age, 20, ">=")`. A query reads nothing until a terminal ends it:
`.all()` (every row), `.first()` (one row or `nil`), `.count()`, `.exists()`.
An unknown column is `HS0219`; a bad column in `order_by` is `HS0220`; a
malformed query is `HS0221`. A comparison against `nil` renders `IS NULL`,
because `col = NULL` is never true in SQL.

## Relationships

```hard
model Post {
    id      : Int @primary @auto_increment
    title   : String
    user_id : Int @foreign(user.id)
    user    : User @belongs_to
    tags    : Tag @many_to_many @through(post_tag)
}
model User {
    id    : Int @primary @auto_increment
    posts : Post @has_many
}
model post_tag {
    post_id : Int
    tag_id  : Int
}
```

```hard
posts <- user.posts.all()
owner <- post.user.first()
tags  <- post.tags.all()
```

`belongs_to` and `has_one` read one row; `has_many` and `many_to_many` read a
list and need a list terminal. The join table is an ordinary model with a
column per side, named after the two tables (`post_id`, `tag_id`). Reading an
undeclared relation is `HS0227`; a missing link column is `HS0228`; a missing
join table is `HS0229`.

## Transactions

```hard
db.transaction {
    User.create({ email: "a@b.c", name: "Ada" })
    db.savepoint("before_post")
    Post.create({ title: "hi", user_id: 1 })
    db.rollback_to("before_post")
}
```

The block commits when it completes and rolls back when an error escapes it.
A nested block marks a savepoint instead of beginning, so an inner failure
rolls back the inner block without killing the outer one. Returning early
from inside a block commits what ran so far; names bound inside stay inside.

`db.savepoint` and `db.rollback_to` only appear inside a block (`HS0230`), a
rollback names a savepoint the same block created (`HS0231`), the call takes
exactly one literal identifier name (`HS0232`), and the block itself is a
statement, not a value (`HS0233`).

## Batches

```hard
users <- User.create_many([{ email: "a@b.c", name: "Ada" }, { email: "b@c.d", name: "Bo" }])
users <- User.update_many([{ id: 1, name: "Ada" }])
n     <- User.delete_many([1, 2, 3])
users <- User.find_many([3, 1])
```

Every write batch runs in one transaction: a bad record fails the batch, not
half of it. `create_many` returns the records with their keys, in order.
`update_many` needs each record to carry its key and returns the records;
records that set the same columns share one prepared statement.
`delete_many` reports how many rows went; long id lists are cut into chunks
under the backend's variable cap. `find_many` returns rows in the keys'
order, skipping keys with no row. Literal batches are validated element by
element with the single-write rules.

## Migrations

```hard
hard migrate diff --name add_posts
hard migrate up
hard migrate down
hard migrate status
```

`migrate diff` compares the project's models against the models embedded in
the newest migration file and writes `migrations/NNNN_name.sql` with an up
side and a down side generated together. Each file records the schema
fingerprint it was generated from; a file edited afterwards fails the
fingerprint check instead of silently dropping the edit. `migrate up` applies
pending migrations oldest first, each in its own transaction with its history
row; `down` rolls back the newest first. History lives in `schema_migrations`
and remembers each file's checksum, so an edited file is `HS0223` rather than
a re-run. A migration file:

```sql
-- hardscript:migration 0001
-- hardscript:fingerprint <schema fingerprint>
-- hardscript:models
-- model User {
--     ...
-- }
-- hardscript:end
-- +migrate Up
CREATE TABLE "user" (...);
-- +migrate Down
DROP TABLE IF EXISTS "user";
```

## Seeds

```hard
hard seed
hard seed seeds/demo.sql
```

`hard seed` runs every `.sql` file in `seeds/`, or the file it is given, each
in one transaction: a seed lands whole or not at all. A seed that fails names
its file and statement (`HS0224`). Seeds are data, not history, and are not
recorded.

## Backend notes

SQLite is loaded at run time through a hand-declared ABI: the header is not
needed to build, and a program that never opens a database pays nothing for
it. The name comes from `HS_SQLITE_LIB` when set. Foreign keys are turned on
per connection, because SQLite leaves them off and a `REFERENCES` clause that
is silently not enforced is worse than none. There is no
`last_insert_rowid()` on PostgreSQL, so inserts ask for the key with
`RETURNING` and read it off the row.

PostgreSQL speaks the extended protocol: each statement is parsed once and
its values travel separately, with `$1` placeholders and `ILIKE` for
case-insensitive match. A statement that fails inside a transaction leaves it
failed until rollback; the backend names the statement that caused it, and
`ROLLBACK TO SAVEPOINT` is the way back to a running transaction.

## Diagnostics

The compiler checks queries, writes, relationships, transactions and batches
before anything runs. Every error names its code: `HS0211`–`HS0229` cover
schema, queries, writes, relationships and migrations; `HS0230`–`HS0233`
cover transactions. The catalog in `docs/errors.md` is generated from the
compiler and lists every code with its causes and fixes.

## Performance

`qa/bench_orm.sh` measures single-row operations, joins, transactions,
batches and migrations on both backends and writes `reports/orm-performance.md`,
`reports/db-performance.md` and `reports/migration-performance.md`. Every
cell in those reports comes from a run.
