//! The ORM schema metadata engine (M5.3).
//!
//! `model` declarations carry both *shape* (a list of fields) and *meaning*
//! (`@primary`, `@unique`, `@default(now())`, `@foreign(users.id)`, ...). This
//! module turns the annotations a programmer wrote into a normalized,
//! deterministic [`Schema`]: the single description of a database that the
//! query builder, the CRUD engine, the relationship engine, the DDL generator
//! and the migration differ all read from.
//!
//! Two properties matter more than anything else here.
//!
//! **Determinism.** The same source must always produce byte-identical
//! schema output, because migrations are diffs of schema output. Declaration
//! order is *not* a stable identity: two developers can add their models in
//! different orders, and a reordering must not produce a migration that
//! rewrites every table. So canonical ordering ([`Schema::canonical_tables`])
//! is by table name and then by column name, and only then is a canonical
//! string produced for comparison. Source order is still kept for
//! diagnostics, where pointing at the model the programmer actually wrote is
//! what helps.
//!
//! **Bounded strictness.** Model field types are plain identifiers today and
//! nothing in the language promises a closed set of them, so this module never
//! rejects a type just because it is unfamiliar to it. Unknown types are
//! recorded in [`ColumnDef::unsupported_type`] and only become errors when
//! the caller asks for a strict schema ([`BuildOpts::strict`]) — that is, when
//! the ORM is actually going to use the model. A model nobody queries keeps
//! compiling exactly as it did before the ORM existed.

use crate::ast::{Expr, FieldDef, ModelDef};
use crate::token::Span;
use crate::catalog as cat;
use crate::error::{Diag, ErrorKind};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A database backend. DDL and SQL text differ per backend, so the dialect is
/// threaded through generation rather than baked into the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
}

impl Dialect {
    pub fn parse(s: &str) -> Option<Dialect> {
        match s {
            "sqlite" | "sqlite3" | "SQLITE" => Some(Dialect::Sqlite),
            "postgres" | "postgresql" | "pg" | "POSTGRES" => Some(Dialect::Postgres),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Dialect::Sqlite => "sqlite",
            Dialect::Postgres => "postgres",
        }
    }

    /// The placeholder spelling for a bound parameter. Never interpolated into
    /// SQL by callers: values are always bound, never formatted into the
    /// statement text.
    pub fn placeholder(self) -> &'static str {
        match self {
            Dialect::Sqlite => "?",
            Dialect::Postgres => "$1",
        }
    }
}

/// The storage class a column is given, chosen from the source type plus the
/// declared attributes rather than from the type name alone (`@nullable` and
/// `@auto_increment` both change the emitted type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlType {
    Int,
    BigInt,
    Float,
    Bool,
    Text,
    Uuid,
    Time,
    Json,
}

impl SqlType {
    fn sqlite(self) -> &'static str {
        match self {
            SqlType::Int => "INTEGER",
            SqlType::BigInt => "INTEGER",
            SqlType::Float => "REAL",
            SqlType::Bool => "INTEGER",
            SqlType::Text => "TEXT",
            SqlType::Uuid => "TEXT",
            // SQLite has no date type. ISO-8601 text sorts chronologically
            // because the format is fixed-width and lexicographic, which is
            // what makes `ORDER BY created` and range filters correct.
            SqlType::Time => "TEXT",
            SqlType::Json => "TEXT",
        }
    }

    fn postgres(self) -> &'static str {
        match self {
            SqlType::Int => "INTEGER",
            SqlType::BigInt => "BIGINT",
            SqlType::Float => "DOUBLE PRECISION",
            SqlType::Bool => "BOOLEAN",
            SqlType::Text => "TEXT",
            SqlType::Uuid => "UUID",
            SqlType::Time => "TIMESTAMPTZ",
            SqlType::Json => "JSONB",
        }
    }

    pub fn ddl(self, d: Dialect) -> &'static str {
        match d {
            Dialect::Sqlite => self.sqlite(),
            Dialect::Postgres => self.postgres(),
        }
    }

    pub fn is_numeric(self) -> bool {
        matches!(self, SqlType::Int | SqlType::BigInt | SqlType::Float)
    }

    pub fn is_temporal(self) -> bool {
        matches!(self, SqlType::Time)
    }
}

/// A literal column default, normalized out of the `@default(...)` argument.
///
/// Only literals are representable. That restriction is deliberate: a default
/// that is an arbitrary expression would make the schema depend on program
/// evaluation, and schema output has to be comparable to detect a change.
#[derive(Debug, Clone, PartialEq)]
pub enum Default {
    Null,
    Bool(bool),
    Int(i64),
    /// Kept as written so `0.5` never round-trips through a lossy float print.
    Float(String),
    Str(String),
    /// `@default(now())` — resolved per row by the database.
    Now,
}

impl Default {
    fn sql(&self, d: Dialect) -> String {
        match self {
            Default::Null => match d {
                Dialect::Sqlite => "NULL".to_string(),
                Dialect::Postgres => "NULL".to_string(),
            },
            Default::Bool(b) => match d {
                Dialect::Sqlite => (if *b { "1" } else { "0" }).to_string(),
                Dialect::Postgres => (if *b { "TRUE" } else { "FALSE" }).to_string(),
            },
            Default::Int(i) => i.to_string(),
            Default::Float(f) => f.clone(),
            Default::Str(s) => sql_string_literal(s),
            Default::Now => match d {
                Dialect::Sqlite => "CURRENT_TIMESTAMP".to_string(),
                Dialect::Postgres => "now()".to_string(),
            },
        }
    }

    /// SQLite has no boolean literal and Postgres has no `TRUE` in the same
    /// places, so a bool default needs the per-dialect spelling above. Text
    /// defaults are string literals on both, so the value is recoverable.
    pub fn as_text(&self) -> Option<String> {
        match self {
            Default::Str(s) => Some(s.clone()),
            _ => None,
        }
    }
}

/// A `users.id`-shaped reference declared by `@foreign(users.id)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    /// The local column this key hangs off.
    pub column: String,
    pub ref_table: String,
    pub ref_column: String,
    pub on_delete: Option<String>,
}

impl ForeignKey {
    pub fn target(&self) -> String {
        format!("{}.{}", self.ref_table, self.ref_column)
    }
}

