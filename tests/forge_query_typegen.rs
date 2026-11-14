//! T-0033 · Q1a — Acceptance 11 / Design 10 / R3-2: the generated declarations,
//! checked by `tsc` against the **real** scaffold `albedo-env.d.ts`.
//!
//! Accepted shapes carry exact type assertions; refused shapes sit under
//! `// @ts-expect-error`, so a protection that silently stops working fails the
//! run as an unused directive. Two mutants prove the run can fail: `__row` put
//! back on `Query`, and the legacy partition overload widened to any key.
//!
//! `tsc` is not vendored. Set `ALBEDO_TSC` to a TypeScript `bin/tsc` script
//! (run with `node`); without it these tests print "tsc not available" and
//! check only that emission is byte-deterministic — they do not claim the type
//! errors were proven.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use dom_render_compiler::forge::declare::CollectionDecl;
use dom_render_compiler::forge::emit_forge_dts;

const SCAFFOLD_ENV: &str = include_str!("../scaffold/src/albedo-env.d.ts");

fn decls() -> BTreeMap<String, CollectionDecl> {
    let mut out = BTreeMap::new();
    for (name, json) in [
        ("guestbook", r#"{ "fields": { "author": "text", "message": "text" } }"#),
        ("messages", r#"{ "fields": { "room": "text", "body": "text" }, "partition_by": "room" }"#),
        (
            "invoices",
            r#"{ "fields": { "amount": "real", "due": "timestamp", "owner": "text", "status": "text" } }"#,
        ),
    ] {
        out.insert(name.to_string(), serde_json::from_str(json).expect("decl"));
    }
    out
}

const CHECK: &str = r#"import { useSharedSlot } from "albedo";
import { guestbook, messages, invoices } from "albedo/forge";
import type { GuestbookRow, MessagesRow, InvoicesRow } from "albedo/forge";
import { or, optional, count, sum } from "albedo/query";

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends (<T>() => T extends B ? 1 : 2) ? true : false;
function expectType<T extends true>(): void {}
declare const params: { id: string };
declare const filter: { owner?: string };

export async function accepted() {
  const rows = await guestbook.where({ author: "a" });
  expectType<Equal<typeof rows, GuestbookRow[]>>();
  const one = await invoices.get(params.id);
  expectType<Equal<typeof one, InvoicesRow | null>>();
  const n = await invoices.where({ status: "open" }).count();
  expectType<Equal<typeof n, number>>();
  const first = await invoices.where({ owner: "me" }).orderBy("due", "desc").first();
  expectType<Equal<typeof first, InvoicesRow | null>>();
  const page = await invoices.where({ status: "open", due: { lt: 5 } }).orderBy("due", "desc").limit(50);
  expectType<Equal<typeof page, InvoicesRow[]>>();
  const groups = await invoices.groupBy("status", { n: count(), total: sum("amount") });
  expectType<Equal<(typeof groups)[number]["n"], number>>();
  expectType<Equal<(typeof groups)[number]["status"], string>>();
  await invoices.where(or({ status: "open" }, { owner: "me" }));
  await invoices.where({ owner: optional(filter.owner) });
  await invoices.where({ id: params.id, due: { gte: "17" } });
  await invoices.where({ owner: null, status: { not: null }, amount: { in: [1, 2] } });

  // The legacy forms still typecheck, rows typed from the config.
  const live = useSharedSlot(guestbook);
  expectType<Equal<typeof live, GuestbookRow[]>>();
  const room = useSharedSlot(messages.where({ room: params.id }));
  expectType<Equal<typeof room, MessagesRow[]>>();
  const topic = useSharedSlot<number>("counter");
  expectType<Equal<typeof topic, number>>();
}

export function refused() {
  // @ts-expect-error a new-style query is not a useSharedSlot source (live queries: Q3)
  useSharedSlot(guestbook.where({ author: "a" }));
  // @ts-expect-error nor is a bounded one
  useSharedSlot(invoices.where({ status: "open" }).limit(10));
  const pred = { room: params.id, body: "a" };
  // @ts-expect-error R3-2: a key wider than the partition column is not the legacy read
  useSharedSlot(messages.where(pred));
  // @ts-expect-error an unknown column
  invoices.where({ nope: 1 });
  // @ts-expect-error orderBy names a real column
  invoices.orderBy("nope");
  // @ts-expect-error a collection can't be awaited as a value
  const whole: InvoicesRow[] = invoices;
  return whole;
}
"#;

const TSCONFIG: &str = r#"{
  "compilerOptions": {
    "target": "ES2020",
    "module": "ESNext",
    "moduleResolution": "bundler",
    "lib": ["ES2020", "DOM"],
    "strict": true,
    "noEmit": true,
    "skipLibCheck": false,
    "types": []
  },
  "include": ["*.ts"]
}
"#;

/// `tsc -p` over a fixture with `forge_dts` → `Some((passed, output))`, or
/// `None` when no `tsc` is configured.
fn tsc(forge_dts: &str) -> Option<(bool, String)> {
    let tsc = std::env::var("ALBEDO_TSC").ok()?;
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "tsconfig.json", TSCONFIG);
    write(dir.path(), "albedo-env.d.ts", SCAFFOLD_ENV);
    write(dir.path(), "forge.d.ts", forge_dts);
    write(dir.path(), "check.ts", CHECK);
    let out = Command::new("node")
        .arg(&tsc)
        .arg("-p")
        .arg(dir.path().join("tsconfig.json"))
        .output()
        .expect("run node tsc");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Some((out.status.success(), text))
}

