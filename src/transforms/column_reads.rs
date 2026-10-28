//! Column projection · which fields of a live binding's rows a component reads.
//!
//! ## Why
//!
//! A live topic's value is the whole materialised collection: every row, every
//! declared column. The page that subscribes renders some of them. Before this,
//! the lanes shipped the value as stored, so a public page that rendered a
//! waitlist's *count* received every email on it, and a page that rendered a
//! post's title received its author's internal flags. ALB-01 (2026-10-27) closed
//! the cross-*route* half of that — a route is granted only the topics it renders
//! — and this is the cross-*column* half: a route receives only the fields it
//! reads.
//!
//! ## What is derived
//!
//! [`column_reads`] walks a component's body for one binding and answers with the
//! top-level field names read from it — `e.title`, `{ title } = e`,
//! `key={e.id}` — through the shapes real components write: `.map`/`.filter`
//! /`.find` callbacks and chains of them, `.length`, a `const` alias of the
//! binding or of a chain on it, and `find`'s single row.
//!
//! ## What is not, and the direction it fails
//!
//! Anything the walk cannot bound — a row handed to a child component, spread,
//! passed to a function, indexed by a computed key, a callback passed by name —
//! answers [`ColumnReads::All`]. That is the pre-projection behaviour, not a
//! guess: the cost of being unsure is the status quo, never a page painting a
//! field it was not sent. The fields a row is *rendered* with never cross the
//! wire as JSON anyway when the list ships as row HTML; this bounds the JSON that
//! does (the seed every subscriber gets, and the snapshot a write falls back to).

use crate::runtime::eval::expr::ComponentFunction;
use std::collections::{BTreeSet, HashMap};
use swc_ecma_ast::{
    BlockStmtOrExpr, Callee, Expr, MemberExpr, MemberProp, ObjectPatProp, OptChainBase, Pat,
    PropName, VarDeclarator,
};
use swc_ecma_visit::{Visit, VisitWith};

/// The fields of a binding's rows one component (or one route) reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnReads {
    /// Unbounded: something the walk could not follow saw the whole row.
    All,
    /// Exactly these top-level fields. Empty is legitimate — `rows.length`
    /// reads the count and no column.
    Only(BTreeSet<String>),
}

impl ColumnReads {
    /// Widen `self` to also cover `other`. `All` absorbs everything.
    pub fn merge(&mut self, other: &ColumnReads) {
        match (&mut *self, other) {
            (ColumnReads::All, _) => {}
            (_, ColumnReads::All) => *self = ColumnReads::All,
            (ColumnReads::Only(mine), ColumnReads::Only(theirs)) => {
                mine.extend(theirs.iter().cloned());
            }
        }
    }
}

/// The fields of `binding`'s value that `function` reads. See the module docs.
#[must_use]
pub fn column_reads(function: &ComponentFunction, binding: &str) -> ColumnReads {
    let mut walk = Walk {
        tracked: HashMap::from([(binding.to_string(), Role::Value)]),
        fields: BTreeSet::new(),
        all: false,
    };
    for stmt in &function.body_stmts {
        stmt.visit_with(&mut walk);
        if walk.all {
            return ColumnReads::All;
        }
    }
    ColumnReads::Only(walk.fields)
}

/// The key a binding's reads are filed under in
/// [`RouteManifest::shared_slot_columns`](crate::manifest::schema::RouteManifest):
/// what the binding names, before a request resolves it to a topic.
#[must_use]
pub fn column_key(spec: &crate::transforms::shared_slots::TopicSpec) -> String {
    use crate::transforms::shared_slots::TopicSpec;
    match spec {
        TopicSpec::Static(topic) => topic.clone(),
        TopicSpec::Partition { collection, .. } => collection.clone(),
        TopicSpec::Source { source, route, .. } => format!("aperture:{source}.{route}"),
    }
}

