//! Stamp `data-albedo-id` anchors into JSX so the **QuickJS** renderer produces
//! markup bakabox can address.
//!
//! ## The bug this exists to close
//!
//! Two renderers produce Tier-B markup. The pure-Rust one
//! ([`crate::runtime::eval::core`]) stamps every host element with
//! `data-albedo-id = fnv1a_32("{module_spec}#{counter}")`, pre-order, and emits
//! the `BindEvent` / `SetTextRef` opcodes that reference those ids. The QuickJS
//! one — the request path for every Tier-B component — stamped **nothing**, so
//! the chunk it injected carried no anchors at all. The inline opcode frame then
//! named ids that existed nowhere in the document, `_requireNode` threw, and the
//! whole frame was abandoned: no Tier-B `onClick` ever bound.
//!
//! It survived because the two Tier-B components anyone had shipped were
//! form-driven. A `<form action="action:NAME">` binds by *attribute*, through
//! `link-forms.js`, and never goes near a `BindEvent` — so the one component
//! shape that would have exposed this was the one shape nobody had written.
//!
//! ## Why the ids come out equal on both sides
//!
//! Not by copying a number across — by computing the same function of the same
//! two inputs in the same order:
//!
//! * **the module spec** — baked per module by this pass, in the project-relative form the compiled
//!   project keys on (an absolute path would make the ids machine-dependent and break build
//!   reproducibility).
//! * **a pre-order counter** — and this is the part that falls out for free. JSX compiles to nested
//!   `h(type, props, ...children)` calls, and JS evaluates arguments left to right, so a parent's
//!   props object is constructed *before* any child's `h(…)` call runs. Injecting the counter bump
//!   into the props object therefore numbers elements in exactly the pre-order the Rust renderer
//!   uses when it allocates "BEFORE children render".
//!
//! A bottom-up stamp inside `h()` itself cannot do this: by the time `h` is
//! called its children are already stringified, so the parent would be numbered
//! last. The ordering is the reason this is a JSX pass and not a runtime one.
//!
//! ## What is deliberately skipped
//!
//! * **Component elements** (`<Charger />`). `h` invokes those as functions and the Rust renderer
//!   does not stamp them either — only host (lowercase) tags are anchors.
//! * **An element that already carries `data-albedo-id`.** The Rust renderer honours an explicit id
//!   *and does not advance its counter*, so stamping over one would desynchronise every later
//!   element on the page.
//! * **The Tier-C client-island build.** Islands hydrate in the browser against their own runtime;
//!   `__albedo_stable_id` does not exist there, and a call to it would be a `ReferenceError` on the
//!   first render. Gated by the caller passing `None`.
//! * **`<children />`.** Lowercase, so it *looks* like a host tag, but it is the layout-wrap
//!   intrinsic: both renderers lower it to a sentinel comment and neither allocates an id for it.
//!   Stamping it advanced this side's counter and not the evaluator's, so in any layout with markup
//!   **after** the sentinel — a footer, a closing `</nav>`'s siblings — every subsequent element was
//!   numbered one step ahead on the QuickJS side. The two renders stayed byte-identical up to the
//!   sentinel and disagreed on every id after it, which is the worst version of this bug: it looks
//!   fine in a diff of the opening markup.

use swc_common::DUMMY_SP;
use swc_ecma_ast::{
    CallExpr, Callee, Expr, ExprOrSpread, Ident, JSXAttr, JSXAttrName, JSXAttrOrSpread,
    JSXAttrValue, JSXElement, JSXElementName, JSXExpr, JSXExprContainer, Lit, Module, Str,
};
use swc_ecma_visit::{VisitMut, VisitMutWith};

/// The attribute bakabox seeds its node map from. Must match
/// [`crate::runtime::eval::core::ALBEDO_ID_ATTR`] and
/// `DEFAULT_ANCHOR_ATTRIBUTE` in `assets/albedo-runtime.js`.
const ALBEDO_ID_ATTR: &str = "data-albedo-id";

/// The per-render allocator installed by the QuickJS bootstrap. Takes the
/// module spec, returns the next id and advances the shared counter.
pub const STABLE_ID_FN: &str = "__albedo_stable_id";

/// Stamp every host element in `module` with an id call keyed to `module_spec`.
///
/// `module_spec` must be the **project-relative** specifier the compiled project
/// keys on (`components/Charger.tsx`), not the absolute path the manifest
/// carries — that is the string the pure-Rust renderer hashes, and the ids only
/// agree if both sides hash the same one.
pub fn stamp_stable_ids(module: &mut Module, module_spec: &str) {
    module.visit_mut_with(&mut StableIdStamper {
        module_spec: module_spec.to_string(),
    });
}

struct StableIdStamper {
    module_spec: String,
}

impl VisitMut for StableIdStamper {
    fn visit_mut_jsx_element(&mut self, el: &mut JSXElement) {
        el.visit_mut_children_with(self);

        if !is_host_element(&el.opening.name) {
            return;
        }
        if has_albedo_id(el) {
            return;
        }
        el.opening.attrs.push(stable_id_attr(&self.module_spec));
    }
}

/// The layout-wrap intrinsic. Lowercase like a host tag, but neither renderer
/// serialises it as an element — both lower it to the layout-children sentinel —
/// so neither allocates an id for it, and this pass must not either.
const LAYOUT_CHILDREN_TAG: &str = "children";

