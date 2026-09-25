//! Markdown documentation generator. Produces a compact `API.md` describing
//! the routes, models, sockets, functions and tests in a program.

use crate::ast::*;

pub fn render_markdown(prog: &Program) -> String {
    let title = prog
        .path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("app")
        .to_string();
    let mut out = String::new();
    out.push_str(&format!("# {title}\n\n"));
    out.push_str("HardScript application overview.\n\n");

    if let Some(p) = prog
        .stmts
        .iter()
        .find_map(|s| if let Stmt::App(p, _) = s { Some(*p) } else { None })
    {
        out.push_str(&format!("- Port: `{p}`\n"));
    }
    for m in prog
        .stmts
        .iter()
        .filter_map(|s| if let Stmt::Bring(m, _) = s { Some(m) } else { None })
    {
        out.push_str(&format!("- Module: `{}`\n", module_name(m)));
    }
    out.push_str("\n");

    let mut routes: Vec<&RouteDef> = Vec::new();
    let mut models: Vec<&ModelDef> = Vec::new();
    let mut sockets: Vec<&SocketDef> = Vec::new();
    let mut funcs: Vec<&FunDef> = Vec::new();
    let mut tests: Vec<&TestDef> = Vec::new();
    for s in &prog.stmts {
        match s {
            Stmt::Route(r) => routes.push(r),
            Stmt::Model(m) => models.push(m),
            Stmt::Socket(s) => sockets.push(s),
            Stmt::Func(f) => funcs.push(f),
            Stmt::Test(t) => tests.push(t),
            _ => {}
        }
    }

    if !routes.is_empty() {
        out.push_str("## Routes\n\n");
        for r in &routes {
            out.push_str(&format!(
                "- `{} {path}` -> `{path}`\n",
                r.method,
                path = r.path
            ));
        }
        out.push_str("\n");
    }

    if !models.is_empty() {
        out.push_str("## Models\n\n");
        for m in &models {
            let head = match (m.table == m.name.to_lowercase(), m.strict) {
                (true, true) => format!("### `{}` (table `{}`, strict)\n\n", m.name, m.table),
                (true, false) => format!("### `{}` (table `{}`)\n\n", m.name, m.table),
                (false, true) => format!("### `{}` (table `{}`, strict)\n\n", m.name, m.table),
                (false, false) => format!("### `{}` (table `{}`)\n\n", m.name, m.table),
            };
            out.push_str(&head);
            // The declared type is the interesting part, so carry the
            // constraint list and the presence marker: `Int!(min=18, max=120)`
            // says more than `Int` plus a separate attributes column.
            out.push_str("| Field | Type | Constraints | Attributes |\n|---|---|---|---|\n");
            for f in &m.fields {
                let constraints = f
                    .args
                    .iter()
                    .map(|a| match a {
                        FieldArg::Positional(e) => e.render(),
                        FieldArg::Constraint(k, v) => format!("{k}={}", v.render()),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let attrs = f
                    .attrs
                    .iter()
                    .filter(|a| !(a.arg.is_none() && matches!(a.name.as_str(), "required" | "nullable" | "optional")))
                    .map(|a| match &a.arg {
                        Some(e) => format!("@{}({})", a.name, e.render()),
                        None => format!("@{}", a.name),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let presence = if f.attrs.iter().any(|a| a.arg.is_none() && a.name == "required") {
                    "required"
                } else if f.attrs.iter().any(|a| {
                    a.arg.is_none() && matches!(a.name.as_str(), "nullable" | "optional")
                }) {
                    "optional"
                } else {
                    ""
                };
                let constraints = if presence.is_empty() {
                    constraints
                } else if constraints.is_empty() {
                    presence.to_string()
                } else {
                    format!("{presence}, {constraints}")
                };
                out.push_str(&format!("| {} | {} | {} | {} |\n", f.name, f.ty, constraints, attrs));
            }
            out.push_str("\n");
        }
    }

    if !sockets.is_empty() {
        out.push_str("## Sockets\n\n");
        for s in &sockets {
            out.push_str(&format!("- WebSocket at `{}`\n", s.path));
        }
        out.push_str("\n");
    }

    if !funcs.is_empty() {
        out.push_str("## Functions\n\n");
        for f in &funcs {
            let params: Vec<String> = f.params.iter().map(|p| p.name.clone()).collect();
            out.push_str(&format!(
                "- `calc {}({}){}\n",
                f.name,
                params.join(", "),
                f.ret.as_ref().map(|t| format!(" => {t}")).unwrap_or_default()
            ));
        }
        out.push_str("\n");
    }

    if !tests.is_empty() {
        out.push_str("## Tests\n\n");
        for t in &tests {
            out.push_str(&format!("- `{}`\n", t.name));
        }
        out.push_str("\n");
    }

    out
}

fn module_name(m: &Module) -> &str {
    m.as_str()
}