/// The columns a route may receive for the resolved `topic`, from its
/// `shared_slot_columns`. `None` means every column.
///
/// Inverts [`column_key`]: a static topic is its own key; a source topic
/// (`aperture:github.repo:owner=…`) is filed under the part before its
/// arguments; a partition (`messages:room-a`) under its collection. The whole
/// name is tried first, which settles a static topic that contains a colon.
#[must_use]
pub fn columns_for_topic<'a>(
    columns: &'a std::collections::BTreeMap<String, Vec<String>>,
    topic: &str,
) -> Option<&'a [String]> {
    if columns.is_empty() {
        return None;
    }
    if let Some(found) = columns.get(topic) {
        return Some(found);
    }
    if let Some(rest) = topic.strip_prefix("aperture:") {
        let base = &topic[..topic.len() - rest.len() + rest.find(':').unwrap_or(rest.len())];
        return columns.get(base).map(Vec::as_slice);
    }
    let (collection, _) = crate::runtime::split_partition_topic(topic)?;
    columns.get(collection).map(Vec::as_slice)
}

/// A route's resolved `topics` as subscription grants, each carrying the
/// columns the route reads from it (`None` — all — where the walk could not
/// bound them). Equal column sets share nothing but their contents, which is
/// what the fan-out keys its one-encode-per-set cache on.
#[must_use]
pub fn route_grants(
    columns: &std::collections::BTreeMap<String, Vec<String>>,
    topics: &[String],
) -> Vec<crate::runtime::TopicGrant> {
    topics
        .iter()
        .map(|topic| crate::runtime::TopicGrant {
            topic: topic.clone(),
            columns: columns_for_topic(columns, topic)
                .map(|fields| std::sync::Arc::new(fields.iter().cloned().collect())),
        })
        .collect()
}

/// What a tracked expression holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The binding itself: a collection's rows, or a source's object. A field
    /// read straight off it is an object field; an array method treats it as
    /// rows.
    Value,
    /// An array known to hold the binding's rows (`filter`, `slice`, an alias).
    Rows,
    /// One row (a callback parameter, `find`'s result).
    Row,
}

/// Methods whose callback's first parameter is a row.
const ROW_CALLBACKS: &[&str] = &[
    "every",
    "filter",
    "find",
    "findIndex",
    "findLast",
    "findLastIndex",
    "flatMap",
    "forEach",
    "map",
    "some",
];

/// Methods that read no field and return something other than rows.
const OPAQUE_READS: &[&str] = &["includes", "indexOf", "lastIndexOf"];

struct Walk {
    tracked: HashMap<String, Role>,
    fields: BTreeSet<String>,
    all: bool,
}

/// `e` without the wrappers that do not change its value.
fn unwrap(mut e: &Expr) -> &Expr {
    loop {
        e = match e {
            Expr::Paren(inner) => &inner.expr,
            Expr::TsAs(inner) => &inner.expr,
            Expr::TsNonNull(inner) => &inner.expr,
            Expr::TsSatisfies(inner) => &inner.expr,
            Expr::TsTypeAssertion(inner) => &inner.expr,
            Expr::TsConstAssertion(inner) => &inner.expr,
            _ => return e,
        };
    }
}

/// `a.b` or `a?.b`.
fn as_member(e: &Expr) -> Option<&MemberExpr> {
    match unwrap(e) {
        Expr::Member(member) => Some(member),
        Expr::OptChain(chain) => match &*chain.base {
            OptChainBase::Member(member) => Some(member),
            OptChainBase::Call(_) => None,
        },
        _ => None,
    }
}

/// `callee(args)` or `callee?.(args)`, as `(callee, args)`.
fn as_call(e: &Expr) -> Option<(&Expr, &[swc_ecma_ast::ExprOrSpread])> {
    match unwrap(e) {
        Expr::Call(call) => match &call.callee {
            Callee::Expr(callee) => Some((callee, &call.args)),
            _ => None,
        },
        Expr::OptChain(chain) => match &*chain.base {
            OptChainBase::Call(call) => Some((&call.callee, &call.args)),
            OptChainBase::Member(_) => None,
        },
        _ => None,
    }
}

/// `obj.method(args)`, as `(obj, method, args)`.
fn as_method_call(e: &Expr) -> Option<(&Expr, &str, &[swc_ecma_ast::ExprOrSpread])> {
    let (callee, args) = as_call(e)?;
    let member = as_member(callee)?;
    let MemberProp::Ident(method) = &member.prop else {
        return None;
    };
    Some((&member.obj, method.sym.as_ref(), args))
}