/// A host element is a lowercase (or dashed) tag: `div`, `my-widget`. A
/// capitalised or dotted name is a component, which `h` calls rather than
/// serialises.
fn is_host_element(name: &JSXElementName) -> bool {
    match name {
        JSXElementName::Ident(ident) => {
            let sym = ident.sym.as_ref();
            if sym == LAYOUT_CHILDREN_TAG {
                return false;
            }
            sym.starts_with(|c: char| c.is_ascii_lowercase()) || sym.contains('-')
        }
        // `<Foo.Bar />` and `<ns:tag />` are never plain host tags here.
        _ => false,
    }
}

fn has_albedo_id(el: &JSXElement) -> bool {
    el.opening.attrs.iter().any(|attr| {
        matches!(
            attr,
            JSXAttrOrSpread::JSXAttr(JSXAttr { name: JSXAttrName::Ident(ident), .. })
                if ident.sym.as_ref() == ALBEDO_ID_ATTR
        )
    })
}

/// `data-albedo-id={__albedo_stable_id("components/Charger.tsx")}`
fn stable_id_attr(module_spec: &str) -> JSXAttrOrSpread {
    let call = Expr::Call(CallExpr {
        span: DUMMY_SP,
        callee: Callee::Expr(Box::new(Expr::Ident(Ident::new_no_ctxt(
            STABLE_ID_FN.into(),
            DUMMY_SP,
        )))),
        args: vec![ExprOrSpread {
            spread: None,
            expr: Box::new(Expr::Lit(Lit::Str(Str {
                span: DUMMY_SP,
                value: module_spec.into(),
                raw: None,
            }))),
        }],
        type_args: None,
        ctxt: Default::default(),
    });

    JSXAttrOrSpread::JSXAttr(JSXAttr {
        span: DUMMY_SP,
        name: JSXAttrName::Ident(swc_ecma_ast::IdentName {
            span: DUMMY_SP,
            sym: ALBEDO_ID_ATTR.into(),
        }),
        value: Some(JSXAttrValue::JSXExprContainer(JSXExprContainer {
            span: DUMMY_SP,
            expr: JSXExpr::Expr(Box::new(call)),
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use swc_common::sync::Lrc;
    use swc_common::{FileName, SourceMap};
    use swc_ecma_ast::EsVersion;
    use swc_ecma_codegen::{text_writer::JsWriter, Config, Emitter};
    use swc_ecma_parser::{parse_file_as_module, Syntax, TsSyntax};

    fn parse(source: &str) -> (Module, Lrc<SourceMap>) {
        let cm: Lrc<SourceMap> = Default::default();
        let fm = cm.new_source_file(Lrc::new(FileName::Custom("t.tsx".into())), source.into());
        let module = parse_file_as_module(
            &fm,
            Syntax::Typescript(TsSyntax {
                tsx: true,
                ..Default::default()
            }),
            EsVersion::Es2022,
            None,
            &mut Vec::new(),
        )
        .expect("parses");
        (module, cm)
    }

    fn emit(module: &Module, cm: Lrc<SourceMap>) -> String {
        let mut buf = Vec::new();
        {
            let mut emitter = Emitter {
                cfg: Config::default(),
                cm: cm.clone(),
                comments: None,
                wr: JsWriter::new(cm, "\n", &mut buf, None),
            };
            emitter.emit_module(module).expect("emits");
        }
        String::from_utf8(buf).expect("utf8")
    }

    fn stamp(source: &str) -> String {
        let (mut module, cm) = parse(source);
        stamp_stable_ids(&mut module, "components/Charger.tsx");
        emit(&module, cm)
    }

    #[test]
    fn every_host_element_gets_an_id_call_keyed_to_its_module() {
        let out = stamp(r#"const A = () => <div><button>go</button><span>x</span></div>;"#);
        assert_eq!(
            out.matches(r#"__albedo_stable_id("components/Charger.tsx")"#)
                .count(),
            3,
            "div, button and span; got: {out}"
        );
    }

    /// The ordering guarantee the whole approach rests on, asserted on the
    /// emitted source: the parent's attribute is written before the children's,
    /// so argument evaluation numbers them pre-order — the same order the
    /// pure-Rust renderer allocates in.
    #[test]
    fn the_parent_id_call_is_emitted_before_its_children() {
        let out = stamp(r#"const A = () => <div><button>go</button></div>;"#);
        let div = out.find("div").expect("div");
        let button = out.find("button").expect("button");
        let first_call = out.find(STABLE_ID_FN).expect("a call");
        assert!(
            div < first_call && first_call < button,
            "the div's id call must precede the button: {out}"
        );
    }

    #[test]
    fn components_are_not_stamped() {
        let out = stamp(r#"const A = () => <Charger />;"#);
        assert!(!out.contains(STABLE_ID_FN), "got: {out}");
    }

    /// The Rust renderer honours an explicit id and does **not** advance its
    /// counter for it. Stamping a second one here would both duplicate the
    /// attribute and desynchronise every element after it.
    #[test]
    fn an_explicit_id_is_left_alone() {
        let out = stamp(r#"const A = () => <div data-albedo-id="7"><i>x</i></div>;"#);
        assert_eq!(
            out.matches(STABLE_ID_FN).count(),
            1,
            "only the <i> is stamped; got: {out}"
        );
        assert!(out.contains(r#"data-albedo-id="7""#), "got: {out}");
    }

    #[test]
    fn a_fragment_is_transparent_and_its_children_are_still_stamped() {
        let out = stamp(r#"const A = () => <><p>a</p><p>b</p></>;"#);
        assert_eq!(out.matches(STABLE_ID_FN).count(), 2, "got: {out}");
    }
}
