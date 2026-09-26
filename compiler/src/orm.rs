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

    /// Whether an insert has to supply this column. The ORM's create path
    /// checks this so a missing value is a compile error rather than a
    /// constraint violation at runtime.
    pub fn required_clause(&self) -> bool {
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
    /// The columns that need an index statement of their own.
    ///
    /// Both the create and the drop side read this list, so a drop can never
    /// name an index the create did not make, or miss one it did.
    fn standalone_index_columns(&self) -> Vec<&ColumnDef> {
        self.columns
            .iter()
            .filter(|c| {
                if c.primary || !(c.index || c.unique) {
                    return false;
                }
                // A unique column already carries a unique index from its
                // constraint, so only an explicit `@index` adds another.
                c.index
            })
            .collect()
    }

    pub fn create_index_sql(&self, d: Dialect) -> Vec<String> {
        self.standalone_index_columns()
            .into_iter()
            .map(|c| {
                format!(
                    "CREATE {}INDEX {} ON {} ({})",
                    if c.unique { "UNIQUE " } else { "" },
                    quote_ident(d, &index_name(&self.table, &c.name)),
                    quote_ident(d, &self.table),
                    quote_ident(d, &c.name)
                )
            })
            .collect()
    }

    /// The complete set of statements that create this table, in the order
    /// they must run: table first, then its indexes.
    pub fn create_sql(&self, d: Dialect) -> Vec<String> {
        let mut out = vec![self.create_table_sql(d)];
        out.extend(self.create_index_sql(d));
        out
    }

    /// The statements that drop it, in reverse dependency-safe order.
    /// A copy of the table with its columns sorted by name.
    pub fn canonical(&self) -> TableDef {
        let mut t = self.clone();
        t.columns.sort_by(|a, b| a.name.cmp(&b.name));
        t
    }

    pub fn drop_sql(&self, d: Dialect) -> Vec<String> {
        // Generated from the columns rather than by rewriting the create
        // statements: `DROP INDEX` takes no `ON` clause, and a unique index
        // has to keep its `UNIQUE` word out of the drop entirely.
        let mut out: Vec<String> = self
            .standalone_index_columns()
            .into_iter()
            .map(|c| format!("DROP INDEX IF EXISTS {}", quote_ident(d, &index_name(&self.table, &c.name))))
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
            // Fields are sorted here so that reordering them in the model is
            // not mistaken for a schema change. A fresh `create_all` still
            // emits columns in the order they were written.
            let t = t.canonical();
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

/// A diagnostic with an ORM code and a suggested fix.
pub fn err(span: Span, code: u16, msg: impl Into<String>, fix: impl Into<String>) -> Diag {
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

/// A comparison usable in a `where` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    pub fn sql(self) -> &'static str {
        match self {
            CompareOp::Eq => "=",
            CompareOp::Ne => "<>",
            CompareOp::Lt => "<",
            CompareOp::Le => "<=",
            CompareOp::Gt => ">",
            CompareOp::Ge => ">=",
        }
    }

    pub fn from_binop(op: crate::ast::BinOp) -> Option<CompareOp> {
        use crate::ast::BinOp::*;
        Some(match op {
            Eq => CompareOp::Eq,
            Ne => CompareOp::Ne,
            Lt => CompareOp::Lt,
            Le => CompareOp::Le,
            Gt => CompareOp::Gt,
            Ge => CompareOp::Ge,
            _ => return None,
        })
    }
}

/// One builder call, in source order.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryStep {
    /// `where(col = value)`, `where(col, value)`, `where_like(col, pat)`,
    /// `where_in(col, [..])`. `value` is an index into the call's argument
    /// list, so the plan stays independent of how a value is generated.
    Where { column: String, op: CompareOp, arg: usize, kind: WhereKind },
    Limit(i64),
    Offset(i64),
    OrderBy { column: String, desc: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhereKind {
    Compare,
    Like,
    In,
}

/// What a query returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    All,
    First,
    Count,
    Exists,
}

impl Terminal {
    pub fn name(self) -> &'static str {
        match self {
            Terminal::All => "all",
            Terminal::First => "first",
            Terminal::Count => "count",
            Terminal::Exists => "exists",
        }
    }
}

/// A validated ORM query: the model it runs against and the builder calls
/// that shape it, with every column name checked against the schema.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryPlan {
    pub model: String,
    pub table: String,
    pub steps: Vec<QueryStep>,
    pub terminal: Terminal,
}

impl QueryPlan {
    /// Columns referenced by the query, in canonical order — the smallest set
    /// a `SELECT` needs. `None` means every column.
    pub fn projected_columns(&self, schema: &Schema) -> Option<Vec<String>> {
        let t = schema.table(&self.model)?;
        let mut v: Vec<String> = Vec::new();
        for s in &self.steps {
            let c = match s {
                QueryStep::Where { column, .. } | QueryStep::OrderBy { column, .. } => column,
                _ => continue,
            };
            if t.column(c).is_none() {
                return None;
            }
            v.push(c.clone());
        }
        if v.is_empty() {
            return None;
        }
        v.sort();
        v.dedup();
        Some(v)
    }
}

/// Methods that finish a query. Anything after one of these is a mistake,
/// because the result is rows, not a builder.
pub fn terminal_of(name: &str) -> Option<Terminal> {
    Some(match name {
        "all" => Terminal::All,
        "first" => Terminal::First,
        "count" => Terminal::Count,
        "exists" => Terminal::Exists,
        _ => return None,
    })
}

/// Whether a method finishes a query rather than shaping one.
pub fn is_orm_terminal(name: &str) -> bool {
    terminal_of(name).is_some()
}

/// Builder methods, whether or not they appear in a chain.
pub fn is_orm_method(name: &str) -> bool {
    terminal_of(name).is_some()
        || matches!(
            name,
            "where" | "where_like" | "where_in" | "limit" | "offset" | "order_by" | "find"
        )
}

/// The bare identifier a call chain is rooted at, if it is a chain at all.
///
/// Callers use this to decide *whether* a chain is a query, because
/// `orm_chain` deliberately only describes shape: `json.parse(..)` has the same
/// shape as `User.all()`, and treating it as a query would report a missing
/// model for a perfectly good module call.
pub fn chain_root(call: &Expr) -> Option<String> {
    orm_chain(call).map(|(m, _, _)| m)
}

/// The outermost call of a chain: `(Model, [(method, args, span)], span)`.
///
/// Walks `Model.a(1).b(2)` down to the `Model` and returns the calls in
/// source order. Returns `None` for anything that is not a chain rooted at a
/// bare identifier, so a non-ORM call is left alone.
pub fn orm_chain(call: &Expr) -> Option<(String, Vec<(String, Vec<Expr>, Span)>, Span)> {
    let Expr::Call { callee, args, span } = call else { return None };
    let mut steps: Vec<(String, Vec<Expr>, Span)> = Vec::new();
    let mut cur = Expr::Call {
        callee: callee.clone(),
        args: args.clone(),
        span: *span,
    };
    let outer_span = *span;
    let model = loop {
        match cur {
            Expr::Call { callee, args, span } => {
                let Expr::Member(base, name, _) = *callee else { return None };
                steps.push((name, args, span));
                match *base {
                    Expr::Ident(m, _) => break m,
                    inner @ Expr::Call { .. } => cur = inner,
                    _ => return None,
                }
            }
            _ => return None,
        }
    };
    steps.reverse();
    Some((model, steps, outer_span))
}

