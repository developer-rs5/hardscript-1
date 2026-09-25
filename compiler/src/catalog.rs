//! The permanent HardScript diagnostic catalog (Diagnostics V2).
//!
//! Every [`Diag`] references exactly one code from this catalog. Codes are
//! grouped by reserved ranges:
//!
//! * `HS0001..=HS0099` — lexer & parser (syntax)
//! * `HS0100..=HS0199` — name resolution (undefined, duplicates, shadowing)
//! * `HS0200..=HS0299` — type checking
//! * `HS0300..=HS0399` — imports and the module graph
//! * `HS0400..=HS0499` — optimizer
//! * `HS0500..=HS0599` — internal / compiler & native toolchain errors
//! * `HS0600..=HS0699` — formatter & CLI usage
//! * `HS2000..=HS2099` — warnings
//!
//! The catalog is the single source of truth for `docs/errors.md`,
//! `hard explain HSxxxx`, the CLI `hard errors` listing, and the error
//! catalog report. Codes are unique; a unit test enforces it.

use crate::error::ErrorKind;

/// Format a numeric code as its public `HSxxxx` spelling.
pub fn format(number: u16) -> String {
    format!("HS{number:04}")
}

// Named constants for every catalog code, referenced from `Diag::with_code`
// call sites. Names read like the entry they point at.
pub const UNEXPECTED_TOKEN: u16 = 1;
pub const EXPECTED_TOKEN: u16 = 2;
pub const UNTERMINATED_DELIMITER: u16 = 3;
pub const MALFORMED_LITERAL: u16 = 4;
pub const UNEXPECTED_EOI: u16 = 5;
pub const INVALID_IDENTIFIER: u16 = 6;
pub const NESTING_TOO_DEEP: u16 = 7;
pub const EXPRESSION_TOO_LONG: u16 = 8;
pub const UNDEFINED_FUNCTION: u16 = 101;
pub const UNDEFINED_MODEL: u16 = 102;
pub const UNKNOWN_MODULE: u16 = 103;
pub const UNDEFINED_VARIABLE: u16 = 104;
pub const DUPLICATE_DECL: u16 = 105;
pub const DUPLICATE_ROUTE: u16 = 106;
pub const DUPLICATE_MIDDLEWARE: u16 = 107;
pub const DUPLICATE_MODEL: u16 = 108;
pub const UNDEFINED_MEMBER: u16 = 110;
pub const TYPE_MISMATCH: u16 = 201;
pub const WRONG_ARG_COUNT: u16 = 202;
pub const OPERATOR_TYPE_ERROR: u16 = 203;
pub const INVALID_FIELD_OR_INDEX: u16 = 204;
pub const RETURN_TYPE_MISMATCH: u16 = 205;
pub const CONDITION_NOT_BOOL: u16 = 206;
pub const IMPORT_CYCLE: u16 = 301;
pub const MODULE_NOT_FOUND: u16 = 302;
pub const IMPORT_CHAIN_TOO_DEEP: u16 = 303;
pub const INVALID_IMPORT_TARGET: u16 = 304;
pub const OPTIMIZER_INTERNAL: u16 = 401;
pub const OPTIMIZER_UNSOUND: u16 = 402;
pub const INTERNAL_COMPILER: u16 = 501;
pub const NATIVE_COMPILE_FAILED: u16 = 502;
pub const IO_ERROR: u16 = 503;
pub const FORMAT_ERROR: u16 = 601;
pub const CLI_USAGE_ERROR: u16 = 602;
pub const W_UNUSED_VARIABLE: u16 = 2001;
pub const W_UNUSED_FUNCTION: u16 = 2002;
pub const W_UNUSED_IMPORT: u16 = 2003;
pub const W_SHADOWED_VARIABLE: u16 = 2004;
pub const W_DEAD_CODE: u16 = 2005;
pub const W_CONSTANT_CONDITION: u16 = 2006;
pub const W_REDUNDANT_RETURN: u16 = 2007;
pub const W_EMPTY_BLOCK: u16 = 2008;
pub const W_UNREACHABLE_STATEMENT: u16 = 2009;
pub const W_DUPLICATE_MIDDLEWARE: u16 = 2010;
pub const W_DUPLICATE_ROUTE: u16 = 2011;
pub const W_DUPLICATE_MODEL: u16 = 2012;
pub const W_CONSTANT_EXPRESSION: u16 = 2013;
pub const W_ALWAYS_TRUE_COMPARISON: u16 = 2014;
pub const W_ALWAYS_FALSE_COMPARISON: u16 = 2015;