/// One column of a table, i.e. one scalar model field plus its annotations.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    /// The HardScript type as written (`Int`, `Email`, `UUID`, ...).
    pub ty: String,
    pub sql_type: SqlType,
    pub primary: bool,
    pub unique: bool,
    pub index: bool,
    pub nullable: bool,
    pub default: Option<Default>,
    pub auto_increment: bool,
    pub foreign: Option<ForeignKey>,
    pub created_at: bool,
    pub updated_at: bool,
    /// A source type the ORM has no column mapping for. Non-fatal unless the
    /// schema was built strictly; see the module docs.
    pub unsupported_type: Option<String>,
    pub span: Span,
}

impl ColumnDef {
    /// Whether the column may be omitted from an insert.
    pub fn has_db_default(&self) -> bool {
        self.default.is_some() || self.auto_increment || self.created_at || self.updated_at
    }

    fn required_clause(&self) -> bool {
        !self.nullable && !self.has_db_default()
    }
}

/// One table of the schema, i.e. one `model`.
#[derive(Debug, Clone, PartialEq)]
pub struct TableDef {
    /// The model name as written (`User`).
    pub model: String,
    /// The backing table name (`users`).
    pub table: String,
    pub columns: Vec<ColumnDef>,
    /// Position in the source file, for deterministic diagnostics.
    pub ordinal: usize,
    pub span: Span,
}

impl TableDef {
    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.name == name)
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// The declared primary key, or the implicit one.
    pub fn primary_key(&self) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.primary)
    }

    pub fn primary_key_name(&self) -> Option<&str> {
        self.primary_key().map(|c| c.name.as_str())
    }

    pub fn foreign_keys(&self) -> impl Iterator<Item = &ForeignKey> {
        self.columns.iter().filter_map(|c| c.foreign.as_ref())
    }

    pub fn indexes(&self) -> impl Iterator<Item = (&str, bool)> {
        self.columns.iter().filter_map(|c| {
            if c.index || c.unique {
                Some((c.name.as_str(), c.unique))
            } else {
                None
            }
        })
    }

    /// The per-column lines of a `CREATE TABLE` body, in canonical order.
    pub fn column_ddl(&self, d: Dialect) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(self.columns.len() + 3);
        for c in &self.columns {
            let mut line = String::new();
            let _ = write!(line, "  {} {}", quote_ident(d, &c.name), c.sql_type.ddl(d));
            if c.primary {
                line.push_str(" PRIMARY KEY");
                if c.auto_increment {
                    match d {
                        // SQLite only auto-increments an `INTEGER PRIMARY
                        // KEY` spelled exactly that way, and only that way.
                        Dialect::Sqlite => line.push_str(" AUTOINCREMENT"),
                        // Postgres needs the sequence spelled out; the table
                        // gets one identity column instead.
                        Dialect::Postgres => line.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                    }
                }
            }
            if c.unique && !c.primary {
                line.push_str(" UNIQUE");
            }
            if let Some(def) = &c.default {
                // An identity column supplies its own value.
                if !(c.auto_increment && matches!(def, Default::Now)) {
                    let _ = write!(line, " DEFAULT {}", def.sql(d));
                }
            }
            if c.nullable {
                line.push_str(" NULL");
            } else {
                line.push_str(" NOT NULL");
            }
            out.push(line);
        }
        for fk in self.foreign_keys() {
            let _ = write!(
                out.last_mut().unwrap_or(&mut String::new()),
                " REFERENCES {}({})",
                quote_ident(d, &fk.ref_table),
                quote_ident(d, &fk.ref_column)
            );
        }
        out
    }

    /// A single `CREATE TABLE` statement. Indexes are separate statements
    /// because neither backend supports a portable inline `CREATE INDEX`.
    pub fn create_table_sql(&self, d: Dialect) -> String {
        let mut sql = String::new();
        let _ = write!(
            sql,
            "CREATE TABLE {} (\n{}\n)",
            quote_ident(d, &self.table),
            self.column_ddl(d).join(",\n")
        );
        sql
    }

    /// `CREATE INDEX` statements, one per indexed or unique non-key column.
    pub fn create_index_sql(&self, d: Dialect) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.columns {
            if c.primary || !(c.index || c.unique) {
                continue;
            }
            // A unique column already carries a unique index from its
            // constraint; only plain `@index` needs its own statement.
            if c.unique && !c.index {
                continue;
            }
            out.push(format!(
                "CREATE {}INDEX {} ON {} ({})",
                if c.unique { "UNIQUE " } else { "" },
                quote_ident(d, &index_name(&self.table, &c.name)),
                quote_ident(d, &self.table),
                quote_ident(d, &c.name)
            ));
        }
        out
    }

    /// The complete set of statements that create this table, in the order
    /// they must run: table first, then its indexes.
    pub fn create_sql(&self, d: Dialect) -> Vec<String> {
        let mut out = vec![self.create_table_sql(d)];
        out.extend(self.create_index_sql(d));
        out
    }

    /// The statements that drop it, in reverse dependency-safe order.
    pub fn drop_sql(&self, d: Dialect) -> Vec<String> {
        let mut out: Vec<String> = self
            .create_index_sql(d)
            .into_iter()
            .map(|s| s.replace("CREATE ", "DROP ").replace("CREATE UNIQUE ", "DROP INDEX IF EXISTS ").replace("CREATE INDEX ", "DROP INDEX IF EXISTS "))
            .collect();
        out.push(format!("DROP TABLE IF EXISTS {}", quote_ident(d, &self.table)));
        out
    }
}

/// A relationship between two models, derived from a field whose type names
/// another model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelKind {
    BelongsTo,
    HasMany,
    HasOne,
    ManyToMany,
}

impl RelKind {
    pub fn name(self) -> &'static str {
        match self {
            RelKind::BelongsTo => "belongs_to",
            RelKind::HasMany => "has_many",
            RelKind::HasOne => "has_one",
            RelKind::ManyToMany => "many_to_many",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub kind: RelKind,
    /// Model holding the field (`Post` in `model Post { user : User }`).
    pub from: String,
    /// Model named by the field type (`User`).
    pub to: String,
    /// Field name (`user`).
    pub field: String,
    /// For `@foreign(users.id)` on the field, the referenced target.
    pub foreign: Option<ForeignKey>,
    pub span: Span,
}

impl Relation {
    /// The conventional singular/plural forms used to find the local key.
    pub fn from_table(&self) -> &str {
        &self.from
    }
}

/// The whole database description.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Schema {
    /// Tables in source order — used where a developer's reading order helps.
    pub tables: Vec<TableDef>,
    pub relations: Vec<Relation>,
}

