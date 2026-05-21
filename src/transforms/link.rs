//! Phase L · `<Link href>` extractor.
//!
//! Surfaces every JSX `<Link>` element in a component body. The
//! renderer treats `<Link>` as a compile-time component: at render
//! time it emits an `<a href="...">` host element with
//! `data-albedo-link` set so the client-side runtime can intercept the
//! click, prevent default browser navigation, and request the new
//! route's shell + Tier-C patches over WebTransport instead.
//!
//! The extractor is a metadata pass mirroring `transforms::events`:
//! it does not rewrite the AST. The renderer interprets `<Link>` by
//! consulting the [`LinkExtract`] for the current component and
//! emitting the corresponding `<a>` tag at the matching position.
//!
//! Phase L Stage 1 supports the canonical attribute shape:
//!
//! ```tsx
//! <Link href="/about">About</Link>
//! ```
//!
//! `href` may be a static string literal (Stage 1) or a JSX expression
//! container holding a string-typed expression (Stage 2 — falls
//! through to the existing expression evaluator at render time).

use swc_ecma_ast::{
    BlockStmtOrExpr, Decl, Expr, JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXElement,
    JSXElementChild, JSXElementName, JSXExpr, Lit, Stmt,
};

/// One `<Link>` element surfaced by [`extract_links_in_function`].
#[derive(Debug, Clone)]
pub struct LinkExtract {
    /// Position among all `<Link>` elements in this component in
    /// source-traversal order. Stable across recompilations of the
    /// same source — used by the renderer to align extracted metadata
    /// with the element it is currently emitting.
    pub link_idx: usize,
    /// Static href when the attribute value is a string literal.
    /// `None` when the value is a JSX expression container; in that
    /// case the renderer evaluates the expression via the Phase J
    /// interpreter at render time.
    pub static_href: Option<String>,
    /// True when the original element used an `href` JSX expression
    /// container (e.g. `href={dynamic}`). The renderer needs this to
    /// decide whether to emit the static value inline or to evaluate
    /// the surrounding expression and stringify the result.
    pub href_is_expression: bool,
}

/// Walks every JSX `<Link>` in the function body and returns the
/// metadata for each occurrence in source-traversal order.
///
/// Non-`Link` elements are recursed into so a `<Link>` nested
/// arbitrarily deep is still surfaced. Returned indices match the
/// position in source order; the renderer uses them to dispatch into
/// the right `LinkExtract` as it walks JSX at render time.
pub fn extract_links_in_function(stmts: &[Stmt]) -> Vec<LinkExtract> {
    let mut sink = Vec::new();
    for stmt in stmts {
        visit_stmt_for_jsx(stmt, &mut sink);
    }
    sink
}

/// Statement-level recursion entry point. Mirrors the event
/// extractor's traversal so the two passes stay equivalent — a future
/// fusion into a single JSX walker can lift the shared visitor
/// without changing observable output.
fn visit_stmt_for_jsx(stmt: &Stmt, sink: &mut Vec<LinkExtract>) {
    match stmt {
        Stmt::Return(ret) => {
            if let Some(arg) = &ret.arg {
                visit_expr_for_jsx(arg, sink);
            }
        }
        Stmt::Expr(es) => visit_expr_for_jsx(&es.expr, sink),
        Stmt::Block(block) => {
            for s in &block.stmts {
                visit_stmt_for_jsx(s, sink);
            }
        }
        Stmt::Decl(Decl::Var(var)) => {
            for d in &var.decls {
                if let Some(init) = &d.init {
                    visit_expr_for_jsx(init, sink);
                }
            }
        }
        _ => {}
    }
}

/// Descend an expression looking for JSX. The set of expression
/// shapes we recurse into matches Phase J's renderer; anything the
/// renderer cannot evaluate is also not a place a `<Link>` can
/// meaningfully live.
fn visit_expr_for_jsx(expr: &Expr, sink: &mut Vec<LinkExtract>) {
    match expr {
        Expr::JSXElement(element) => visit_element(element, sink),
        Expr::JSXFragment(fragment) => {
            for child in &fragment.children {
                visit_child(child, sink);
            }
        }
        Expr::Paren(paren) => visit_expr_for_jsx(&paren.expr, sink),
        Expr::Cond(c) => {
            visit_expr_for_jsx(&c.cons, sink);
            visit_expr_for_jsx(&c.alt, sink);
        }
        Expr::Arrow(arrow) => match &*arrow.body {
            BlockStmtOrExpr::Expr(e) => visit_expr_for_jsx(e, sink),
            BlockStmtOrExpr::BlockStmt(b) => {
                for s in &b.stmts {
                    visit_stmt_for_jsx(s, sink);
                }
            }
        },
        _ => {}
    }
}

/// Visit one JSX element. When the opening tag is `Link`, surface the
/// `href` metadata; recurse into children regardless so a `<Link>`
/// nested inside another element is still found.
fn visit_element(element: &JSXElement, sink: &mut Vec<LinkExtract>) {
    if is_link_tag(&element.opening.name) {
        let (static_href, href_is_expression) = extract_href(&element.opening.attrs);
        let link_idx = sink.len();
        sink.push(LinkExtract {
            link_idx,
            static_href,
            href_is_expression,
        });
    }

    for child in &element.children {
        visit_child(child, sink);
    }
}