/// `rows.reduce((best, row) => (best && best.at >= row.at ? best : row), null)`
/// — a reduce that starts from nothing and whose every result is the
/// accumulator or the current row, so what it returns is one of the rows. The
/// "pick the newest" idiom. Returns `(accumulator, row, body)`.
fn picking_reduce(args: &[swc_ecma_ast::ExprOrSpread]) -> Option<(String, String, &Expr)> {
    use swc_ecma_ast::Lit;
    fn nothing(e: &Expr) -> bool {
        match unwrap(e) {
            Expr::Lit(Lit::Null(_)) => true,
            Expr::Ident(ident) => ident.sym.as_ref() == "undefined",
            _ => false,
        }
    }
    fn leaves(e: &Expr, acc: &str, row: &str) -> bool {
        match unwrap(e) {
            Expr::Cond(cond) => leaves(&cond.cons, acc, row) && leaves(&cond.alt, acc, row),
            Expr::Ident(ident) => matches!(ident.sym.as_ref(), name if name == acc || name == row),
            other => nothing(other),
        }
    }
    let callback = args.first().filter(|arg| arg.spread.is_none())?;
    if let Some(init) = args.get(1) {
        if !nothing(&init.expr) {
            return None;
        }
    }
    let Expr::Arrow(arrow) = unwrap(&callback.expr) else {
        return None;
    };
    let [Pat::Ident(acc), Pat::Ident(row), ..] = arrow.params.as_slice() else {
        return None;
    };
    let BlockStmtOrExpr::Expr(body) = &*arrow.body else {
        return None;
    };
    let (acc, row) = (acc.id.sym.to_string(), row.id.sym.to_string());
    leaves(body, &acc, &row).then_some((acc, row, &**body))
}

impl Walk {
    /// The role `e` plays, when it is a tracked name or something derived from
    /// one that still holds rows.
    fn classify(&self, e: &Expr) -> Option<Role> {
        let e = unwrap(e);
        if let Expr::Ident(ident) = e {
            return self.tracked.get(ident.sym.as_ref()).copied();
        }
        // `rows || []` / `rows ?? []` — the fallback the scaffold warns against,
        // and people write anyway.
        if let Expr::Bin(bin) = e {
            use swc_ecma_ast::BinaryOp::{LogicalOr, NullishCoalescing};
            if matches!(bin.op, LogicalOr | NullishCoalescing)
                && matches!(unwrap(&bin.right), Expr::Array(_) | Expr::Object(_))
            {
                return self.classify(&bin.left);
            }
            return None;
        }
        let (obj, method, args) = as_method_call(e)?;
        if !matches!(self.classify(obj)?, Role::Value | Role::Rows) {
            return None;
        }
        match method {
            "filter" | "slice" | "sort" | "toSorted" | "reverse" | "toReversed" | "concat" => {
                Some(Role::Rows)
            }
            "find" | "findLast" | "at" => Some(Role::Row),
            "reduce" | "reduceRight" if picking_reduce(args).is_some() => Some(Role::Row),
            _ => None,
        }
    }