/// A single catalog entry: everything the tooling knows about one code.
#[derive(Debug, Clone, Copy)]
pub struct CodeDef {
    /// Numeric part of the code (`104` -> `HS0104`).
    pub number: u16,
    /// Short human title, e.g. "Undefined Variable".
    pub name: &'static str,
    /// Family classification used for grouping.
    pub kind: ErrorKind,
    /// One sentence on what this diagnostic means.
    pub meaning: &'static str,
    /// A minimal illustrative fragment of source that triggers it.
    pub example: &'static str,
    /// Common causes (used by `hard explain` and `docs/errors.md`).
    pub causes: &'static [&'static str],
    /// Concrete fixes shown by `hard explain` and `docs/errors.md`.
    pub fixes: &'static [&'static str],
}

/// The full, ordered diagnostic catalog. Order is sorted by number; the
/// unit test `catalog_is_sorted_and_unique` enforces both properties.
pub fn catalog() -> &'static [CodeDef] {
    &[
        // ---------------- Syntax (HS0001..=HS0099) ----------------
        CodeDef {
            number: 1,
            name: "Unexpected Token",
            kind: ErrorKind::Parse,
            meaning: "The parser found a token it did not expect in this position.",
            example: "GET \"/\" :: { <- { a: 1, } }",
            causes: &[
                "A stray closing bracket, brace or semicolon.",
                "A keyword used where an expression was expected.",
                "A leftover token from an earlier edit.",
            ],
            fixes: &[
                "Remove the offending token.",
                "Check that every `{` has one matching `}`.",
                "Re-read the line the caret points at.",
            ],
        },
        CodeDef {
            number: 2,
            name: "Expected Token",
            kind: ErrorKind::Parse,
            meaning: "The parser reached a point where a specific token had to appear, but found something else instead.",
            example: "app 3000   (missing the @)",
            causes: &[
                "A missing delimiter such as `::`, `{`, `(` or `,`.",
                "A keyword spelled with an extra or missing letter.",
            ],
            fixes: &[
                "Add the token named in the message, e.g. `app @3000`.",
                "Use the exact spelling from the language reference.",
            ],
        },
        CodeDef {
            number: 3,
            name: "Unterminated Delimiter",
            kind: ErrorKind::Lex,
            meaning: "A string, bracket, brace or parenthesis was opened but never closed before the end of the input.",
            example: "GET \"/users :: { ... }",
            causes: &[
                "A closing `\"`, `]`, `}` or `)` was deleted.",
                "A multi-line string was not joined by the runtime string syntax.",
            ],
            fixes: &[
                "Add the matching closer.",
                "Check string escapes; an unescaped newline ends the literal.",
            ],
        },
        CodeDef {
            number: 4,
            name: "Malformed Literal",
            kind: ErrorKind::Lex,
            meaning: "A number, string or other literal is not a valid HardScript value.",
            example: "x ::= 1_000_000_  (trailing underscore)",
            causes: &[
                "A number with a trailing or doubled underscore.",
                "A string escape that is not part of the language.",
                "A float written without digits after the dot.",
            ],
            fixes: &[
                "Rewrite the literal so it parses as a plain value.",
                "Consult the reference for the allowed escapes.",
            ],
        },
        CodeDef {
            number: 5,
            name: "Unexpected End of Input",
            kind: ErrorKind::Parse,
            meaning: "The source ended in the middle of a construct that still expected tokens.",
            example: "model User = users [ id => Int  (EOF)",
            causes: &[
                "The file is truncated or was saved mid-edit.",
                "A delimiter sequence was deleted.",
            ],
            fixes: &[
                "Complete the construct before the end of the file.",
                "The caret points just after the last readable token.",
            ],
        },
        CodeDef {
            number: 6,
            name: "Invalid Identifier",
            kind: ErrorKind::Lex,
            meaning: "A name does not follow the identifier rules of the language.",
            example: "3users ::= 1",
            causes: &["A name starting with a digit", "A hyphens or spaces inside a name"],
            fixes: &[
                "Use letters, digits and underscores starting with a letter.",
                "Rename to a valid identifier.",
            ],
        },
        CodeDef {
            number: 7,
            name: "Nesting Too Deep",
            kind: ErrorKind::Parse,
            meaning: "A construct nests deeper than the parser supports (limit 256).",
            example: "((((... 256 parens ...))))",
            causes: &["Generated or hand-written deeply nested expressions."],
            fixes: &[
                "Factor the expression into intermediate variables.",
                "Reduce the nesting below the limit.",
            ],
        },
        CodeDef {
            number: 8,
            name: "Expression Too Long",
            kind: ErrorKind::Parse,
            meaning: "A chained expression has more operands than the parser supports (limit 256).",
            example: "a + b + c + ...  (over 256 operands)",
            causes: &["A long inline sum or concatenation chain."],
            fixes: &["Split the chain into multiple statements."],
        },

        // ---------------- Name resolution (HS0100..=HS0199) ----------------
        CodeDef {
            number: 101,
            name: "Undefined Function",
            kind: ErrorKind::Type,
            meaning: "A call names a `calc` that does not exist anywhere in the project.",
            example: "GET \"/\" :: { <- total() }  (calc total never declared)",
            causes: &["The function was never declared.", "The function lives in another module that is not brought in."],
            fixes: &[
                "Define `calc {name}(..) => .. { .. }` before calling it.",
                "Add the missing `bring \"./module\"`.",
            ],
        },
        CodeDef {
            number: 102,
            name: "Undefined Model",
            kind: ErrorKind::Type,
            meaning: "A reference names a model that was never declared.",
            example: "GET \"/\" :: { <- User.find(1) }  (no model User)",
            causes: &["The model was not declared.", "The model is in a module that is not imported."],
            fixes: &["Declare `model {name} = table [ ... ]`.", "Import the module that declares it."],
        },
        CodeDef {
            number: 103,
            name: "Unknown Module",
            kind: ErrorKind::Module,
            meaning: "`bring` names a module that is neither a builtin nor a local `.hard` file.",
            example: "bring cryptoo",
            causes: &["A typo in a builtin such as `crypto`.", "A local module file does not exist at the given path."],
            fixes: &["Check the spelling — did you mean a listed module?", "Create the module or fix the import path."],
        },
        CodeDef {
            number: 104,
            name: "Undefined Variable",
            kind: ErrorKind::Type,
            meaning: "An expression references a variable or constant that is not in scope and is not a function.",
            example: "GET \"/\" :: { <- user_id }",
            causes: &[
                "The variable was never declared, or is declared after this use.",
                "The variable lives in an outer or sibling scope (e.g. middleware).",
                "A typo in the name.",
            ],
            fixes: &[
                "Declare the variable before using it.",
                "Move the declaration into the scope that uses it.",
                "Fix the spelling — see the suggested name.",
            ],
        },
        CodeDef {
            number: 105,
            name: "Duplicate Declaration",
            kind: ErrorKind::Type,
            meaning: "A name (function, constant, variable, parameter) is declared more than once in the same scope.",
            example: "calc total() => Int { <- 1 }\ncalc total() => Int { <- 2 }",
            causes: &["Copy-paste left two declarations alive.", "Two modules declare the same top-level name."],
            fixes: &["Rename one declaration.", "Remove the duplicate."],
        },
        CodeDef {
            number: 106,
            name: "Duplicate Route",
            kind: ErrorKind::Type,
            meaning: "The same HTTP verb and path are declared more than once; the second is unreachable.",
            example: "GET \"/users\" :: { ... }\nGET \"/users\" :: { ... }",
            causes: &["A route was copied and not renamed."],
            fixes: &["Change the path or the verb.", "Remove the duplicate route."],
        },
        CodeDef {
            number: 107,
            name: "Duplicate Middleware",
            kind: ErrorKind::Type,
            meaning: "Two `before`/middleware blocks share the same name.",
            example: "before auth :: { ... }\nbefore auth :: { ... }",
            causes: &["A middleware was copied and not renamed."],
            fixes: &["Give the later middleware a distinct name.", "Remove the duplicate block."],
        },
        CodeDef {
            number: 108,
            name: "Duplicate Model",
            kind: ErrorKind::Type,
            meaning: "Two `model` declarations share a name.",
            example: "model User = users [..]\nmodel User = users [..]",
            causes: &["A model was copied across modules.", "A module was imported twice."],
            fixes: &["Rename one model.", "Import the declaring module once."],
        },
        CodeDef {
            number: 110,
            name: "Undefined Member",
            kind: ErrorKind::Type,
            meaning: "A field or member access names something that does not exist on the value.",
            example: "user.nmae",
            causes: &["A typo in the field name.", "The field belongs to a different model."],
            fixes: &["Check the field names of the model.", "Use the suggested spelling from the diagnostic."],
        },

        // ---------------- Types (HS0200..=HS0299) ----------------
        CodeDef {
            number: 201,
            name: "Type Mismatch",
            kind: ErrorKind::Type,
            meaning: "A value of one type was used where a different type was required.",
            example: "x ::= \"a\"\n<- x + 1  (Text + Int)",
            causes: &["Operands of mixed types.", "Passing a typed value to the wrong slot."],
            fixes: &[
                "Convert one side explicitly, e.g. `Int(x)` or `Str(x)`.",
                "Check the declared return type of the function.",
            ],
        },
        CodeDef {
            number: 202,
            name: "Wrong Argument Count",
            kind: ErrorKind::Type,
            meaning: "A function call passes a different number of arguments than the declaration accepts.",
            example: "calc pair(a, b) => Int { .. }\npair(1)",
            causes: &["Missing or extra arguments in a call."],
            fixes: &["Match the argument list to the `calc` signature.", "Add or remove arguments."],
        },
        CodeDef {
            number: 203,
            name: "Operator Type Error",
            kind: ErrorKind::Type,
            meaning: "An operator is applied to a value it does not support.",
            example: "\"a\" / 2",
            causes: &["Arithmetic on text.", "Grouping or logic on numbers."],
            fixes: &["Use the operator on values of the matching type.", "Convert first."],
        },
        CodeDef {
            number: 204,
            name: "Invalid Field or Index Access",
            kind: ErrorKind::Type,
            meaning: "An index or member access is not valid on the value's type.",
            example: "xs[0]  when xs is an object, not a list",
            causes: &["Indexing a non-list", "Reading a field that is not on the value."],
            fixes: &["Check the value's shape.", "Use the access that matches the type."],
        },
        CodeDef {
            number: 205,
            name: "Return Type Mismatch",
            kind: ErrorKind::Type,
            meaning: "A function returns a value that does not match its declared `=> Type`.",
            example: "calc f() => Int { <- \"hi\" }",
            causes: &["The return expression has the wrong type."],
            fixes: &["Return a value of the declared type.", "Change the declared return type."],
        },
        CodeDef {
            number: 206,
            name: "Condition Not Boolean",
            kind: ErrorKind::Type,
            meaning: "A condition (`?(cond)`, `loop`) is not a `Bool` value.",
            example: "?(users) { .. }",
            causes: &["Using a non-boolean value as the condition."],
            fixes: &["Compare explicitly, e.g. `?(users.count > 0)`."],
        },

        // ---------------- Modules (HS0300..=HS0399) ----------------
        CodeDef {
            number: 301,
            name: "Import Cycle",
            kind: ErrorKind::Module,
            meaning: "Two or more modules import each other (directly or transitively).",
            example: "a.hard: bring \"./b\"   b.hard: bring \"./a\"",
            causes: &["Circular dependencies introduced while refactoring."],
            fixes: &["Break the cycle by moving the shared code into a third module.", "Import only one direction."],
        },
        CodeDef {
            number: 302,
            name: "Module Not Found",
            kind: ErrorKind::Module,
            meaning: "A `bring \"./path\"` points at a file that does not exist.",
            example: "bring \"./models/user\"  (models/user.hard is missing)",
            causes: &["The file was renamed or deleted.", "The path is a typo."],
            fixes: &["Create the module or correct the path.", "Check the extension `.hard` is included."],
        },
        CodeDef {
            number: 303,
            name: "Import Chain Too Deep",
            kind: ErrorKind::Module,
            meaning: "The module dependency graph nests deeper than the supported limit.",
            example: "m1 -> m2 -> ... -> m257",
            causes: &["An over-broad transitive import chain."],
            fixes: &["Flatten the graph so modules import siblings, not chains."],
        },
        CodeDef {
            number: 304,
            name: "Invalid Import Target",
            kind: ErrorKind::Module,
            meaning: "`bring` named something that is not an importable module.",
            example: "bring \"./data.json\"",
            causes: &["Importing a non-module file.", "Importing a directory instead of a module."],
            fixes: &["Import a `.hard` module file or a builtin."],
        },

        // ---------------- Optimizer (HS0400..=HS0499) ----------------
        CodeDef {
            number: 401,
            name: "Optimizer Internal Error",
            kind: ErrorKind::Codegen,
            meaning: "The optimizer reached a state it cannot legally transform; this is an internal bug.",
            example: "(internal — not triggered by user source)",
            causes: &["An invariant in the optimizer pipeline was violated."],
            fixes: &["Report the snippet to the compiler maintainers."],
        },
        CodeDef {
            number: 402,
            name: "Optimizer Unsoundness",
            kind: ErrorKind::Codegen,
            meaning: "An optimization would change program behavior, so it was refused.",
            example: "(internal — not triggered by user source)",
            causes: &["A transformation was applied to a construct it does not model yet."],
            fixes: &["Rewrite the construct explicitly."],
        },

        // ---------------- Internal / native (HS0500..=HS0599) ----------------
        CodeDef {
            number: 501,
            name: "Internal Compiler Error",
            kind: ErrorKind::Codegen,
            meaning: "The compiler hit an unexpected internal state.",
            example: "(internal)",
            causes: &["A bug in the compiler."],
            fixes: &["Minimize the source and report it as an issue."],
        },
        CodeDef {
            number: 502,
            name: "Native Compile Failure",
            kind: ErrorKind::Codegen,
            meaning: "The generated C++ did not compile cleanly with the native compiler (g++).",
            example: "(emitted C++ failed to compile)",
            causes: &["A hardware/runtime mismatch.", "An internal codegen bug surfaced by unusual inputs."],
            fixes: &["Check the toolchain with `hard doctor`.", "Report the failing generated C++."],
        },
        CodeDef {
            number: 503,
            name: "I/O Error",
            kind: ErrorKind::Module,
            meaning: "The compiler could not read or write a file it needed.",
            example: "cannot read main.hard",
            causes: &["Missing permissions.", "The file was deleted while building."],
            fixes: &["Restore the file or fix permissions.", "Re-run the command."],
        },

        // ---------------- Formatter & CLI (HS0600..=HS0699) ----------------
        CodeDef {
            number: 601,
            name: "Formatting Error",
            kind: ErrorKind::Fmt,
            meaning: "The formatter could not produce valid output for the source.",
            example: "(source that cannot round-trip)",
            causes: &["A construct the formatter does not model yet."],
            fixes: &["Format a smaller file, or fix the syntax first."],
        },
        CodeDef {
            number: 602,
            name: "CLI Usage Error",
            kind: ErrorKind::Fmt,
            meaning: "A command-line flag or argument is missing or misspelled.",
            example: "hard build --jobs",
            causes: &["A flag without its value.", "An unknown command."],
            fixes: &["Run `hard help` and follow the usage."],
        },

        // ---------------- Warnings (HS2000..=HS2099) ----------------
        CodeDef {
            number: 2001,
            name: "Unused Variable",
            kind: ErrorKind::Type,
            meaning: "A variable or constant is declared but never read.",
            example: "tmp ::= compute()",
            causes: &["Dead code from an edit.", "A comment-out that left the declaration behind."],
            fixes: &["Remove the declaration or use it."],
        },
        CodeDef {
            number: 2002,
            name: "Unused Function",
            kind: ErrorKind::Type,
            meaning: "A `calc` is never called anywhere in the program.",
            example: "calc helper() => Int { <- 1 }  (never called)",
            causes: &["The function is not reachable from any route."],
            fixes: &["Call it, or remove the declaration."],
        },
        CodeDef {
            number: 2003,
            name: "Unused Import",
            kind: ErrorKind::Type,
            meaning: "A `bring` module is imported but no member of it is referenced.",
            example: "bring crypto  (never used)",
            causes: &["The import was left over from an edit."],
            fixes: &["Remove the `bring` line."],
        },
        CodeDef {
            number: 2004,
            name: "Shadowed Variable",
            kind: ErrorKind::Type,
            meaning: "An inner scope redeclares a name that already exists in an outer scope.",
            example: "x ::= 1\n?(ok) { x ::= 2 }",
            causes: &["Reusing a name for a different value is confusing."],
            fixes: &["Rename the inner variable."],
        },
        CodeDef {
            number: 2005,
            name: "Dead Code",
            kind: ErrorKind::Type,
            meaning: "A construct can never run because it follows a return on every path.",
            example: "<- 1\n<- 2",
            causes: &["Statements after a terminal `<-`."],
            fixes: &["Delete the unreachable statements."],
        },
        CodeDef {
            number: 2006,
            name: "Constant Condition",
            kind: ErrorKind::Type,
            meaning: "A conditional's condition is a literal that never changes.",
            example: "?(true) { ... }",
            causes: &["A hard-coded condition."],
            fixes: &["Use a variable, or collapse the branch."],
        },
        CodeDef {
            number: 2007,
            name: "Redundant Return",
            kind: ErrorKind::Type,
            meaning: "A final `<- expr` duplicates an already-terminal expression.",
            example: "<- f()\n<- f()",
            causes: &["Copy-pasted return."],
            fixes: &["Remove one return."],
        },
        CodeDef {
            number: 2008,
            name: "Empty Block",
            kind: ErrorKind::Type,
            meaning: "A route, model, function, middleware or condition body has no statements.",
            example: "GET \"/\" :: { }",
            causes: &["The body was never filled in."],
            fixes: &["Add statements or remove the block."],
        },
        CodeDef {
            number: 2009,
            name: "Unreachable Statement",
            kind: ErrorKind::Type,
            meaning: "A statement can never execute because a previous statement always returns.",
            example: "<- 1\nfetch()",
            causes: &["Code placed after a return."],
            fixes: &["Move the statement before the return, or delete it."],
        },
        CodeDef {
            number: 2010,
            name: "Duplicate Middleware",
            kind: ErrorKind::Type,
            meaning: "Two middleware blocks share a name; the second never runs.",
            example: "before auth :: { ... }\nbefore auth :: { ... }",
            causes: &["Copy-paste."],
            fixes: &["Rename one middleware or remove it."],
        },
        CodeDef {
            number: 2011,
            name: "Duplicate Route",
            kind: ErrorKind::Type,
            meaning: "The same verb + path is declared twice; the second route is unreachable.",
            example: "GET \"/\" :: { ... }\nGET \"/\" :: { ... }",
            causes: &["Copy-paste."],
            fixes: &["Change the path or remove one route."],
        },
        CodeDef {
            number: 2012,
            name: "Empty Model",
            kind: ErrorKind::Type,
            meaning: "A `model` declares no fields.",
            example: "model User = users [ ]",
            causes: &["The model was not filled in."],
            fixes: &["Add at least one field."],
        },
        CodeDef {
            number: 2013,
            name: "Constant Expression",
            kind: ErrorKind::Type,
            meaning: "An expression produces a value that never depends on inputs.",
            example: "x ::= 2 + 2",
            causes: &["A literal expression where a reference was intended."],
            fixes: &["Use the intended variable or function."],
        },
        CodeDef {
            number: 2014,
            name: "Always True Comparison",
            kind: ErrorKind::Type,
            meaning: "A comparison is always true regardless of the values.",
            example: "x ::= 1\n.. x == 1 ..",
            causes: &["Comparing a known literal against itself or a constant."],
            fixes: &["Compare against the intended value."],
        },
        CodeDef {
            number: 2015,
            name: "Always False Comparison",
            kind: ErrorKind::Type,
            meaning: "A comparison is always false regardless of the values.",
            example: "x ::= 1\n.. x > 100 ..",
            causes: &["Comparing against an impossible value."],
            fixes: &["Fix the comparison or the value."],
        },
    ]
}