fn write(dir: &Path, name: &str, body: &str) {
    fs::write(dir.join(name), body).expect("write fixture");
}

fn tsc_version() -> String {
    let Ok(tsc) = std::env::var("ALBEDO_TSC") else {
        return "tsc not available (ALBEDO_TSC unset)".into();
    };
    let out = Command::new("node").arg(&tsc).arg("--version").output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|e| format!("tsc failed to start: {e}"))
}

#[test]
fn the_generated_declarations_type_the_chain_and_refuse_what_q1a_refuses() {
    let dts = emit_forge_dts(&decls());
    assert_eq!(dts, emit_forge_dts(&decls()), "byte-deterministic");
    // For a reviewer: the fixture's declarations and check file, to run by hand.
    if let Ok(dir) = std::env::var("ALBEDO_TYPEGEN_DUMP") {
        let dir = Path::new(&dir);
        fs::create_dir_all(dir).expect("dump dir");
        write(dir, "tsconfig.json", TSCONFIG);
        write(dir, "albedo-env.d.ts", SCAFFOLD_ENV);
        write(dir, "forge.d.ts", &dts);
        write(dir, "check.ts", CHECK);
    }
    eprintln!("{}", tsc_version());
    let Some((passed, output)) = tsc(&dts) else {
        eprintln!("tsc not available: the type errors are NOT proven by this run");
        return;
    };
    assert!(passed, "tsc rejected the fixture:\n{output}");
}

#[test]
fn mutant_a_query_with_a_row_phantom_becomes_a_shared_slot_source_and_tsc_says_so() {
    let dts = emit_forge_dts(&decls());
    let anchor = "export interface Query<Row> extends PromiseLike<Row[]> {\n";
    assert!(dts.contains(anchor));
    let mutant = dts.replace(anchor, &format!("{anchor}    readonly __row?: Row;\n"));
    let Some((passed, output)) = tsc(&mutant) else {
        eprintln!("tsc not available");
        return;
    };
    assert!(!passed, "the mutant must fail:\n{output}");
    assert!(output.contains("Unused '@ts-expect-error'"), "{output}");
}

#[test]
fn mutant_a_legacy_overload_taking_any_key_lets_a_wide_predicate_through_and_tsc_says_so() {
    let dts = emit_forge_dts(&decls());
    let exact = "key: K & Record<Exclude<AllKeys<K>, PartitionCol>, never>,";
    assert!(dts.contains(exact));
    let mutant = dts.replace(exact, "key: K,");
    let Some((passed, output)) = tsc(&mutant) else {
        eprintln!("tsc not available");
        return;
    };
    assert!(!passed, "the mutant must fail:\n{output}");
    assert!(output.contains("Unused '@ts-expect-error'"), "{output}");
}