    /// `row && …`, `!row`, `row ? … : …`, `row == null`, `typeof row` — a
    /// test of whether a row is there, which reads no column. `None` when `e`
    /// is not such a test.
    fn existence_test<'e>(&self, e: &'e Expr) -> Option<&'e Expr> {
        use swc_ecma_ast::{BinaryOp, Lit, UnaryOp};
        let is_nothing = |e: &Expr| match unwrap(e) {
            Expr::Lit(Lit::Null(_)) => true,
            Expr::Ident(ident) => ident.sym.as_ref() == "undefined",
            _ => false,
        };
        match unwrap(e) {
            Expr::Unary(unary) if matches!(unary.op, UnaryOp::Bang | UnaryOp::TypeOf) => {
                self.classify(&unary.arg).is_some().then_some(&*unary.arg)
            }
            Expr::Bin(bin)
                if matches!(
                    bin.op,
                    BinaryOp::EqEq | BinaryOp::EqEqEq | BinaryOp::NotEq | BinaryOp::NotEqEq
                ) =>
            {
                if is_nothing(&bin.right) && self.classify(&bin.left).is_some() {
                    Some(&*bin.left)
                } else if is_nothing(&bin.left) && self.classify(&bin.right).is_some() {
                    Some(&*bin.right)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Walk a classified expression without counting its root as a bare use —
    /// it is being read through a member or a method, which is what the caller
    /// already accounted for. Chained calls on the way down are analysed.
    fn consume(&mut self, e: &Expr) {
        let e = unwrap(e);
        if let Expr::Ident(_) = e {
            return;
        }
        if let Expr::Bin(bin) = e {
            self.consume(&bin.left);
            bin.right.visit_with(self);
            return;
        }
        if let Some((obj, method, args)) = as_method_call(e) {
            self.method(method, args);
            self.consume(obj);
        }
    }

    /// A read `.prop` off something holding `role`.
    fn member(&mut self, role: Role, prop: &MemberProp) {
        match prop {
            MemberProp::Ident(name) => {
                let name = name.sym.as_ref();
                match role {
                    Role::Row => {
                        self.fields.insert(name.to_string());
                    }
                    Role::Rows if name == "length" => {}
                    // Kept for a source's object, harmless for an array — a row
                    // has no field named `length` to over-share.
                    Role::Value => {
                        if name != "length" {
                            self.fields.insert(name.to_string());
                        }
                    }
                    Role::Rows => self.all = true,
                }
            }
            MemberProp::Computed(computed) => {
                self.all = true;
                computed.expr.visit_with(self);
            }
            MemberProp::PrivateName(_) => self.all = true,
        }
    }

    /// `rows.method(args)`: bind the callback's row parameters and walk it.
    fn method(&mut self, method: &str, args: &[swc_ecma_ast::ExprOrSpread]) {
        if matches!(method, "reduce" | "reduceRight") {
            if let Some((acc, row, body)) = picking_reduce(args) {
                let saved: Vec<(String, Option<Role>)> = [acc, row]
                    .into_iter()
                    .map(|name| {
                        let previous = self.tracked.insert(name.clone(), Role::Row);
                        (name, previous)
                    })
                    .collect();
                self.picked(body);
                for (name, previous) in saved.into_iter().rev() {
                    match previous {
                        Some(role) => self.tracked.insert(name, role),
                        None => self.tracked.remove(&name),
                    };
                }
                return;
            }
        }
        let row_params: &[usize] = if ROW_CALLBACKS.contains(&method) {
            &[0]
        } else if matches!(method, "sort" | "toSorted") {
            &[0, 1]
        } else if matches!(method, "reduce" | "reduceRight") {
            &[1]
        } else if matches!(
            method,
            "slice" | "reverse" | "toReversed" | "concat" | "at"
        ) || OPAQUE_READS.contains(&method)
        {
            &[]
        } else {
            self.all = true;
            return;
        };
        for (index, arg) in args.iter().enumerate() {
            if index == 0 && !row_params.is_empty() {
                self.callback(&arg.expr, row_params);
            } else {
                arg.expr.visit_with(self);
            }
        }
    }

    /// Walk an inline callback with the parameters at `row_params` bound as
    /// rows. A callback passed by name (`rows.map(renderRow)`) cannot be read
    /// from here, so it is unbounded.
    fn callback(&mut self, callback: &Expr, row_params: &[usize]) {
        let (params, body): (Vec<&Pat>, Box<dyn Fn(&mut Walk) + '_>) = match unwrap(callback) {
            Expr::Arrow(arrow) => (
                arrow.params.iter().collect(),
                Box::new(move |walk: &mut Walk| match &*arrow.body {
                    BlockStmtOrExpr::BlockStmt(block) => block.visit_with(walk),
                    BlockStmtOrExpr::Expr(expr) => expr.visit_with(walk),
                }),
            ),
            Expr::Fn(function) => (
                function.function.params.iter().map(|param| &param.pat).collect(),
                Box::new(move |walk: &mut Walk| {
                    if let Some(body) = &function.function.body {
                        body.visit_with(walk);
                    }
                }),
            ),
            _ => {
                self.all = true;
                return;
            }
        };

        let mut restore: Vec<(String, Option<Role>)> = Vec::new();
        for &index in row_params {
            let Some(param) = params.get(index) else {
                continue;
            };
            match param {
                Pat::Ident(ident) => {
                    let name = ident.id.sym.to_string();
                    let previous = self.tracked.insert(name.clone(), Role::Row);
                    restore.push((name, previous));
                }
                pattern => self.destructure_row(pattern),
            }
        }
        // `(row, i, all) => …` — the third parameter of an array callback is
        // the array itself.
        if row_params == [0] {
            if let Some(Pat::Ident(ident)) = params.get(2) {
                let name = ident.id.sym.to_string();
                let previous = self.tracked.insert(name.clone(), Role::Rows);
                restore.push((name, previous));
            }
        }
        body(self);
        for (name, previous) in restore.into_iter().rev() {
            match previous {
                Some(role) => {
                    self.tracked.insert(name, role);
                }
                None => {
                    self.tracked.remove(&name);
                }
            }
        }
    }

    /// A picking reduce's result expression: its leaves are the accumulator or
    /// the row flowing on — not a use — and its tests are walked as tests.
    fn picked(&mut self, e: &Expr) {
        match unwrap(e) {
            Expr::Cond(cond) => {
                cond.test.visit_with(self);
                self.picked(&cond.cons);
                self.picked(&cond.alt);
            }
            Expr::Ident(_) | Expr::Lit(_) => {}
            other => other.visit_with(self),
        }
    }

    /// `({ title, author: who }) => …` / `const { title } = row` — the keys
    /// named are the fields read. A rest element sees everything.
    fn destructure_row(&mut self, pattern: &Pat) {
        let Pat::Object(object) = pattern else {
            self.all = true;
            return;
        };
        for prop in &object.props {
            match prop {
                ObjectPatProp::KeyValue(kv) => match &kv.key {
                    PropName::Ident(name) => {
                        self.fields.insert(name.sym.to_string());
                    }
                    PropName::Str(name) => {
                        self.fields.insert(name.value.to_string());
                    }
                    _ => self.all = true,
                },
                ObjectPatProp::Assign(assign) => {
                    self.fields.insert(assign.key.sym.to_string());
                    if let Some(default) = &assign.value {
                        default.visit_with(self);
                    }
                }
                ObjectPatProp::Rest(_) => self.all = true,
            }
        }
    }
}

impl Visit for Walk {
    fn visit_expr(&mut self, e: &Expr) {
        if self.all {
            return;
        }
        // A read through a member: `row.title`, `rows.length`, `find(…).title`.
        if let Some(member) = as_member(e) {
            if let Some(role) = self.classify(&member.obj) {
                self.member(role, &member.prop);
                self.consume(&member.obj);
                return;
            }
        }
        // A method on rows. When its result is itself rows (or a row), that
        // result is being used right here, bare — see the arm below.
        if let Some((obj, method, args)) = as_method_call(e) {
            if matches!(self.classify(obj), Some(Role::Value | Role::Rows))
                && self.classify(e).is_none()
            {
                self.method(method, args);
                self.consume(obj);
                return;
            }
        }
        // Asking whether a row is there reads no column. `row && …` is safe in
        // any position, not just a test: a row object is always truthy, so the
        // expression is never the row itself.
        match unwrap(e) {
            Expr::Bin(bin)
                if bin.op == swc_ecma_ast::BinaryOp::LogicalAnd
                    && self.classify(&bin.left).is_some() =>
            {
                self.consume(&bin.left);
                bin.right.visit_with(self);
                return;
            }
            Expr::Cond(cond) if self.classify(&cond.test).is_some() => {
                self.consume(&cond.test);
                cond.cons.visit_with(self);
                cond.alt.visit_with(self);
                return;
            }
            _ => {}
        }
        if let Some(tested) = self.existence_test(e) {
            self.consume(tested);
            return;
        }
        // Rows or a row reaching anything else — a prop, a call argument, a
        // spread, a return value, JSX text — leave the walk's sight whole.
        if self.classify(e).is_some() {
            self.all = true;
            return;
        }
        e.visit_children_with(self);
    }

    /// `if (row) …` — the same existence test as `row ? … : …`.
    fn visit_if_stmt(&mut self, stmt: &swc_ecma_ast::IfStmt) {
        if self.all {
            return;
        }
        if self.classify(&stmt.test).is_some() {
            self.consume(&stmt.test);
            stmt.cons.visit_with(self);
            if let Some(alt) = &stmt.alt {
                alt.visit_with(self);
            }
            return;
        }
        stmt.visit_children_with(self);
    }

    /// `{ rows }` — shorthand is an identifier in the AST, not an expression,
    /// so `visit_expr` never sees it; it hands the whole value on.
    fn visit_prop(&mut self, prop: &swc_ecma_ast::Prop) {
        if let swc_ecma_ast::Prop::Shorthand(ident) = prop {
            if self.tracked.contains_key(ident.sym.as_ref()) {
                self.all = true;
                return;
            }
        }
        prop.visit_children_with(self);
    }

    /// `const visible = rows.filter(…)` and `const first = rows.find(…)` make
    /// new names for the same rows; `const { title } = first` reads fields.
    fn visit_var_declarator(&mut self, declarator: &VarDeclarator) {
        if self.all {
            return;
        }
        let Some(init) = &declarator.init else {
            declarator.visit_children_with(self);
            return;
        };
        let Some(role) = self.classify(init) else {
            declarator.visit_children_with(self);
            return;
        };
        match (&declarator.name, role) {
            (Pat::Ident(ident), _) => {
                self.tracked.insert(ident.id.sym.to_string(), role);
            }
            (pattern @ Pat::Object(_), Role::Row | Role::Value) => self.destructure_row(pattern),
            _ => self.all = true,
        }
        self.consume(init);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn reads(body: &str) -> ColumnReads {
        let source = format!(
            "import {{ useSharedSlot }} from \"albedo\";\n\
             export default function C(props) {{\n  const rows = useSharedSlot(\"posts\");\n{body}\n}}\n"
        );
        let parsed = crate::runtime::eval::expr::parse_module(&source, Path::new("C.tsx"))
            .expect("parses");
        column_reads(parsed.functions.get("C").expect("component"), "rows")
    }

    fn only(fields: &[&str]) -> ColumnReads {
        ColumnReads::Only(fields.iter().map(|f| (*f).to_string()).collect())
    }

    #[test]
    fn a_mapped_list_reads_what_its_row_template_reads() {
        assert_eq!(
            reads("return <ul>{rows.map((r) => <li key={r.id}><a href={r.url}>{r.title}</a></li>)}</ul>;"),
            only(&["id", "title", "url"])
        );
    }

    #[test]
    fn destructured_and_function_callbacks_count() {
        assert_eq!(
            reads("return <ul>{rows.map(({ id, title: t }) => <li key={id}>{t}</li>)}</ul>;"),
            only(&["id", "title"])
        );
        assert_eq!(
            reads("return <ul>{rows.map(function (r) { return <li key={r.id}>{r.body}</li>; })}</ul>;"),
            only(&["body", "id"])
        );
    }

    #[test]
    fn a_count_reads_no_column() {
        assert_eq!(reads("return <p>{rows.length} on the list</p>;"), only(&[]));
        assert_eq!(reads("return <p>{(rows || []).length}</p>;"), only(&[]));
    }

    #[test]
    fn chains_and_aliases_are_followed() {
        assert_eq!(
            reads(
                "const open = rows.filter((r) => !r.done).slice(0, 5);\n\
                 const first = rows.find((r) => r.pinned);\n\
                 return <div><h1>{first?.title}</h1>{open.map((r) => <p key={r.id}>{r.text}</p>)}</div>;"
            ),
            only(&["done", "id", "pinned", "text", "title"])
        );
        assert_eq!(
            reads("return <ol>{rows.toSorted((a, b) => a.rank - b.rank).map((r) => <li key={r.id}>{r.name}</li>)}</ol>;"),
            only(&["id", "name", "rank"])
        );
    }

    /// subtrctn's `/account`, verbatim in shape: pick the newest row, then read
    /// fields off it behind existence tests. The 10-08 falsifier found this page
    /// shipping `owner`, `customer` and `period_end`, which it never reads.
    #[test]
    fn a_picked_row_behind_existence_tests_is_bounded() {
        assert_eq!(
            reads(
                "const now = rows.reduce((newest, row) => (newest && newest.observed_at >= row.observed_at ? newest : row), null);\n\
                 const live = now ? now.status !== \"canceled\" : false;\n\
                 if (!now) return null;\n\
                 return <div>{now.seats ? <p>{now.seats}</p> : null}{now == null ? null : now.plan}</div>;"
            ),
            only(&["observed_at", "plan", "seats", "status"])
        );
        // A plain fold reads what its callback reads.
        assert_eq!(reads("return <p>{rows.reduce((sum, r) => sum + r.amount, 0)}</p>;"), only(&["amount"]));
        // `&&` never yields a row object; a test reads nothing.
        assert_eq!(
            reads("const first = rows.find((r) => r.pinned); return <div>{first && <b>{first.title}</b>}</div>;"),
            only(&["pinned", "title"])
        );
    }

    #[test]
    fn a_picked_row_that_escapes_is_still_all() {
        for body in [
            "const now = rows.reduce((a, r) => (a ? a : r), null); return <Plan row={now} />;",
            "const now = rows.reduce((a, r) => (a ? a : r), {}); return <p>{now.plan}</p>;",
            "const first = rows.find((r) => r.pinned); return <p>{first || 'none'}</p>;",
            "const first = rows.find((r) => r.pinned); return <p>{first ?? 'none'}</p>;",
        ] {
            assert_eq!(reads(body), ColumnReads::All, "{body}");
        }
    }

    #[test]
    fn a_source_object_reads_its_fields() {
        assert_eq!(reads("return <p>{rows.stargazers_count} stars</p>;"), only(&["stargazers_count"]));
    }

    /// The direction it fails: anything it cannot follow is unbounded.
    #[test]
    fn whatever_escapes_the_walk_is_all() {
        for body in [
            "return <ul>{rows.map((r) => <Row key={r.id} entry={r} />)}</ul>;",
            "return <ul>{rows.map(renderRow)}</ul>;",
            "return <ul>{rows.map((r) => <li {...r} />)}</ul>;",
            "return <List items={rows} />;",
            "return <List items={rows.filter((r) => r.live)} />;",
            "return <p>{rows[0].title}</p>;",
            "return <p>{rows.map((r) => r[props.field]).join(',')}</p>;",
            "return <ul>{rows.map(({ id, ...rest }) => <li key={id}>{JSON.stringify(rest)}</li>)}</ul>;",
            "return <pre>{JSON.stringify(rows)}</pre>;",
            "const copy = [...rows]; return <p>{copy.length}</p>;",
            "return <Chart data={{ rows }} />;",
            "return <ul>{rows.map((r) => <li key={r.id}>{format({ r })}</li>)}</ul>;",
        ] {
            assert_eq!(reads(body), ColumnReads::All, "{body}");
        }
    }

    /// A resolved topic finds the key its binding was filed under at build.
    #[test]
    fn a_resolved_topic_finds_its_columns() {
        let columns: std::collections::BTreeMap<String, Vec<String>> = [
            ("guestbook", vec!["message"]),
            ("messages", vec!["body"]),
            ("aperture:github.repo", vec!["stargazers_count"]),
            ("chat:lobby", vec!["text"]),
        ]
        .into_iter()
        .map(|(key, fields)| (key.to_string(), fields.into_iter().map(str::to_string).collect()))
        .collect();
        let found = |topic: &str| columns_for_topic(&columns, topic).map(<[String]>::to_vec);
        assert_eq!(found("guestbook"), Some(vec!["message".to_string()]));
        assert_eq!(found("messages:room-a"), Some(vec!["body".to_string()]), "a partition");
        assert_eq!(
            found("aperture:github.repo:owner=anthropics,name=claude-code"),
            Some(vec!["stargazers_count".to_string()]),
            "a source with arguments"
        );
        assert_eq!(found("aperture:github.repo"), Some(vec!["stargazers_count".to_string()]));
        assert_eq!(found("chat:lobby"), Some(vec!["text".to_string()]), "a static topic with a colon");
        assert_eq!(found("waitlist"), None, "unbounded");
        assert_eq!(found("aperture:other.route:x=1"), None);

        let grants = route_grants(&columns, &["guestbook".to_string(), "waitlist".to_string()]);
        assert_eq!(grants[0].columns.as_deref().map(|set| set.len()), Some(1));
        assert_eq!(grants[1].columns, None);
    }

    #[test]
    fn merging_widens_and_all_absorbs() {
        let mut merged = only(&["a"]);
        merged.merge(&only(&["b"]));
        assert_eq!(merged, only(&["a", "b"]));
        merged.merge(&ColumnReads::All);
        assert_eq!(merged, ColumnReads::All);
        merged.merge(&only(&["c"]));
        assert_eq!(merged, ColumnReads::All);
    }
}