/// Parse and validate an ORM chain against the schema.
///
/// This is where the query builder is *typed*: every column a chain names is
/// checked against the model here, so `User.where(emial = x)` is a compile
/// error naming the typo, rather than a SQL error naming a missing column
/// discovered at request time.
pub fn parse_query(call: &Expr, schema: &Schema) -> Result<QueryPlan, Vec<Diag>> {
    let (model, raw_steps, span) = orm_chain(call).ok_or_else(|| {
        vec![err(
            call.span(),
            cat::ORM_BAD_QUERY,
            "this is not a query on a model.",
            "Start from a model, e.g. `User.all()`.",
        )]
    })?;
    let table = schema.table(&model).ok_or_else(|| {
        vec![err(
            span,
            cat::ORM_UNKNOWN_MODEL,
            format!("no model named `{model}` is declared."),
            "Check the spelling against the `model` declarations in this file.",
        )]
    })?;

    let mut diags: Vec<Diag> = Vec::new();
    let mut steps: Vec<QueryStep> = Vec::new();
    let mut terminal: Option<Terminal> = None;
    // `find` implies a terminal without being spelled like one, so a later
    // explicit terminal is a conflict rather than an override.
    let mut terminal_is_fixed = false;
    let mut seen_terminal_at: Option<usize> = None;

    for (i, (name, args, sp)) in raw_steps.iter().enumerate() {
        if let Some(t) = terminal_of(name) {
            if i + 1 != raw_steps.len() {
                diags.push(err(
                    *sp,
                    cat::ORM_BAD_QUERY,
                    format!("`{name}` runs the query, so nothing can be chained after it."),
                    format!("Move the `{}` call last, or start a new query.", name),
                ));
            }
            if terminal_is_fixed && terminal != Some(t) {
                diags.push(err(
                    *sp,
                    cat::ORM_BAD_QUERY,
                    format!("`find` already fixes the result to a single `{model}`, so `{name}` cannot follow it."),
                    format!("Use `{name}` without `find`, or keep the `find` lookup."),
                ));
            }
            terminal = Some(t);
            seen_terminal_at = Some(i);
            continue;
        }
        if let Some(at) = seen_terminal_at {
            let _ = at;
            continue;
        }
        match name.as_str() {
            "find" => {
                if args.len() != 1 {
                    diags.push(arity_err(*sp, "find", 1, args.len()));
                    continue;
                }
                // `find` is a lookup by primary key, so it is sugar for a
                // comparison against the key column.
                let Some(pk) = table.primary_key_name() else {
                    diags.push(err(
                        *sp,
                        cat::ORM_MISSING_PRIMARY_KEY,
                        format!("`{model}` has no primary key to find by."),
                        "Give the model a primary key, or use `where` with an explicit column.",
                    ));
                    continue;
                };
                // `find` is a single-row lookup, so it also fixes the
                // terminal: `User.find(1).limit(5)` would be a query for a
                // set the caller has already said cannot exist.
                terminal = Some(Terminal::First);
                terminal_is_fixed = true;
                steps.push(QueryStep::Where {
                    column: pk.to_string(),
                    op: CompareOp::Eq,
                    arg: 0,
                    kind: WhereKind::Compare,
                });
            }
            "where" | "where_like" | "where_in" => {
                let kind = match name.as_str() {
                    "where_like" => WhereKind::Like,
                    "where_in" => WhereKind::In,
                    _ => WhereKind::Compare,
                };
                if let Some(s) = parse_where(name, args, *sp, table, kind, &mut diags) {
                    steps.push(s);
                }
            }
            "limit" | "offset" => {
                if args.len() != 1 {
                    diags.push(arity_err(*sp, name, 1, args.len()));
                    continue;
                }
                match &args[0] {
                    Expr::Int(n, _) => {
                        let v = *n;
                        if v < 0 {
                            diags.push(err(
                                *sp,
                                cat::ORM_BAD_QUERY,
                                format!("`{name}` cannot be {v}."),
                                format!("Pass a count of 0 or more to `{name}`."),
                            ));
                            continue;
                        }
                        steps.push(if name == "limit" {
                            QueryStep::Limit(v)
                        } else {
                            QueryStep::Offset(v)
                        });
                    }
                    other => {
                        // A runtime count is expressible, but only a literal can
                        // be checked here and a silently-unchecked limit is how
                        // `LIMIT ?` with a negative value reaches a database.
                        diags.push(err(
                            other.span(),
                            cat::ORM_BAD_QUERY,
                            format!("`{name}` needs a literal count."),
                            "Pass the number directly, e.g. `.limit(10)`.",
                        ));
                    }
                }
            }
            "order_by" => {
                if args.len() != 2 {
                    diags.push(arity_err(*sp, "order_by", 2, args.len()));
                    continue;
                }
                let col = match &args[0] {
                    Expr::Ident(c, _) => c.clone(),
                    Expr::Str(c, _) => c.clone(),
                    other => {
                        diags.push(err(
                            other.span(),
                            cat::ORM_BAD_ORDER,
                            "`order_by` needs a field name.",
                            "Name the field, e.g. `.order_by(created, desc)`.",
                        ));
                        continue;
                    }
                };
                if table.column(&col).is_none() {
                    diags.push(unknown_col_err(*sp, &model, &col, table, cat::ORM_BAD_ORDER));
                    continue;
                }
                let desc = match &args[1] {
                    Expr::Ident(d, _) if d == "desc" => true,
                    Expr::Ident(d, _) if d == "asc" => false,
                    Expr::Str(d, _) if d == "desc" => true,
                    Expr::Str(d, _) if d == "asc" => false,
                    other => {
                        diags.push(err(
                            other.span(),
                            cat::ORM_BAD_ORDER,
                            "`order_by` takes `asc` or `desc` as the direction.",
                            "Write `.order_by(created, desc)` or `.order_by(created, asc)`.",
                        ));
                        let _ = other;
                        continue;
                    }
                };
                steps.push(QueryStep::OrderBy { column: col, desc });
            }
            other => {
                diags.push(err(
                    *sp,
                    cat::ORM_BAD_QUERY,
                    format!("`{other}` is not a query method."),
                    "A query uses all, first, count, exists, where, where_like, where_in, limit, offset, or order_by; a write uses create, upsert, delete, find_or_create_by, save, touch, or destroy.",
                ));
            }
        }
    }

    if !diags.is_empty() {
        return Err(diags);
    }
    Ok(QueryPlan {
        model,
        table: table.table.clone(),
        steps,
        terminal: terminal.unwrap_or(Terminal::All),
    })
}

// ---- writes (M5.3.3) ------------------------------------------------------

/// The fields of a write's object literal, as `(name, value, span)`.
pub type WriteFields = Vec<(String, Expr, Span)>;