/// Match `<Link>` only when the tag is the bare identifier `Link`.
/// Member-expression forms (`Foo.Link`) and namespaced (`ns:Link`)
/// shapes are deliberately ignored — the renderer cannot resolve them
/// generically, and users alias via `import { Link } from 'albedo'`.
fn is_link_tag(name: &JSXElementName) -> bool {
    matches!(name, JSXElementName::Ident(ident) if ident.sym.as_ref() == "Link")
}

/// Pulls `href` out of the attribute list.
///
/// Returns `(static_href, is_expression)`:
///   * Static string literal → `(Some(literal), false)`
///   * Expression container with an inner expression → `(None, true)`
///   * Missing or non-string value → `(None, false)` (renderer falls
///     back to emitting `href=""`)
fn extract_href(attrs: &[JSXAttrOrSpread]) -> (Option<String>, bool) {
    for attr in attrs {
        let JSXAttrOrSpread::JSXAttr(attr) = attr else {
            continue;
        };
        let JSXAttrName::Ident(name_ident) = &attr.name else {
            continue;
        };
        if name_ident.sym.as_ref() != "href" {
            continue;
        }
        return match &attr.value {
            Some(JSXAttrValue::Lit(Lit::Str(s))) => (Some(s.value.to_string()), false),
            Some(JSXAttrValue::JSXExprContainer(container)) => {
                let is_expr = matches!(&container.expr, JSXExpr::Expr(_));
                (None, is_expr)
            }
            _ => (None, false),
        };
    }
    (None, false)
}

/// JSX children walker — symmetric with the event extractor; descends
/// into nested elements, fragments, and expression containers so a
/// `<Link>` rendered via `{cond && <Link .../>}` is still surfaced.
fn visit_child(child: &JSXElementChild, sink: &mut Vec<LinkExtract>) {
    match child {
        JSXElementChild::JSXElement(element) => visit_element(element, sink),
        JSXElementChild::JSXFragment(fragment) => {
            for c in &fragment.children {
                visit_child(c, sink);
            }
        }
        JSXElementChild::JSXExprContainer(container) => {
            if let JSXExpr::Expr(expr) = &container.expr {
                visit_expr_for_jsx(expr, sink);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use swc_common::{FileName, SourceMap};
    use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax};

    /// Parse a snippet that defines a single top-level function
    /// component and return its body statements. The extractors
    /// operate on `&[Stmt]` so the test plumbing matches the production
    /// call site shape.
    fn parse_body(source: &str) -> Vec<Stmt> {
        let cm: Rc<SourceMap> = Rc::new(SourceMap::default());
        let fm = cm.new_source_file(
            FileName::Custom("test.jsx".into()).into(),
            source.to_string(),
        );
        let mut parser = Parser::new(
            Syntax::Es(EsSyntax {
                jsx: true,
                ..Default::default()
            }),
            StringInput::from(&*fm),
            None,
        );
        let module = parser.parse_module().expect("parse module");
        for item in module.body {
            if let swc_ecma_ast::ModuleItem::Stmt(Stmt::Decl(Decl::Fn(fn_decl))) = item {
                if let Some(body) = fn_decl.function.body {
                    return body.stmts;
                }
            }
        }
        Vec::new()
    }

    #[test]
    fn extracts_static_href() {
        let stmts = parse_body(
            r#"
            function App() {
                return <div><Link href="/about">About</Link></div>;
            }
        "#,
        );
        let links = extract_links_in_function(&stmts);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].static_href.as_deref(), Some("/about"));
        assert!(!links[0].href_is_expression);
    }

    #[test]
    fn extracts_expression_href() {
        let stmts = parse_body(
            r#"
            function App() {
                return <Link href={dynamic}>Hello</Link>;
            }
        "#,
        );
        let links = extract_links_in_function(&stmts);
        assert_eq!(links.len(), 1);
        assert!(links[0].static_href.is_none());
        assert!(links[0].href_is_expression);
    }

    #[test]
    fn extracts_multiple_links_in_source_order() {
        let stmts = parse_body(
            r#"
            function Nav() {
                return (
                    <nav>
                        <Link href="/">Home</Link>
                        <Link href="/about">About</Link>
                        <Link href="/contact">Contact</Link>
                    </nav>
                );
            }
        "#,
        );
        let links = extract_links_in_function(&stmts);
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].link_idx, 0);
        assert_eq!(links[0].static_href.as_deref(), Some("/"));
        assert_eq!(links[1].static_href.as_deref(), Some("/about"));
        assert_eq!(links[2].static_href.as_deref(), Some("/contact"));
    }

    #[test]
    fn link_inside_conditional_is_surfaced() {
        let stmts = parse_body(
            r#"
            function App() {
                return <div>{cond ? <Link href="/yes">Y</Link> : <Link href="/no">N</Link>}</div>;
            }
        "#,
        );
        let links = extract_links_in_function(&stmts);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].static_href.as_deref(), Some("/yes"));
        assert_eq!(links[1].static_href.as_deref(), Some("/no"));
    }

    #[test]
    fn non_link_anchor_is_ignored() {
        let stmts = parse_body(
            r#"
            function App() {
                return <a href="/raw">raw</a>;
            }
        "#,
        );
        let links = extract_links_in_function(&stmts);
        assert!(links.is_empty());
    }
}