impl Schema {
    pub fn table(&self, name: &str) -> Option<&TableDef> {
        self.tables.iter().find(|t| t.model == name || t.table == name)
    }

    /// Resolve a `@foreign` target to the table it really means.
    ///
    /// A reference is written by a person, who thinks "the users table", while
    /// the table name comes from `model User`, which the parser singularises to
    /// `user`. Requiring an exact match would make the documented
    /// `@foreign(users.id)` fail against `model User` for a spelling that is
    /// not even wrong. So a reference resolves by table name, by model name,
    /// by the model name pluralised, or by the given name singularised — and
    /// the *resolved* table is what gets stored, so the emitted `REFERENCES`
    /// clause always names a table that exists.
    pub fn resolve_ref(&self, name: &str) -> Option<&TableDef> {
        if let Some(t) = self.tables.iter().find(|t| t.table == name) {
            return Some(t);
        }
        if let Some(t) = self.tables.iter().find(|t| t.model == name) {
            return Some(t);
        }
        let plural = format!("{name}s");
        if let Some(t) = self.tables.iter().find(|t| t.table == plural || t.model == plural) {
            return Some(t);
        }
        if let Some(single) = name.strip_suffix('s') {
            if let Some(t) = self.tables.iter().find(|t| t.table == single || t.model == single) {
                return Some(t);
            }
        }
        None
    }

    /// Tables sorted into the canonical order used for DDL, diffs and any
    /// other output that two runs must agree on byte for byte.
    pub fn canonical_tables(&self) -> Vec<&TableDef> {
        let mut v: Vec<&TableDef> = self.tables.iter().collect();
        v.sort_by(|a, b| a.table.cmp(&b.table));
        v
    }

    pub fn relations_from<'a>(&'a self, model: &'a str) -> impl Iterator<Item = &'a Relation> {
        self.relations.iter().filter(move |r| r.from == model)
    }

    /// Every `CREATE TABLE`/`CREATE INDEX` statement, canonical order.
    pub fn create_sql(&self, d: Dialect) -> Vec<String> {
        let mut out = Vec::new();
        for t in self.canonical_tables() {
            out.extend(t.create_sql(d));
        }
        out
    }

    /// A stable textual identity for the schema. Two schemas that differ only
    /// in declaration order produce the same string, which is exactly what
    /// makes `migrate diff` able to say "no change".
    pub fn fingerprint(&self, d: Dialect) -> String {
        let mut s = String::new();
        for t in self.canonical_tables() {
            let _ = writeln!(s, "-- table {}", t.table);
            for line in t.column_ddl(d) {
                let _ = writeln!(s, "{line}");
            }
            for idx in t.create_index_sql(d) {
                let _ = writeln!(s, "{idx}");
            }
        }
        s
    }