/// Look up a catalog entry by numeric code.
pub fn lookup(number: u16) -> Option<&'static CodeDef> {
    catalog().iter().find(|c| c.number == number)
}

/// The numeric code of a warning category, given its registered name.
///
/// Names here are the canonical identifiers used by the warning engine
/// (`--deny`/`--warnings` accept them case-insensitively).
pub const WARNINGS: &[(&str, u16)] = &[
    ("unused-variable", 2001),
    ("unused-function", 2002),
    ("unused-import", 2003),
    ("shadowed-variable", 2004),
    ("dead-code", 2005),
    ("constant-condition", 2006),
    ("redundant-return", 2007),
    ("empty-block", 2008),
    ("unreachable-statement", 2009),
    ("duplicate-middleware", 2010),
    ("duplicate-route", 2011),
    ("duplicate-model", 2012),
    ("constant-expression", 2013),
    ("always-true-comparison", 2014),
    ("always-false-comparison", 2015),
];

/// Default code for a diagnostic whose site has not been given a granular
/// code. Grouped by [`ErrorKind`] so existing and future call sites always
/// have a valid, on-range code.
pub fn default_for(kind: ErrorKind) -> u16 {
    match kind {
        ErrorKind::Lex => 4,    // Malformed Literal is the generic lexer family
        ErrorKind::Parse => 1,  // Unexpected Token
        ErrorKind::Type => 104, // Undefined Variable (most common type diag)
        ErrorKind::Codegen => 501,
        ErrorKind::Fmt => 601,
        ErrorKind::Module => 302, // Module Not Found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_sorted_and_unique() {
        let c = catalog();
        assert!(c.len() >= 45, "catalog should cover all documented ranges");
        for pair in c.windows(2) {
            assert!(
                pair[0].number < pair[1].number,
                "catalog not ascending: HS{} before HS{}",
                pair[0].number,
                pair[1].number
            );
        }
        let seen: std::collections::HashSet<u16> = c.iter().map(|d| d.number).collect();
        assert_eq!(seen.len(), c.len(), "codes must be unique");
    }

    #[test]
    fn format_matches_spec() {
        assert_eq!(format(1), "HS0001");
        assert_eq!(format(104), "HS0104");
        assert_eq!(format(2015), "HS2015");
    }

    #[test]
    fn warnings_are_registered_with_reserved_range() {
        assert!(WARNINGS.len() >= 15, "need at least 15 warning categories");
        let codes: Vec<u16> = WARNINGS.iter().map(|(_, n)| *n).collect();
        let uniq: std::collections::HashSet<u16> = codes.iter().copied().collect();
        assert_eq!(uniq.len(), codes.len(), "warning codes must be unique");
        for n in codes {
            assert!((2000..=2099).contains(&n), "warning {n} outside reserved range");
            assert!(lookup(n).is_some(), "warning {n} missing from catalog");
        }
    }

    #[test]
    fn default_codes_land_in_their_reserved_range() {
        for k in [
            ErrorKind::Lex,
            ErrorKind::Parse,
            ErrorKind::Type,
            ErrorKind::Codegen,
            ErrorKind::Fmt,
            ErrorKind::Module,
        ] {
            let n = default_for(k);
            assert!(lookup(n).is_some(), "default code {n} not in catalog");
        }
    }
}