/// The field names of a write, which is what a test usually wants to compare.
pub fn write_field_names(fields: &WriteFields) -> Vec<&str> {
    fields.iter().map(|(n, _, _)| n.as_str()).collect()
}

/// A write against one model: `Model.create(..)`, `Model.upsert(..)`,
/// `Model.delete(..)`, `Model.find_or_create_by(..)`, and a record's own
/// `save`, `touch` and `destroy`.
///
/// A write is validated the same way a read is. The object literal is the
/// interesting part: its keys are checked against the model's columns, and a
/// column with neither a value nor a database default has to be an error here
/// rather than a constraint violation the caller discovers in production.
#[derive(Debug, Clone)]
pub enum WriteOp {
    /// `User.create({..})` — insert.
    Create(WriteFields),
    /// `User.upsert({..})` — update by key, insert when there is no such row.
    Upsert(WriteFields),
    /// `User.delete(key)` — remove by key.
    DeleteKey,
    /// `User.find_or_create_by({..})` — read by attributes, insert if absent.
    FindOrCreate(WriteFields),
    /// `record.save()` — update the row it came from, or insert when it has
    /// no key yet.
    Save,
    /// `record.touch()` — refresh its `@updated_at`.
    Touch,
    /// `record.destroy()` — delete the row it came from.
    Destroy,
}

#[derive(Debug, Clone)]
pub struct WritePlan {
    pub model: String,
    pub table: String,
    pub op: WriteOp,
    /// For a record method, the model is known from the object's type rather
    /// than from the root of the chain.
    pub on_record: bool,
    pub span: Span,
}

/// Is this a write rather than a read? The two are told apart by their
/// methods, so a chain is classified before it is validated.
pub fn is_orm_write_method(name: &str) -> bool {
    matches!(name, "create" | "upsert" | "delete" | "find_or_create_by" | "save" | "touch" | "destroy")
}

pub fn is_orm_write_method_on_record(name: &str) -> bool {
    matches!(name, "save" | "touch" | "destroy")
}

/// A one-method call like `record.save()`, if that is what `call` is.
pub fn record_write(call: &Expr) -> Option<(String, Vec<Expr>, Span)> {
    let Expr::Call { callee, args, span } = call else { return None };
    let Expr::Member(base, name, _) = callee.as_ref() else { return None };
    // Only a bare name can be a record: `req.body.save()` is something else.
    if !matches!(base.as_ref(), Expr::Ident(..)) {
        return None;
    }
    if !is_orm_write_method_on_record(name) {
        return None;
    }
    Some((name.clone(), args.clone(), *span))
}

/// Validate a write, given the model it targets.
///
/// `model` is resolved by the caller: for `Model.create(..)` from the root
/// identifier, for `record.save()` from the record's declared type.
pub fn parse_write(model: &str, method: &str, args: &[Expr], on_record: bool, schema: &Schema) -> Result<WritePlan, Vec<Diag>> {
    let Some(table) = schema.table(model) else {
        return Err(vec![err(
            args.first().map(|a| a.span()).unwrap_or_default(),
            cat::ORM_UNKNOWN_MODEL,
            format!("no model named `{model}`."),
            "Check the name, or declare the model before using it.",
        )]);
    };
    let span = args.first().map(|a| a.span()).unwrap_or_default();
    let mut diags: Vec<Diag> = Vec::new();

    let op = if on_record {
        match method {
            "save" if !args.is_empty() => {
                diags.push(arity_err(span, method, 0, args.len()));
                WriteOp::Save
            }
            "save" => WriteOp::Save,
            "touch" if !args.is_empty() => {
                diags.push(arity_err(span, method, 0, args.len()));
                WriteOp::Touch
            }
            "touch" => WriteOp::Touch,
            "destroy" if !args.is_empty() => {
                diags.push(arity_err(span, method, 0, args.len()));
                WriteOp::Destroy
            }
            "destroy" => WriteOp::Destroy,
            other => {
                diags.push(bad_write(span, other));
                WriteOp::Save
            }
        }
    } else {
        match method {
            "create" | "upsert" | "find_or_create_by" => {
                if args.len() != 1 {
                    diags.push(arity_err(span, method, 1, args.len()));
                    WriteOp::Create(Vec::new())
                } else {
                    // Only a create has to be complete: an upsert updates the
                    // fields it is given, and a find names attributes rather
                    // than a whole row.
                    let fields = check_write_fields(model, table, &args[0], &mut diags, method);
                    match method {
                        "create" => WriteOp::Create(fields),
                        "upsert" => WriteOp::Upsert(fields),
                        _ => WriteOp::FindOrCreate(fields),
                    }
                }
            }
            "delete" => {
                if args.len() != 1 {
                    diags.push(arity_err(span, method, 1, args.len()));
                }
                WriteOp::DeleteKey
            }
            other => {
                diags.push(bad_write(span, other));
                WriteOp::DeleteKey
            }
        }
    };

    if !diags.is_empty() {
        return Err(diags);
    }
    Ok(WritePlan { model: model.to_string(), table: table.table.clone(), op, on_record, span })
}

fn bad_write(span: Span, name: &str) -> Diag {
    err(
        span,
        cat::ORM_BAD_QUERY,
        format!("`{name}` is not a write method."),
        "Use `create`, `upsert`, `delete` or `find_or_create_by` on a model, or `save`, `touch` and `destroy` on a record.",
    )
}

/// How much of a row a write's object has to carry.
#[derive(Debug, Clone, Copy, PartialEq)]
enum WriteCompleteness {
    /// `create` — every column the database will not fill in.
    Complete,
    /// `upsert` — the fields being changed; the rest are left alone.
    Partial,
    /// `find_or_create_by` — attributes to match on, and the row to insert if
    /// nothing matches, so it is complete enough to insert.
    Attributes,
}

/// Check an object literal's keys against the model.
fn check_write_fields(
    model: &str,
    table: &TableDef,
    arg: &Expr,
    diags: &mut Vec<Diag>,
    method: &str,
) -> WriteFields {
    let completeness = match method {
        "upsert" => WriteCompleteness::Partial,
        "find_or_create_by" => WriteCompleteness::Attributes,
        _ => WriteCompleteness::Complete,
    };
    // An upsert identifies its row *by* the key, so naming it is the point.
    let key_is_writable = completeness == WriteCompleteness::Partial;
    let require_all = completeness != WriteCompleteness::Partial;
    let Expr::Obj(fields, span) = arg else {
        diags.push(err(
            arg.span(),
            cat::ORM_BAD_QUERY,
            format!("`{model}` writes take an object of column values."),
            "Write `{ column: value }`, naming the model's fields.",
        ));
        return Vec::new();
    };
    let mut out: Vec<(String, Expr, Span)> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (name, value) in fields {
        let fsp = value.span();
        let Some(col) = table.column(name) else {
            diags.push(err(
                fsp,
                cat::ORM_UNKNOWN_COLUMN,
                format!("`{model}` has no field `{name}`."),
                suggest_field(table, name),
            ));
            continue;
        };
        if seen.contains(&name.as_str()) {
            diags.push(err(
                fsp,
                cat::ORM_DUPLICATE_COLUMN,
                format!("`{name}` is given twice."),
                "Keep one value per field.",
            ));
            continue;
        }
        // A value for a generated column is refused on a key the database
        // owns: an insert that names its own auto-increment key is either
        // ignored or a constraint violation, depending on the backend.
        if col.auto_increment && !key_is_writable {
            diags.push(err(
                fsp,
                cat::ORM_BAD_DEFAULT,
                format!("`{model}.{name}` is generated by the database."),
                "Leave the field out and read it back from the created record, or use `upsert` to name the row.",
            ));
            continue;
        }
        seen.push(name);
        out.push((name.clone(), value.clone(), fsp));
    }

    if require_all {
        for col in &table.columns {
            if col.required_clause() && !seen.contains(&col.name.as_str()) {
                diags.push(err(
                    *span,
                    cat::ORM_MISSING_FIELD,
                    format!("`{model}.{}` has no default, so a write must give it a value.", col.name),
                    format!("Add `{}` to the object, or give the field a default.", col.name),
                ));
            }
        }
    }
    out
}