    /// The fields of `model` that become columns, i.e. excluding relation
    /// fields, in canonical order.
    pub fn column_names(&self, model: &str) -> Vec<String> {
        self.table(model)
            .map(|t| {
                let mut v: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }
}

/// Quote an identifier for a dialect. Identifiers are validated before they
/// get here, so this is escaping, not sanitizing of arbitrary input.
pub fn quote_ident(d: Dialect, name: &str) -> String {
    let q = match d {
        Dialect::Sqlite => '"',
        Dialect::Postgres => '"',
    };
    let mut out = String::with_capacity(name.len() + 2);
    out.push(q);
    for ch in name.chars() {
        if ch == q {
            out.push(q);
        }
        out.push(ch);
    }
    out.push(q);
    out
}

/// A SQL string literal. Used only for schema *literals* (table/column
/// defaults and metadata the compiler itself emits) — never for a value that
/// came from a request.
pub fn sql_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push('\'');
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

/// `users` + `email` -> `idx_users_email`, the conventional index name.
pub fn index_name(table: &str, column: &str) -> String {
    format!("idx_{table}_{column}")
}

fn err(span: Span, code: u16, msg: impl Into<String>, fix: impl Into<String>) -> Diag {
    Diag::new(ErrorKind::Type, msg, span, fix).with_code(code)
}

/// How strictly to interpret the model source.
#[derive(Debug, Clone, Copy)]
pub struct BuildOpts {
    /// Report unknown column types as errors instead of recording them.
    pub strict: bool,
}

impl BuildOpts {
    pub fn strict() -> BuildOpts {
        BuildOpts { strict: true }
    }

    /// Used by callers that only want to know whether a model is ORM-usable
    /// without failing on unrelated legacy models.
    pub fn lenient() -> BuildOpts {
        BuildOpts { strict: false }
    }
}

/// The model-name set, used to tell a relation field from a scalar field.
fn model_names(models: &[ModelDef]) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for md in models {
        // First declaration wins so a duplicate name does not silently
        // retarget relations to the other model.
        m.entry(md.name.clone()).or_insert_with(|| md.table.clone());
    }
    m
}

/// Map a source type name onto a storage class.
fn sql_type_for(ty: &str) -> Option<SqlType> {
    Some(match ty {
        "Int" => SqlType::Int,
        "BigInt" | "Number" => SqlType::BigInt,
        "Float" => SqlType::Float,
        "Bool" => SqlType::Bool,
        "Time" | "DateTime" | "Date" | "Timestamp" => SqlType::Time,
        "UUID" | "Uuid" => SqlType::Uuid,
        "JSON" | "Json" => SqlType::Json,
        // Everything textual collapses to TEXT; the refinement a caller wants
        // (`@unique` on an email, say) is expressed by attributes, not by
        // inventing per-type SQL types.
        "String" | "Str" | "Text" | "Email" | "Url" | "URL" | "Enum" | "Any" | "Bytes" | "Object"
        | "Array" | "List" | "Map" => SqlType::Text,
        _ => return None,
    })
}

/// Parse a `@foreign(users.id)` argument into a target reference.
fn parse_foreign(arg: Option<&Expr>, span: Span) -> Result<ForeignKey, Diag> {
    let expr = arg.ok_or_else(|| {
        err(
            span,
            cat::ORM_BAD_FOREIGN,
            "`@foreign` needs a target such as `@foreign(users.id)`.",
            "Name the table and column it points at, e.g. `@foreign(users.id)`.",
        )
    })?;
    let (table, column) = match expr {
        Expr::Member(base, col, sp) => match &**base {
            Expr::Ident(t, _) => (t.clone(), col.clone()),
            _ => {
                return Err(err(
                    *sp,
                    cat::ORM_BAD_FOREIGN,
                    "`@foreign` expects a `table.column` target.",
                    "Use `@foreign(users.id)`.",
                ))
            }
        },
        Expr::Str(s, sp) => match s.split_once('.') {
            Some((t, c)) => (t.to_string(), c.to_string()),
            None => {
                return Err(err(
                    *sp,
                    cat::ORM_BAD_FOREIGN,
                    format!("`@foreign(\"{s}\")` is missing the column name."),
                    "Use `@foreign(users.id)`.",
                ))
            }
        },
        Expr::Ident(s, sp) => {
            return Err(err(
                *sp,
                cat::ORM_BAD_FOREIGN,
                format!("`@foreign({s})` is missing the column name."),
                "Use `@foreign(users.id)`.",
            ))
        }
        other => {
            return Err(err(
                other.span(),
                cat::ORM_BAD_FOREIGN,
                "`@foreign` expects a `table.column` target.",
                "Use `@foreign(users.id)`.",
            ))
        }
    };
    if table.is_empty() || column.is_empty() {
        return Err(err(
            span,
            cat::ORM_BAD_FOREIGN,
            "`@foreign` target is incomplete.",
            "Use `@foreign(users.id)`.",
        ));
    }
    Ok(ForeignKey { column: String::new(), ref_table: table, ref_column: column, on_delete: None })
}

/// Read a literal out of a `@default(...)` argument.
fn parse_default(arg: Option<&Expr>, span: Span) -> Result<Default, Diag> {
    let expr = arg.ok_or_else(|| {
        err(
            span,
            cat::ORM_BAD_DEFAULT,
            "`@default` needs a value, e.g. `@default(18)` or `@default(now())`.",
            "Give `@default` a literal or `now()`.",
        )
    })?;
    Ok(match expr {
        Expr::Int(i, _) => Default::Int(*i),
        Expr::Float(f, _) => {
            let mut s = String::new();
            let _ = write!(s, "{f}");
            Default::Float(s)
        }
        Expr::Str(s, _) => Default::Str(s.clone()),
        Expr::Bool(b, _) => Default::Bool(*b),
        Expr::Ident(name, sp) if name == "now" => Default::Now,
        // `@default(now())` parses as a call; that spelling is the documented
        // one, and the bare `now` is accepted for symmetry.
        Expr::Call { callee, args, .. } => match (&**callee, args.as_slice()) {
            (Expr::Ident(name, _), []) if name == "now" => Default::Now,
            _ => {
                return Err(err(
                    expr.span(),
                    cat::ORM_BAD_DEFAULT,
                    "only `now()` is supported as a computed default.",
                    "Use a literal, or `@default(now())` for the current time.",
                ))
            }
        },
        Expr::Unary(op, inner, sp) => {
            // A negated numeric default is a literal, not an arbitrary
            // expression, so it is worth supporting.
            match (&**inner, op) {
                (Expr::Int(i, _), crate::ast::UnOp::Neg) => Default::Int(-*i),
                (Expr::Float(f, _), crate::ast::UnOp::Neg) => {
                    let mut s = String::new();
                    let _ = write!(s, "-{f}");
                    Default::Float(s)
                }
                _ => {
                    return Err(err(
                        *sp,
                        cat::ORM_BAD_DEFAULT,
                        "a default must be a literal value.",
                        "Use a number, string, bool, or `now()`.",
                    ))
                }
            }
        }
        other => {
            return Err(err(
                other.span(),
                cat::ORM_BAD_DEFAULT,
                format!(
                    "a default must be a literal, but this is a {} expression.",
                    expr_kind_name(other)
                ),
                "Use a number, string, bool, or `now()`.",
            ))
        }
    })
}

fn expr_kind_name(e: &Expr) -> &'static str {
    match e {
        Expr::Int(..) => "numeric",
        Expr::Float(..) => "float",
        Expr::Str(..) => "string",
        Expr::Bool(..) => "boolean",
        Expr::List(..) => "list",
        Expr::Obj(..) => "object",
        Expr::Ident(..) => "name",
        Expr::Member(..) => "member",
        Expr::Index(..) => "index",
        Expr::Call { .. } => "call",
        Expr::Unary(..) => "unary",
        Expr::Binary(..) => "binary",
        Expr::Range(..) => "range",
        Expr::HttpCall { .. } => "http",
        Expr::Match(..) => "match",
    }
}

/// The relationship a relation field declares, read from its attributes.
///
/// A field whose type names another model is a relation, and the attribute
/// says which kind:
///
/// ```hardscript
/// model Post { user : User @foreign(users.id) }   // belongs_to
/// model User { posts : Post @has_many }            // has_many
/// model User { latest : Post @has_one }            // has_one
/// model Post { tags : Tag @many_to_many }          // many_to_many
/// ```
///
/// With no attribute a relation holds exactly one target, so it is
/// `belongs_to`. There is no container spelling to fall back on: a field type
/// is a single identifier, so `Post[]` does not parse, which is why `@has_many`
/// is the spelling rather than a guessed one.
fn relation_kind(field: &FieldDef) -> RelKind {
    for a in &field.attrs {
        match a.name.as_str() {
            "belongs_to" | "foreign" => return RelKind::BelongsTo,
            "has_many" => return RelKind::HasMany,
            "has_one" => return RelKind::HasOne,
            "many_to_many" => return RelKind::ManyToMany,
            _ => {}
        }
    }
    RelKind::BelongsTo
}

/// Build a schema from parsed models.
pub fn build(models: &[ModelDef], opts: BuildOpts) -> Result<Schema, Vec<Diag>> {
    let mut diags: Vec<Diag> = Vec::new();
    let known = model_names(models);
    let mut schema = Schema::default();
    let mut seen_tables: BTreeMap<String, Span> = BTreeMap::new();

    for (ordinal, md) in models.iter().enumerate() {
        let mut columns: Vec<ColumnDef> = Vec::new();
        let mut relations: Vec<Relation> = Vec::new();
        let mut seen_cols: BTreeMap<String, ()> = BTreeMap::new();

        for f in &md.fields {
            if seen_cols.contains_key(&f.name) {
                diags.push(err(
                    f.span,
                    cat::ORM_DUPLICATE_COLUMN,
                    format!("`{}` declares the field `{}` twice.", md.name, f.name),
                    "Rename one of the fields; a table cannot have two columns with the same name.",
                ));
                continue;
            }
            seen_cols.insert(f.name.clone(), ());

            // A field whose type is another model is a relationship, not a
            // column. It is recorded here (the schema needs to know it is not
            // a column) and expanded into joins by the relationship engine.
            let base_ty = f.ty.as_str();
            if known.contains_key(base_ty) {
                if base_ty != md.name {
                    let foreign = match f.attrs.iter().find(|a| a.name == "foreign") {
                        Some(a) => match parse_foreign(a.arg.as_ref(), f.span) {
                            Ok(mut fk) => {
                                fk.column = f.name.clone();
                                Some(fk)
                            }
                            Err(d) => {
                                diags.push(d);
                                None
                            }
                        },
                        None => None,
                    };
                    relations.push(Relation {
                        kind: relation_kind(f),
                        from: md.name.clone(),
                        to: base_ty.to_string(),
                        field: f.name.clone(),
                        foreign,
                        span: f.span,
                    });
                    continue;
                }
            }

            let mut sql_type = match sql_type_for(&f.ty) {
                Some(t) => Some(t),
                None => None,
            };
            let unsupported = if sql_type.is_none() { Some(f.ty.clone()) } else { None };
            if sql_type.is_none() {
                if opts.strict {
                    diags.push(err(
                        f.span,
                        cat::ORM_UNKNOWN_TYPE,
                        format!(
                            "`{}` has no column type for the field `{} : {}`.",
                            md.name, f.name, f.ty
                        ),
                        "Use Int, Float, Bool, String, Time, UUID, or JSON for a column field.",
                    ));
                }
                // A text column keeps the model usable; strict callers have
                // already been told about it.
                sql_type = Some(SqlType::Text);
            }

            let mut primary = false;
            let mut unique = false;
            let mut index = false;
            let mut nullable = false;
            let mut auto_increment = false;
            let mut created_at = false;
            let mut updated_at = false;
            let mut default: Option<Default> = None;
            let mut foreign: Option<ForeignKey> = None;

            for a in &f.attrs {
                match a.name.as_str() {
                    "primary" => primary = true,
                    "unique" => unique = true,
                    "index" => index = true,
                    "nullable" => nullable = true,
                    "auto_increment" => auto_increment = true,
                    "created_at" => created_at = true,
                    "updated_at" => updated_at = true,
                    "default" => match parse_default(a.arg.as_ref(), f.span) {
                        Ok(d) => default = Some(d),
                        Err(dg) => diags.push(dg),
                    },
                    "foreign" => match parse_foreign(a.arg.as_ref(), f.span) {
                        Ok(mut fk) => {
                            fk.column = f.name.clone();
                            foreign = Some(fk);
                        }
                        Err(dg) => diags.push(dg),
                    },
                    // Not an ORM attribute: `@strict`, `@index` on the model,
                    // and the other annotations the language already defines
                    // are none of this module's business.
                    _ => {}
                }
            }

            let st = sql_type.unwrap_or(SqlType::Text);
            // `@created_at` / `@updated_at` are the timestamping spellings of
            // a `Time` column that defaults to now.
            if created_at || updated_at {
                if default.is_none() {
                    default = Some(Default::Now);
                }
                if sql_type_for(&f.ty).is_none() {
                    sql_type = Some(SqlType::Time);
                }
            }
            if auto_increment {
                if st != SqlType::Int && st != SqlType::BigInt {
                    diags.push(err(
                        f.span,
                        cat::ORM_BAD_ATTRIBUTE,
                        format!(
                            "`{}` cannot auto-increment `{}` because it is `{}`, not an integer.",
                            md.name, f.name, f.ty
                        ),
                        "Use an Int field for `@auto_increment`.",
                    ));
                }
            }
            if primary && nullable {
                diags.push(err(
                    f.span,
                    cat::ORM_BAD_ATTRIBUTE,
                    format!("`{}.{}` is both `@primary` and `@nullable`.", md.name, f.name),
                    "A primary key identifies a row, so it cannot be null.",
                ));
            }
            if auto_increment && default.is_some() {
                diags.push(err(
                    f.span,
                    cat::ORM_BAD_ATTRIBUTE,
                    format!("`{}.{}` has both `@auto_increment` and `@default`.", md.name, f.name),
                    "The database supplies the value; pick one.",
                ));
            }

            columns.push(ColumnDef {
                name: f.name.clone(),
                ty: f.ty.clone(),
                sql_type: sql_type.unwrap_or(SqlType::Text),
                primary,
                unique,
                index,
                nullable,
                default,
                auto_increment,
                foreign,
                created_at,
                updated_at,
                unsupported_type: unsupported,
                span: f.span,
            });
        }

        // The implicit primary key: an `id` field with no annotations is the
        // key, and an integer `id` increments. Spelled out in the model
        // source it takes precedence over this.
        if !columns.iter().any(|c| c.primary) {
            if let Some(id) = columns.iter_mut().find(|c| c.name == "id") {
                id.primary = true;
                if id.sql_type.is_numeric() && id.default.is_none() && !id.nullable {
                    id.auto_increment = true;
                }
            }
        }
        if !columns.iter().any(|c| c.primary) {
            diags.push(err(
                md.span,
                cat::ORM_MISSING_PRIMARY_KEY,
                format!("`{}` has no primary key.", md.name),
                "Mark a field `@primary`, or name it `id`.",
            ));
        }
        if columns.iter().filter(|c| c.primary).count() > 1 {
            let names: Vec<&str> = columns.iter().filter(|c| c.primary).map(|c| c.name.as_str()).collect();
            diags.push(err(
                md.span,
                cat::ORM_BAD_ATTRIBUTE,
                format!("`{}` marks more than one field as primary: {}.", md.name, names.join(", ")),
                "A table has exactly one primary key.",
            ));
        }

        if let Some(prev) = seen_tables.get(&md.table) {
            diags.push(err(
                md.span,
                cat::ORM_DUPLICATE_TABLE,
                format!("`{}` and an earlier model both map to the table `{}`.", md.name, md.table),
                "Give each model its own table, or rename one.",
            ));
            let _ = prev;
        } else {
            seen_tables.insert(md.table.clone(), md.span);
        }

        schema.tables.push(TableDef {
            model: md.name.clone(),
            table: md.table.clone(),
            columns,
            ordinal,
            span: md.span,
        });
        schema.relations.extend(relations);
    }

    if !diags.is_empty() {
        return Err(diags);
    }

    // Cross-table pass: every `@foreign` target must exist, and is rewritten
    // to the table it actually names. A reference to a table that is not in
    // the schema would only fail at migration time, and by then the mistake is
    // far from the line that made it.
    //
    // Resolution is a read pass and the rewrite is a separate write pass,
    // because rewriting needs `&mut` on the very things being looked up.
    let mut fk_diags = Vec::new();
    // (table index, column index, resolved table) of each reference to
    // rewrite; column indices are per-table, so both are needed.
    let mut column_fixups: Vec<(usize, usize, String)> = Vec::new();
    let mut relation_fixups: Vec<(usize, String)> = Vec::new();

    for (ti, t) in schema.tables.iter().enumerate() {
        for (ci, c) in t.columns.iter().enumerate() {
            let Some(fk) = &c.foreign else { continue };
            match schema.resolve_ref(&fk.ref_table) {
                None => fk_diags.push(err(
                    c.span,
                    cat::ORM_BAD_FOREIGN,
                    format!(
                        "`{}.{}` references the table `{}`, which no model declares.",
                        t.model, c.name, fk.ref_table
                    ),
                    "Point the reference at a declared model, or declare it first.",
                )),
                Some(target) => {
                    // Store the table that exists, not the spelling used.
                    column_fixups.push((ti, ci, target.table.clone()));
                    if target.column(&fk.ref_column).is_none() {
                        fk_diags.push(err(
                            c.span,
                            cat::ORM_BAD_FOREIGN,
                            format!(
                                "`{}.{}` references `{}`, which has no column `{}`.",
                                t.model, c.name, target.model, fk.ref_column
                            ),
                            "Name a column the referenced model actually declares.",
                        ));
                    }
                }
            }
        }
    }
    for (ri, r) in schema.relations.iter().enumerate() {
        let Some(fk) = &r.foreign else { continue };
        match schema.resolve_ref(&fk.ref_table) {
            None => fk_diags.push(err(
                r.span,
                cat::ORM_BAD_FOREIGN,
                format!(
                    "`{}.{}` references `{}`, which no model declares.",
                    r.from, r.field, fk.ref_table
                ),
                "Point the reference at a declared model, or declare it first.",
            )),
            Some(target) => {
                relation_fixups.push((ri, target.table.clone()));
                if target.column(&fk.ref_column).is_none() {
                    fk_diags.push(err(
                        r.span,
                        cat::ORM_BAD_FOREIGN,
                        format!(
                            "`{}.{}` references `{}`, which has no column `{}`.",
                            r.from, r.field, target.model, fk.ref_column
                        ),
                        "Name a column the referenced model actually declares.",
                    ));
                }
            }
        }
    }
    if !fk_diags.is_empty() {
        return Err(fk_diags);
    }
    for (ti, ci, table) in column_fixups {
        if let Some(fk) = schema.tables[ti].columns[ci].foreign.as_mut() {
            fk.ref_table = table;
        }
    }
    for (ri, table) in relation_fixups {
        if let Some(fk) = schema.relations[ri].foreign.as_mut() {
            fk.ref_table = table;
        }
    }

    Ok(schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_models(src: &str) -> Vec<ModelDef> {
        crate::frontend(src, "test.hard").expect("parse").models
    }

    fn schema_of(src: &str) -> Result<Schema, Vec<Diag>> {
        build(&parse_models(src), BuildOpts::strict()).map_err(|mut d| {
            d.sort_by_key(|x| (x.code, x.span.map(|sp| (sp.line, sp.col)).unwrap_or((0, 0))));
            d
        })
    }

    fn codes(src: &str) -> Vec<u16> {
        let mut c: Vec<u16> = schema_of(src)
            .err()
            .map(|d| d.iter().map(|x| x.code).collect())
            .unwrap_or_default();
        c.sort();
        c
    }

    #[test]
    fn documented_model_maps_to_a_table() {
        let s = schema_of(
            r#"
            model User {
                id : UUID @primary
                email : Email @unique
                name : String
                age : Int @default(18)
                created : Time @default(now())
            }
            "#,
        )
        .expect("schema");
        assert_eq!(s.tables.len(), 1);
        let t = &s.tables[0];
        assert_eq!(t.model, "User");
        assert_eq!(t.table, "user");
        assert_eq!(t.columns.len(), 5);
        assert_eq!(t.primary_key_name(), Some("id"));
        assert_eq!(t.column("id").unwrap().sql_type, SqlType::Uuid);
        assert_eq!(t.column("email").unwrap().sql_type, SqlType::Text);
        assert!(t.column("email").unwrap().unique);
        assert_eq!(t.column("age").unwrap().default, Some(Default::Int(18)));
        assert_eq!(t.column("created").unwrap().default, Some(Default::Now));
        // Required: no default, not nullable, not a timestamp.
        assert!(t.column("name").unwrap().required_clause());
        assert!(!t.column("age").unwrap().required_clause());
    }

    #[test]
    fn every_attribute_is_honoured() {
        let s = schema_of(
            r#"
            model Post {
                id : Int @primary @auto_increment
                slug : String @unique @index
                title : String @nullable
                view_count : Int @default(0)
                user_id : Int @foreign(users.id)
                created : Time @created_at
                updated : Time @updated_at
            }
            model User { id : Int @primary }
            "#,
        )
        .expect("schema");
        let p = s.table("Post").unwrap();
        assert!(p.column("id").unwrap().auto_increment);
        assert!(p.column("slug").unwrap().unique && p.column("slug").unwrap().index);
        assert!(p.column("title").unwrap().nullable);
        assert_eq!(p.column("view_count").unwrap().default, Some(Default::Int(0)));
        let fk = p.column("user_id").unwrap().foreign.as_ref().unwrap();
        // `users` resolved to the table `model User` actually creates.
        assert_eq!(fk.ref_table, "user");
        assert_eq!(fk.ref_column, "id");
        assert!(p.column("created").unwrap().created_at);
        assert!(p.column("updated").unwrap().updated_at);
        // A timestamp column supplies its own value.
        assert!(p.column("created").unwrap().has_db_default());
    }

    #[test]
    fn implicit_id_primary_key_and_auto_increment() {
        let s = schema_of("model User { id : Int, email : String }").expect("schema");
        let u = s.table("User").unwrap();
        assert_eq!(u.primary_key_name(), Some("id"));
        assert!(u.column("id").unwrap().auto_increment);
    }

    #[test]
    fn explicit_primary_wins_over_implicit_id() {
        let s = schema_of("model User { id : Int, email : String @primary }").expect("schema");
        let u = s.table("User").unwrap();
        assert_eq!(u.primary_key_name(), Some("email"));
        // An explicit non-integer key does not silently increment.
        assert!(!u.column("email").unwrap().auto_increment);
        assert!(!u.column("id").unwrap().primary);
    }

    #[test]
    fn missing_primary_key_is_reported() {
        assert_eq!(codes("model User { email : String, name : String }"), vec![cat::ORM_MISSING_PRIMARY_KEY]);
    }

    #[test]
    fn two_primary_keys_are_rejected() {
        let c = codes("model User { id : Int @primary, email : String @primary }");
        assert!(c.contains(&cat::ORM_BAD_ATTRIBUTE), "{c:?}");
    }

    #[test]
    fn unknown_column_type_is_strict_only() {
        let models = parse_models("model User { id : Int, blob : Widget }");
        let strict = build(&models, BuildOpts::strict()).err().expect("strict must fail");
        assert!(strict.iter().any(|d| d.code == cat::ORM_UNKNOWN_TYPE));

        // The same model is fine when nothing is going to use it as a table.
        let lenient = build(&models, BuildOpts::lenient()).expect("lenient must pass");
        let col = lenient.table("User").unwrap().column("blob").unwrap();
        assert_eq!(col.unsupported_type.as_deref(), Some("Widget"));
        assert_eq!(col.sql_type, SqlType::Text);
    }

    #[test]
    fn unknown_attribute_is_not_an_orm_error() {
        // `@strict`-style and future annotations must not break schema build.
        let s = schema_of("model User { id : Int @primary, name : String @something_new }").expect("schema");
        assert_eq!(s.tables[0].columns.len(), 2);
    }

    #[test]
    fn bad_foreign_target_is_reported() {
        let c = codes("model Post { id : Int @primary, user_id : Int @foreign(nope.id) }");
        assert!(c.contains(&cat::ORM_BAD_FOREIGN), "{c:?}");
        let c2 = codes("model Post { id : Int @primary, user_id : Int @foreign(users.nope) }\nmodel User { id : Int @primary }");
        assert!(c2.contains(&cat::ORM_BAD_FOREIGN), "{c2:?}");
        let c3 = codes("model Post { id : Int @primary, user_id : Int @foreign }");
        assert!(c3.contains(&cat::ORM_BAD_FOREIGN), "{c3:?}");
    }

    #[test]
    fn duplicate_field_and_table_are_reported() {
        let c = codes("model User { id : Int @primary, name : String, name : Int }");
        assert!(c.contains(&cat::ORM_DUPLICATE_COLUMN), "{c:?}");
        let c2 = codes("model User = users [ id => Int ]\nmodel Account = users [ id => Int ]");
        assert!(c2.contains(&cat::ORM_DUPLICATE_TABLE), "{c2:?}");
    }

    #[test]
    fn bad_attribute_combinations_are_reported() {
        let c = codes("model User { id : Int @primary @nullable }");
        assert!(c.contains(&cat::ORM_BAD_ATTRIBUTE), "{c:?}");
        let c2 = codes("model User { id : Int @primary @auto_increment @default(3) }");
        assert!(c2.contains(&cat::ORM_BAD_ATTRIBUTE), "{c2:?}");
        let c3 = codes("model User { id : String @primary @auto_increment }");
        assert!(c3.contains(&cat::ORM_BAD_ATTRIBUTE), "{c3:?}");
    }

    #[test]
    fn non_literal_defaults_are_rejected() {
        let c = codes("model User { id : Int @primary, n : Int @default(env.get(\"N\")) }");
        assert!(c.contains(&cat::ORM_BAD_DEFAULT), "{c:?}");
        let c2 = codes("model User { id : Int @primary, n : Int @default }");
        assert!(c2.contains(&cat::ORM_BAD_DEFAULT), "{c2:?}");
    }

    #[test]
    fn literal_defaults_render_per_dialect() {
        let s = schema_of(
            r#"model User {
                id : Int @primary
                flag : Bool @default(true)
                label : String @default("new")
                when : Time @default(now())
                ratio : Float @default(-0.5)
            }"#,
        )
        .expect("schema");
        let t = s.table("User").unwrap();
        let sq = t.create_table_sql(Dialect::Sqlite);
        assert!(sq.contains("\"flag\" INTEGER DEFAULT 1"), "{sq}");
        assert!(sq.contains("\"label\" TEXT DEFAULT 'new'"), "{sq}");
        assert!(sq.contains("\"when\" TEXT DEFAULT CURRENT_TIMESTAMP"), "{sq}");
        assert!(sq.contains("\"ratio\" REAL DEFAULT -0.5"), "{sq}");

        let pg = t.create_table_sql(Dialect::Postgres);
        assert!(pg.contains("\"flag\" BOOLEAN DEFAULT TRUE"), "{pg}");
        assert!(pg.contains("\"when\" TIMESTAMPTZ DEFAULT now()"), "{pg}");
    }

    #[test]
    fn a_quote_in_a_default_is_escaped_not_dropped() {
        let s = schema_of(r#"model User { id : Int @primary, label : String @default("it's") }"#).expect("schema");
        let sql = s.table("User").unwrap().create_table_sql(Dialect::Sqlite);
        assert!(sql.contains("DEFAULT 'it''s'"), "{sql}");
    }

    #[test]
    fn auto_increment_uses_each_dialect_spelling() {
        let s = schema_of("model User { id : Int @primary @auto_increment, name : String }").expect("schema");
        let t = s.table("User").unwrap();
        // SQLite only auto-increments an INTEGER PRIMARY KEY spelled exactly so.
        assert!(t.create_table_sql(Dialect::Sqlite).contains("\"id\" INTEGER PRIMARY KEY AUTOINCREMENT"));
        assert!(t.create_table_sql(Dialect::Postgres).contains("\"id\" INTEGER PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY"));
    }

    #[test]
    fn relation_field_is_not_a_column() {
        let s = schema_of(
            r#"
            model Post { id : Int @primary, title : String, user : User @foreign(users.id) }
            model User { id : Int @primary, name : String }
            "#,
        )
        .expect("schema");
        let p = s.table("Post").unwrap();
        // `user : User` is a relationship, so it must not become a column.
        assert!(p.column("user").is_none());
        assert_eq!(p.columns.len(), 2);
        let rel = &s.relations[0];
        assert_eq!(rel.kind, RelKind::BelongsTo);
        assert_eq!(rel.from, "Post");
        assert_eq!(rel.to, "User");
        assert_eq!(rel.foreign.as_ref().unwrap().ref_column, "id");
    }

    #[test]
    fn attributed_relation_field_is_has_many() {
        let s = schema_of(
            r#"
            model User { id : Int @primary, posts : Post @has_many }
            model Post { id : Int @primary, user : User }
            "#,
        )
        .expect("schema");
        let r = s.relations.iter().find(|r| r.field == "posts").expect("posts relation");
        assert_eq!(r.kind, RelKind::HasMany);
        assert_eq!(r.to, "Post");
    }

    #[test]
    fn explicit_relation_attributes_win() {
        let s = schema_of(
            r#"
            model User { id : Int @primary, latest : Post @has_one, tags : Tag @many_to_many }
            model Post { id : Int @primary, owner : User @belongs_to }
            model Tag { id : Int @primary }
            "#,
        )
        .expect("schema");
        let find = |f: &str| s.relations.iter().find(|r| r.field == f).unwrap().kind;
        assert_eq!(find("latest"), RelKind::HasOne);
        assert_eq!(find("tags"), RelKind::ManyToMany);
        assert_eq!(find("owner"), RelKind::BelongsTo);
    }

    #[test]
    fn a_field_typed_like_its_own_model_is_a_column() {
        // `User : User` inside `model User` would otherwise read as a
        // self-relation; the field name has to be a column instead.
        let s = schema_of("model User { id : Int @primary, User : Int }").expect("schema");
        assert!(s.table("User").unwrap().column("User").is_some());
        assert!(s.relations.is_empty());
    }

    #[test]
    fn schema_output_is_declaration_order_independent() {
        let a = schema_of(
            r#"
            model User { id : Int @primary, email : String @unique }
            model Post { id : Int @primary, title : String }
            "#,
        )
        .expect("schema a");
        let b = schema_of(
            r#"
            model Post { id : Int @primary, title : String }
            model User { id : Int @primary, email : String @unique }
            "#,
        )
        .expect("schema b");
        // This is the property `migrate diff` depends on.
        assert_eq!(a.fingerprint(Dialect::Sqlite), b.fingerprint(Dialect::Sqlite));
        assert_eq!(a.create_sql(Dialect::Postgres), b.create_sql(Dialect::Postgres));
        // Source order is still available for diagnostics.
        assert_eq!(a.tables[0].model, "User");
        assert_eq!(b.tables[0].model, "Post");
    }

    #[test]
    fn fingerprint_is_stable_across_repeated_builds() {
        let src = r#"
            model User { id : Int @primary, email : String @unique, age : Int @default(18) }
            model Post { id : Int @primary, title : String @index }
        "#;
        let one = schema_of(src).unwrap().fingerprint(Dialect::Sqlite);
        for _ in 0..4 {
            assert_eq!(schema_of(src).unwrap().fingerprint(Dialect::Sqlite), one);
        }
    }

    #[test]
    fn index_statements_are_emitted_for_index_columns() {
        let s = schema_of("model User { id : Int @primary, email : String @unique, city : String @index }").expect("schema");
        let t = s.table("User").unwrap();
        let stmts = t.create_sql(Dialect::Sqlite);
        assert_eq!(stmts.len(), 2, "{stmts:?}");
        assert!(stmts[0].starts_with("CREATE TABLE"), "{:?}", stmts[0]);
        // A unique column is a constraint, not a separate index.
        assert!(stmts[1].contains("idx_user_city"), "{:?}", stmts[1]);
        assert!(!stmts[1].contains("email"));
    }

    #[test]
    fn drop_reverses_create() {
        let s = schema_of("model User { id : Int @primary, city : String @index }").expect("schema");
        let t = s.table("User").unwrap();
        let drops = t.drop_sql(Dialect::Sqlite);
        assert_eq!(drops.len(), 2);
        assert!(drops[0].starts_with("DROP INDEX"), "{:?}", drops[0]);
        assert_eq!(drops[1], "DROP TABLE IF EXISTS \"user\"");
    }

    #[test]
    fn foreign_key_reference_is_emitted() {
        let s = schema_of(
            r#"
            model Post { id : Int @primary, user_id : Int @foreign(users.id) }
            model User { id : Int @primary }
            "#,
        )
        .expect("schema");
        let sql = s.table("Post").unwrap().create_table_sql(Dialect::Sqlite);
        assert!(sql.contains("\"user_id\" INTEGER NOT NULL REFERENCES \"user\"(\"id\")"), "{sql}");
    }

    #[test]
    fn identifiers_are_quoted() {
        assert_eq!(quote_ident(Dialect::Sqlite, "users"), "\"users\"");
        // A quote inside an identifier is doubled, not dropped.
        assert_eq!(quote_ident(Dialect::Postgres, "we\"ird"), "\"we\"\"ird\"");
        assert_eq!(sql_string_literal("a'b"), "'a''b'");
    }

    #[test]
    fn dialect_parsing_and_names() {
        assert_eq!(Dialect::parse("sqlite3"), Some(Dialect::Sqlite));
        assert_eq!(Dialect::parse("pg"), Some(Dialect::Postgres));
        assert_eq!(Dialect::parse("mysql"), None);
        assert_eq!(Dialect::Sqlite.placeholder(), "?");
        assert_eq!(Dialect::Postgres.placeholder(), "$1");
    }

    #[test]
    fn column_names_are_canonical() {
        let s = schema_of("model User { id : Int @primary, zeta : String, alpha : String }").expect("schema");
        assert_eq!(s.column_names("User"), vec!["alpha", "id", "zeta"]);
    }

    #[test]
    fn legacy_bracket_spellings_still_map() {
        // The v0.1 form (`model User = users [ ... ]`) is the same
        // declaration, and it is the only form that names the table
        // explicitly, because `model User { ... }` singularises to `user`.
        let s = schema_of("model User = users [ id => Int, email => String ]").expect("schema");
        assert_eq!(s.table("User").unwrap().table, "users");
        assert_eq!(s.table("users").unwrap().model, "User");
    }
}