fn suggest_field(table: &TableDef, name: &str) -> String {
    let mut best: Option<(&str, usize)> = None;
    for col in &table.columns {
        let d = edit_distance(name, &col.name);
        if d <= 2 && best.map(|(_, bd)| d < bd).unwrap_or(true) {
            best = Some((&col.name, d));
        }
    }
    match best {
        Some((n, _)) => format!("Did you mean `{n}`?"),
        None => format!("`{}` has: {}.", table.model, column_list(table)),
    }
}

fn column_list(table: &TableDef) -> String {
    table.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn arity_err(span: Span, name: &str, want: usize, got: usize) -> Diag {
    err(
        span,
        cat::ORM_BAD_QUERY,
        format!("`{name}` takes {want} argument{}, but {got} were given.", if want == 1 { "" } else { "s" }),
        format!("Call `{name}` with {want} argument{}.", if want == 1 { "" } else { "s" }),
    )
}

fn unknown_col_err(span: Span, model: &str, col: &str, table: &TableDef, code: u16) -> Diag {
    let mut declared: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    declared.sort();
    let names: Vec<String> = declared.iter().map(|s| s.to_string()).collect();
    let hint = crate::suggest::closest(col, &names);
    err(
        span,
        code,
        format!("`{model}` has no field `{col}`."),
        match hint {
            Some(h) => format!("Did you mean `{h}`?"),
            None => format!("`{model}` declares: {}.", declared.join(", ")),
        },
    )
}

/// Parse the argument forms of `where`.
fn parse_where(
    name: &str,
    args: &[Expr],
    sp: Span,
    table: &TableDef,
    kind: WhereKind,
    diags: &mut Vec<Diag>,
) -> Option<QueryStep> {
    let model = &table.model;
    match kind {
        WhereKind::Compare => {
            // `where(col = value)` — the documented spelling.
            if args.len() == 1 {
                let Expr::Binary(op, _l, _r, bsp) = &args[0] else {
                    diags.push(err(
                        args[0].span(),
                        cat::ORM_BAD_QUERY,
                        format!("`{name}` needs a comparison, e.g. `where({model}.field = value)`."),
                        "Write `where(column = value)` or `where(column, value)`.",
                    ));
                    return None;
                };
                let Some(cmp) = CompareOp::from_binop(*op) else {
                    diags.push(err(
                        *bsp,
                        cat::ORM_BAD_QUERY,
                        "a `where` clause cannot use that operator.",
                        "Compare with =, !=, <, <=, > or >=.",
                    ));
                    return None;
                };
                let col = column_name(_l, model, table, diags)?;
                return Some(QueryStep::Where { column: col, op: cmp, arg: 0, kind });
            }
            // `where(col, value)` or `where(col, value, op)`.
            if args.len() == 2 || args.len() == 3 {
                let col = column_name(&args[0], model, table, diags)?;
                let op = if args.len() == 3 {
                    let Some(c) = compare_from(&args[2]) else {
                        diags.push(err(
                            args[2].span(),
                            cat::ORM_BAD_QUERY,
                            "the third argument of `where` must be a comparison operator.",
                            "Use one of =, !=, <, <=, >, >= as a string, e.g. `where(age, 20, \">\")`.",
                        ));
                        return None;
                    };
                    c
                } else {
                    CompareOp::Eq
                };
                return Some(QueryStep::Where { column: col, op, arg: 1, kind });
            }
            diags.push(arity_err(sp, name, 1, args.len()));
            None
        }
        WhereKind::Like => {
            if args.len() != 2 {
                diags.push(arity_err(sp, name, 2, args.len()));
                return None;
            }
            let col = column_name(&args[0], model, table, diags)?;
            Some(QueryStep::Where { column: col, op: CompareOp::Eq, arg: 1, kind })
        }
        WhereKind::In => {
            if args.len() != 2 {
                diags.push(arity_err(sp, name, 2, args.len()));
                return None;
            }
            let col = column_name(&args[0], model, table, diags)?;
            if !matches!(args[1], Expr::List(..)) {
                diags.push(err(
                    args[1].span(),
                    cat::ORM_BAD_QUERY,
                    "`where_in` needs a list of values.",
                    "Write `where_in(id, [1, 2, 3])`.",
                ));
                return None;
            }
            Some(QueryStep::Where { column: col, op: CompareOp::Eq, arg: 1, kind })
        }
    }
}

fn compare_from(e: &Expr) -> Option<CompareOp> {
    let s = match e {
        Expr::Str(s, _) => s.as_str(),
        Expr::Ident(s, _) => s.as_str(),
        _ => return None,
    };
    Some(match s {
        "=" | "==" => CompareOp::Eq,
        "!=" | "<>" => CompareOp::Ne,
        "<" => CompareOp::Lt,
        "<=" => CompareOp::Le,
        ">" => CompareOp::Gt,
        ">=" => CompareOp::Ge,
        _ => return None,
    })
}

/// Read a column reference, which must be a literal field name. A variable
/// would make the query stringly-typed, and the point of this builder is that
/// it is not.
fn column_name(e: &Expr, model: &str, table: &TableDef, diags: &mut Vec<Diag>) -> Option<String> {
    let col = match e {
        Expr::Ident(c, _) => c.clone(),
        Expr::Str(c, _) => c.clone(),
        Expr::Member(_, c, _) => c.clone(),
        other => {
            diags.push(err(
                other.span(),
                cat::ORM_BAD_QUERY,
                format!("`{model}` query needs a field name."),
                "Name the field directly, e.g. `where(email, value)`.",
            ));
            return None;
        }
    };
    if table.column(&col).is_none() {
        diags.push(unknown_col_err(e.span(), model, &col, table, cat::ORM_UNKNOWN_COLUMN));
        return None;
    }
    Some(col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_models(src: &str) -> Vec<ModelDef> {
        crate::frontend(src, "test.hard").expect("parse").model_defs()
    }

    /// A program as the merge and cache paths build it: models in `stmts`,
    /// with the lifted field left empty.
    fn as_merged(src: &str) -> crate::ast::Program {
        let mut prog = crate::frontend(src, "test.hard").expect("parse");
        prog.models = Vec::new();
        prog
    }

    // ---------------- writes (M5.3.3) ----------------

    const WRITE_SRC: &str = r#"
        model User {
            id : Int @primary @auto_increment
            email : Email
            name : String
            age : Int @default(18)
            updated : Time @updated_at
        }
    "#;

    /// The plan for a model-rooted write.
    fn w(src: &str) -> Result<WritePlan, Vec<u16>> {
        let schema = build(&parse_models(WRITE_SRC), BuildOpts::strict()).expect("schema");
        let prog = crate::frontend(src, "w.hard").expect("parse");
        let expr = first_call(&prog).expect("a call");
        let (model, chain, _) = orm_chain(&expr).expect("a model-rooted chain");
        let (method, args, _) = chain[0].clone();
        match parse_write(&model, &method, &args, false, &schema) {
            Ok(p) => Ok(p),
            Err(d) => Err(d.iter().map(|x| x.code).collect()),
        }
    }

    /// The plan for a record's own write, given the model it holds.
    fn wr(method: &str, args: &str) -> Result<WritePlan, Vec<u16>> {
        let schema = build(&parse_models(WRITE_SRC), BuildOpts::strict()).expect("schema");
        let src = format!("GET \"/\" :: {{ <- u.{method}({args}) }}");
        let prog = crate::frontend(&src, "w.hard").expect("parse");
        let expr = first_call(&prog).expect("a call");
        let (name, args, _) = record_write(&expr).expect("a record write");
        match parse_write("User", &name, &args, true, &schema) {
            Ok(p) => Ok(p),
            Err(d) => Err(d.iter().map(|x| x.code).collect()),
        }
    }

    fn created(src: &str) -> Vec<String> {
        let p = w(src).expect("plan");
        match p.op {
            WriteOp::Create(f) | WriteOp::Upsert(f) | WriteOp::FindOrCreate(f) => {
                write_field_names(&f).iter().map(|s| s.to_string()).collect()
            }
            other => panic!("expected fields, got {other:?}"),
        }
    }

    #[test]
    fn create_takes_the_columns_it_names() {
        assert_eq!(
            created("GET \"/\" :: { <- User.create({email: \"a@b.c\", name: \"Ada\"}) }"),
            vec!["email".to_string(), "name".to_string()],
            "fields keep the order they were written in"
        );
    }

    #[test]
    fn a_create_must_give_every_column_the_database_will_not() {
        let c = w("GET \"/\" :: { <- User.create({email: \"a@b.c\"}) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_MISSING_FIELD], "name has no default");
        // `age` has a default and `id` is generated, so neither is required.
        assert!(w("GET \"/\" :: { <- User.create({email: \"a@b.c\", name: \"Ada\"}) }").is_ok());
    }

    #[test]
    fn a_nullable_field_is_not_required() {
        let src = "model User { id : Int @primary @auto_increment, nick : String @nullable }";
        let schema = build(&parse_models(src), BuildOpts::strict()).expect("schema");
        let t = schema.table("User").unwrap();
        assert!(!t.column("nick").unwrap().required_clause(), "nullable needs no value");
        assert!(!t.column("id").unwrap().required_clause(), "and a generated key does not either");
    }

    #[test]
    fn an_unknown_field_is_reported_with_a_suggestion() {
        // The typo leaves the real field unset, so the write is also
        // incomplete: both are reported, because both have to be fixed.
        let c = w("GET \"/\" :: { <- User.create({emial: \"a@b.c\", name: \"Ada\"}) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_UNKNOWN_COLUMN, cat::ORM_MISSING_FIELD]);
    }

    #[test]
    fn a_write_cannot_set_the_generated_key() {
        let c = w("GET \"/\" :: { <- User.create({id: 4, email: \"a@b.c\", name: \"Ada\"}) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_BAD_DEFAULT], "an insert may not name its own key");
    }

    #[test]
    fn a_field_given_twice_is_reported() {
        let c = w("GET \"/\" :: { <- User.create({email: \"a@b.c\", name: \"Ada\", age: 1, age: 2}) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_DUPLICATE_COLUMN]);
    }

    #[test]
    fn a_write_needs_an_object() {
        let c = w("GET \"/\" :: { <- User.create(1) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_BAD_QUERY]);
    }

    #[test]
    fn write_arity_is_checked() {
        assert_eq!(w("GET \"/\" :: { <- User.create() }").unwrap_err(), vec![cat::ORM_BAD_QUERY]);
        assert_eq!(w("GET \"/\" :: { <- User.delete() }").unwrap_err(), vec![cat::ORM_BAD_QUERY]);
        assert_eq!(w("GET \"/\" :: { <- User.delete(1, 2) }").unwrap_err(), vec![cat::ORM_BAD_QUERY]);
    }

    #[test]
    fn delete_takes_a_key_and_nothing_else() {
        assert!(matches!(w("GET \"/\" :: { <- User.delete(3) }").unwrap().op, WriteOp::DeleteKey));
    }

    #[test]
    fn an_upsert_names_the_row_it_changes() {
        // The spec's own example: an upsert identifies its row by key and
        // updates only the fields it was given.
        assert_eq!(
            created("GET \"/\" :: { <- User.upsert({id: 1, name: \"Ada\"}) }"),
            vec!["id".to_string(), "name".to_string()],
            "the key is written on an upsert"
        );
        assert!(w("GET \"/\" :: { <- User.upsert({name: \"Ada\"}) }").is_ok(), "and may be left out");
    }

    #[test]
    fn find_or_create_needs_a_complete_enough_object() {
        // The literal is both the lookup and the insert, so it has to satisfy
        // the insert when nothing matches.
        assert_eq!(
            created("GET \"/\" :: { <- User.find_or_create_by({email: \"a@b.c\", name: \"Ada\"}) }"),
            vec!["email".to_string(), "name".to_string()]
        );
        let c = w("GET \"/\" :: { <- User.find_or_create_by({email: \"a@b.c\"}) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_MISSING_FIELD], "the create half would fail without name");
    }

    #[test]
    fn a_record_writes_without_a_schema_round_trip() {
        assert!(matches!(wr("save", "").unwrap().op, WriteOp::Save));
        assert!(matches!(wr("touch", "").unwrap().op, WriteOp::Touch));
        assert!(matches!(wr("destroy", "").unwrap().op, WriteOp::Destroy));
    }

    #[test]
    fn a_record_write_takes_no_arguments() {
        assert_eq!(wr("save", "1").unwrap_err(), vec![cat::ORM_BAD_QUERY]);
        assert_eq!(wr("touch", "1").unwrap_err(), vec![cat::ORM_BAD_QUERY]);
    }

    #[test]
    fn a_write_on_an_unknown_model_is_reported() {
        let schema = build(&parse_models(WRITE_SRC), BuildOpts::strict()).unwrap();
        let c = match parse_write("Nope", "create", &[Expr::Obj(vec![], crate::token::Span::default())], false, &schema) {
            Err(d) => d.iter().map(|x| x.code).collect::<Vec<_>>(),
            Ok(_) => panic!("expected an error"),
        };
        assert_eq!(c, vec![cat::ORM_UNKNOWN_MODEL]);
    }

    #[test]
    fn only_a_bare_name_can_be_a_record() {
        // `req.body.save()` is not a record write: the receiver is a
        // computation rather than a local, so nothing is known about what it
        // holds. The same shape with a bare name is a record write.
        let prog = crate::frontend("GET \"/\" :: { <- req.body.save() }", "x.hard").unwrap();
        let crate::ast::Stmt::Route(r) = &prog.stmts[prog.stmts.len() - 1] else { panic!("a route") };
        let crate::ast::Stmt::Return(e, _) = &r.body[0] else { panic!("a return") };
        assert!(record_write(e).is_none());
        let prog = crate::frontend("GET \"/\" :: { <- u.save() }", "x.hard").unwrap();
        let crate::ast::Stmt::Route(r) = &prog.stmts[prog.stmts.len() - 1] else { panic!("a route") };
        let crate::ast::Stmt::Return(e, _) = &r.body[0] else { panic!("a return") };
        assert!(record_write(e).is_some());
    }

    /// The codes a whole program reports, which is the only way to see a
    /// diagnostic that codegen raises.
    fn compiled(src: &str) -> Vec<u16> {
        match crate::compile_to_cpp(src, "w.hard") {
            Ok(_) => Vec::new(),
            Err(d) => d.iter().map(|x| x.code).collect(),
        }
    }

    #[test]
    fn writing_a_value_that_is_not_a_record_is_reported() {
        // `count()` yields a number, so there is nothing to save. Falling
        // through to a field read would say the field was missing instead,
        // which names the wrong problem.
        let src = format!("{WRITE_SRC}\nGET \"/\" :: {{ n <- User.count()\n <- n.save() }}");
        assert_eq!(compiled(&src), vec![cat::ORM_NOT_A_RECORD]);

        let src = format!("{WRITE_SRC}\nGET \"/\" :: {{ m <- {{a: 1}}\n <- m.destroy() }}");
        assert_eq!(compiled(&src), vec![cat::ORM_NOT_A_RECORD], "a map is not a row either");
    }

    #[test]
    fn a_record_from_a_query_can_be_written_back() {
        // The other half of the same rule: the value has to carry a model for
        // the write to have anything to validate against.
        let src = format!("{WRITE_SRC}\nGET \"/\" :: {{ u <- User.find(1)\n <- u.save()\n <- u.touch() }}");
        assert_eq!(compiled(&src), Vec::<u16>::new(), "a record from find() is a record");
    }

    #[test]
    fn writes_and_queries_are_told_apart() {
        for m in ["create", "upsert", "delete", "find_or_create_by", "save", "touch", "destroy"] {
            assert!(is_orm_write_method(m), "{m} is a write");
            assert!(!is_orm_method(m), "{m} is not a query method");
        }
        for m in ["all", "where", "find", "count"] {
            assert!(!is_orm_write_method(m), "{m} is not a write");
        }
        // The record methods are the ones a value can call, not a model.
        assert!(is_orm_write_method_on_record("save"));
        assert!(!is_orm_write_method_on_record("create"));
    }

    #[test]
    fn the_column_list_in_a_suggestion_is_the_models_own() {
        let s = build(&parse_models(WRITE_SRC), BuildOpts::strict()).unwrap();
        let t = s.table("User").unwrap();
        let msg = suggest_field(t, "zzzzz");
        assert!(msg.contains("id, email, name, age, updated"), "{msg}");
    }

    #[test]
    fn a_fingerprint_ignores_the_order_fields_were_written_in() {
        // A fingerprint drives the "is the database behind the models?"
        // check, so reordering fields must not read as a pending change.
        let a = schema_of(
            "model User { id : Int @primary, name : String, city : String @index, age : Int @default(1) }",
        )
        .expect("schema");
        let b = schema_of(
            "model User { age : Int @default(1), city : String @index, id : Int @primary, name : String }",
        )
        .expect("schema");
        assert_eq!(a.fingerprint(Dialect::Sqlite), b.fingerprint(Dialect::Sqlite));
        assert_eq!(a.fingerprint(Dialect::Postgres), b.fingerprint(Dialect::Postgres));
    }

    #[test]
    fn a_fingerprint_still_notices_a_real_change() {
        let a = schema_of("model User { id : Int @primary, name : String }").expect("schema");
        let b = schema_of("model User { id : Int @primary, name : String @unique }").expect("schema");
        assert_ne!(a.fingerprint(Dialect::Sqlite), b.fingerprint(Dialect::Sqlite));
    }

    #[test]
    fn models_survive_a_program_rebuilt_from_statements() {
        // The merge path and the AST cache both hand the schema engine a
        // program whose lifted model list is empty, so `model_defs` has to
        // recover the declarations from the statements.
        let prog = as_merged("model Post { id : Int @primary @auto_increment\n            title : String }");
        let defs = prog.model_defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "Post");
        let schema = build(&defs, BuildOpts::strict()).expect("schema");
        assert!(schema.table("post").is_some());
    }

    #[test]
    fn models_survive_an_ast_cache_round_trip() {
        let prog = as_merged("model Post { id : Int @primary @auto_increment\n            title : String }");
        let bytes = crate::astser::serialize_stmts(&prog.stmts).expect("serialize");
        let stmts = crate::astser::deserialize_stmts(&bytes).expect("deserialize");
        let cached = crate::ast::Program { stmts, path: String::new(), models: Vec::new() };
        let schema = build(&cached.model_defs(), BuildOpts::strict()).expect("schema");
        assert_eq!(schema.tables.len(), 1);
        assert!(schema.table("post").is_some());
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
        // Exact text, not a prefix: a rewrite of the create statement used to
        // leave the `ON table (col)` tail on a `DROP INDEX`, which no database
        // will run.
        assert_eq!(drops[0], "DROP INDEX IF EXISTS \"idx_user_city\"");
        assert_eq!(drops[1], "DROP TABLE IF EXISTS \"user\"");
    }

    #[test]
    fn a_unique_index_is_dropped_without_its_unique_keyword() {
        let s = schema_of("model User { id : Int @primary, city : String @unique @index }").expect("schema");
        let t = s.table("User").unwrap();
        assert_eq!(
            t.create_index_sql(Dialect::Sqlite),
            vec!["CREATE UNIQUE INDEX \"idx_user_city\" ON \"user\" (\"city\")"]
        );
        assert_eq!(t.drop_sql(Dialect::Sqlite)[0], "DROP INDEX IF EXISTS \"idx_user_city\"");
    }

    #[test]
    fn a_unique_constraint_needs_no_index_statement() {
        // A plain `@unique` is a column constraint, so there is no index
        // statement to create and none to drop.
        let s = schema_of("model User { id : Int @primary, email : Email @unique }").expect("schema");
        let t = s.table("User").unwrap();
        assert!(t.create_index_sql(Dialect::Postgres).is_empty());
        assert_eq!(t.drop_sql(Dialect::Postgres), vec!["DROP TABLE IF EXISTS \"user\""]);
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

    // ---------------- query builder (M5.3.2) ----------------

    fn q(src: &str) -> Result<QueryPlan, Vec<u16>> {
        let models = parse_models(SCHEMA_SRC);
        let schema = build(&models, BuildOpts::strict()).expect("schema");
        let prog = crate::frontend(src, "q.hard").expect("parse");
        let expr = first_call(&prog).expect("a call in the source");
        match parse_query(&expr, &schema) {
            Ok(p) => Ok(p),
            Err(d) => Err(d.iter().map(|x| x.code).collect()),
        }
    }

    const SCHEMA_SRC: &str = r#"
        model User {
            id : Int @primary @auto_increment
            email : Email @unique
            name : String
            age : Int @default(18)
            created : Time @default(now())
        }
    "#;

    /// The first call expression in a program, which for these tests is the
    /// query itself.
    fn first_call(prog: &crate::ast::Program) -> Option<Expr> {
        fn in_expr(e: &Expr) -> Option<Expr> {
            if matches!(e, Expr::Call { .. }) && orm_chain(e).is_some() {
                return Some(e.clone());
            }
            match e {
                Expr::Call { callee, args, .. } => in_expr(callee).or_else(|| args.iter().find_map(in_expr)),
                Expr::Member(b, _, _) | Expr::Index(b, _, _) => in_expr(b),
                Expr::Binary(_, l, r, _) => in_expr(l).or_else(|| in_expr(r)),
                Expr::Unary(_, x, _) => in_expr(x),
                _ => None,
            }
        }
        fn in_stmts(sts: &[crate::ast::Stmt]) -> Option<Expr> {
            for s in sts {
                match s {
                    crate::ast::Stmt::Return(e, _) => {
                        if let Some(f) = in_expr(e) {
                            return Some(f);
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        for st in &prog.stmts {
            if let crate::ast::Stmt::Route(r) = st {
                if let Some(f) = in_stmts(&r.body) {
                    return Some(f);
                }
            }
        }
        None
    }

    #[test]
    fn all_is_the_default_terminal() {
        let p = q("GET \"/\" :: { <- User.all() }").expect("plan");
        assert_eq!(p.model, "User");
        assert_eq!(p.terminal, Terminal::All);
        assert!(p.steps.is_empty());
    }

    #[test]
    fn count_and_exists_are_terminals() {
        assert_eq!(q("GET \"/\" :: { <- User.count() }").unwrap().terminal, Terminal::Count);
        assert_eq!(q("GET \"/\" :: { <- User.exists() }").unwrap().terminal, Terminal::Exists);
        // They chain with the builder methods too.
        let p = q("GET \"/\" :: { <- User.where(age > 20).count() }").unwrap();
        assert_eq!(p.terminal, Terminal::Count);
        assert_eq!(p.steps.len(), 1);
    }

    #[test]
    fn first_is_a_terminal() {
        let p = q("GET \"/\" :: { <- User.first() }").unwrap();
        assert_eq!(p.terminal, Terminal::First);
    }

    #[test]
    fn find_becomes_a_primary_key_comparison() {
        let p = q("GET \"/\" :: { <- User.find(7) }").expect("plan");
        assert_eq!(
            p.steps,
            vec![QueryStep::Where {
                column: "id".to_string(),
                op: CompareOp::Eq,
                arg: 0,
                kind: WhereKind::Compare
            }]
        );
        // `find` is a single-row lookup.
        assert_eq!(p.terminal, Terminal::First);
    }

    #[test]
    fn where_accepts_a_comparison_expression() {
        let p = q("GET \"/\" :: { <- User.where(age > 20) }").expect("plan");
        assert_eq!(
            p.steps,
            vec![QueryStep::Where {
                column: "age".to_string(),
                op: CompareOp::Gt,
                arg: 0,
                kind: WhereKind::Compare
            }]
        );
    }

    #[test]
    fn where_accepts_every_comparison_operator() {
        for (src, want) in [
            ("age == 1", CompareOp::Eq),
            ("age != 1", CompareOp::Ne),
            ("age < 1", CompareOp::Lt),
            ("age <= 1", CompareOp::Le),
            ("age > 1", CompareOp::Gt),
            ("age >= 1", CompareOp::Ge),
        ] {
            let p = q(&format!("GET \"/\" :: {{ <- User.where({src}) }}")).expect(src);
            match &p.steps[0] {
                QueryStep::Where { op, column, .. } => {
                    assert_eq!(*op, want, "{src}");
                    assert_eq!(column, "age", "{src}");
                }
                other => panic!("{src}: {other:?}"),
            }
        }
    }

    #[test]
    fn where_takes_a_column_and_a_value_pair() {
        let p = q("GET \"/\" :: { <- User.where(email, \"a@b.c\") }").expect("plan");
        assert_eq!(
            p.steps,
            vec![QueryStep::Where {
                column: "email".to_string(),
                op: CompareOp::Eq,
                arg: 1,
                kind: WhereKind::Compare
            }]
        );
    }

    #[test]
    fn where_can_take_an_explicit_operator_string() {
        let p = q("GET \"/\" :: { <- User.where(age, 20, \">=\") }").expect("plan");
        match &p.steps[0] {
            QueryStep::Where { op, .. } => assert_eq!(*op, CompareOp::Ge),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn like_and_in_need_their_own_methods() {
        let p = q("GET \"/\" :: { <- User.where_like(name, \"%a%\") }").expect("plan");
        assert_eq!(p.steps[0], QueryStep::Where { column: "name".into(), op: CompareOp::Eq, arg: 1, kind: WhereKind::Like });
        let p = q("GET \"/\" :: { <- User.where_in(id, [1, 2, 3]) }").expect("plan");
        assert_eq!(p.steps[0], QueryStep::Where { column: "id".into(), op: CompareOp::Eq, arg: 1, kind: WhereKind::In });
    }

    #[test]
    fn a_full_chain_keeps_source_order() {
        let p = q(
            "GET \"/\" :: { <- User.where(age > 20).where_like(name, \"%a%\").order_by(created, desc).limit(10).offset(20).all() }",
        )
        .expect("plan");
        assert_eq!(p.terminal, Terminal::All);
        assert_eq!(
            p.steps,
            vec![
                QueryStep::Where { column: "age".into(), op: CompareOp::Gt, arg: 0, kind: WhereKind::Compare },
                QueryStep::Where { column: "name".into(), op: CompareOp::Eq, arg: 1, kind: WhereKind::Like },
                QueryStep::OrderBy { column: "created".into(), desc: true },
                QueryStep::Limit(10),
                QueryStep::Offset(20),
            ]
        );
    }

    #[test]
    fn order_by_takes_a_direction() {
        let p = q("GET \"/\" :: { <- User.order_by(created, desc) }").expect("plan");
        assert_eq!(p.steps[0], QueryStep::OrderBy { column: "created".into(), desc: true });
        let p = q("GET \"/\" :: { <- User.order_by(created, asc) }").expect("plan");
        assert_eq!(p.steps[0], QueryStep::OrderBy { column: "created".into(), desc: false });
    }

    #[test]
    fn limit_and_offset_take_literal_counts() {
        let p = q("GET \"/\" :: { <- User.limit(10).offset(20) }").expect("plan");
        assert_eq!(p.steps, vec![QueryStep::Limit(10), QueryStep::Offset(20)]);
        // Zero is a legal count, unlike a negative one.
        assert!(q("GET \"/\" :: { <- User.limit(0) }").is_ok());
    }

    #[test]
    fn a_non_literal_limit_is_rejected() {
        // A runtime count would compile, but it cannot be checked here, and a
        // silently unchecked limit is how `LIMIT -1` reaches a database.
        let c = q("n ::= 10\nGET \"/\" :: { <- User.limit(n) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_BAD_QUERY]);
    }

    #[test]
    fn a_negative_limit_is_rejected() {
        let c = q("GET \"/\" :: { <- User.limit(-1) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_BAD_QUERY]);
    }

    #[test]
    fn an_unknown_column_is_a_compile_error() {
        let c = q("GET \"/\" :: { <- User.where(emial == \"x\") }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_UNKNOWN_COLUMN]);
        let c = q("GET \"/\" :: { <- User.order_by(nope, asc) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_BAD_ORDER]);
    }

    #[test]
    fn a_relationship_field_is_not_a_column() {
        // `posts` would be a relation on a model that has one; here `User` has
        // no such field, so the query must not compile.
        let c = q("GET \"/\" :: { <- User.where(posts, 1) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_UNKNOWN_COLUMN]);
    }

    #[test]
    fn an_unknown_model_is_reported() {
        let c = q("GET \"/\" :: { <- Nope.all() }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_UNKNOWN_MODEL]);
    }

    #[test]
    fn find_cannot_be_turned_back_into_a_collection() {
        // `find` has already committed to one row, so a later terminal is
        // meaningless rather than a way to fetch a set.
        let c = q("GET \"/\" :: { <- User.find(1).all() }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_BAD_QUERY]);
        let p = q("GET \"/\" :: { <- User.find(1).first() }").expect("plan");
        assert_eq!(p.terminal, Terminal::First);
    }

    #[test]
    fn a_terminal_must_come_last() {
        let c = q("GET \"/\" :: { <- User.all().limit(10) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_BAD_QUERY]);
        let c = q("GET \"/\" :: { <- User.count().order_by(created, asc) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_BAD_QUERY]);
    }

    #[test]
    fn an_unknown_method_is_reported() {
        let c = q("GET \"/\" :: { <- User.fetch_all() }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_BAD_QUERY]);
    }

    #[test]
    fn bad_arity_is_reported_per_method() {
        assert_eq!(q("GET \"/\" :: { <- User.where() }").unwrap_err(), vec![cat::ORM_BAD_QUERY]);
        assert_eq!(q("GET \"/\" :: { <- User.find(1, 2) }").unwrap_err(), vec![cat::ORM_BAD_QUERY]);
        assert_eq!(q("GET \"/\" :: { <- User.order_by(created) }").unwrap_err(), vec![cat::ORM_BAD_QUERY]);
        assert_eq!(q("GET \"/\" :: { <- User.where_like(name) }").unwrap_err(), vec![cat::ORM_BAD_QUERY]);
    }

    #[test]
    fn a_bad_order_direction_is_reported() {
        let c = q("GET \"/\" :: { <- User.order_by(created, sideways) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_BAD_ORDER]);
    }

    #[test]
    fn a_dynamic_column_name_is_refused() {
        // Letting a runtime name through would make the builder stringly-typed,
        // which is the thing it exists to avoid.
        let c = q("col ::= \"age\"\nGET \"/\" :: { <- User.where(col, 20) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_UNKNOWN_COLUMN]);
    }

    #[test]
    fn where_in_needs_a_list() {
        let c = q("GET \"/\" :: { <- User.where_in(id, 5) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_BAD_QUERY]);
    }

    #[test]
    fn a_non_comparison_operator_is_not_a_comparison() {
        let c = q("GET \"/\" :: { <- User.where(age + 1) }").unwrap_err();
        assert_eq!(c, vec![cat::ORM_BAD_QUERY]);
    }

    #[test]
    fn projected_columns_are_the_referenced_ones() {
        let models = parse_models(SCHEMA_SRC);
        let schema = build(&models, BuildOpts::strict()).unwrap();
        let prog = crate::frontend("GET \"/\" :: { <- User.where(age > 1).order_by(name, asc) }", "p.hard").unwrap();
        let expr = first_call(&prog).unwrap();
        let p = parse_query(&expr, &schema).unwrap();
        assert_eq!(p.projected_columns(&schema), Some(vec!["age".to_string(), "name".to_string()]));
        // No reference means every column.
        let prog = crate::frontend("GET \"/\" :: { <- User.all() }", "p.hard").unwrap();
        let p = parse_query(&first_call(&prog).unwrap(), &schema).unwrap();
        assert_eq!(p.projected_columns(&schema), None);
    }

    #[test]
    fn is_orm_method_covers_every_builder_method() {
        for m in ["all", "first", "count", "exists", "where", "where_like", "where_in", "limit",
                  "offset", "order_by", "find"] {
            assert!(is_orm_method(m), "{m}");
        }
        for m in ["fetch", "save", "delete", "nope"] {
            assert!(!is_orm_method(m), "{m}");
        }
    }

    #[test]
    fn a_module_call_has_a_chain_shape_but_is_not_a_query() {
        // `json.parse(..)` and `User.all()` are structurally identical, which
        // is why callers must gate on the root being a declared model rather
        // than on the shape alone.
        let prog = crate::frontend("GET \"/\" :: { <- json.parse(\"{}\") }", "x.hard").unwrap();
        let expr = first_call(&prog).expect("a call");
        assert_eq!(chain_root(&expr).as_deref(), Some("json"));
        let models = parse_models(SCHEMA_SRC);
        let schema = build(&models, BuildOpts::strict()).unwrap();
        let codes: Vec<u16> = parse_query(&expr, &schema).err().unwrap().iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![cat::ORM_UNKNOWN_MODEL]);

        // A non-call expression is not a chain at all.
        let prog = crate::frontend("GET \"/\" :: { <- 1 + 2 }", "x.hard").unwrap();
        if let crate::ast::Stmt::Route(r) = &prog.stmts[0] {
            if let crate::ast::Stmt::Return(e, _) = &r.body[0] {
                assert!(chain_root(e).is_none());
            }
        }
        // Calling a local function is not a query either.
        let prog = crate::frontend(
            "calc one() => Int { return 1 }\nGET \"/\" :: { <- one() }",
            "x.hard",
        )
        .unwrap();
        let mut found = false;
        for st in &prog.stmts {
            if let crate::ast::Stmt::Route(r) = st {
                if let crate::ast::Stmt::Return(Expr::Call { .. }, _) = &r.body[0] {
                    found = true;
                }
            }
        }
        assert!(found, "expected a plain call in the route");
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
