//! Additional T-0033 acceptance falsifiers against the generated declarations.

use std::collections::BTreeMap;
use std::fs;
use std::process::Command;

use dom_render_compiler::forge::declare::CollectionDecl;
use dom_render_compiler::forge::emit_forge_dts;

fn declarations() -> String {
    let decls: BTreeMap<String, CollectionDecl> = serde_json::from_str(
        r#"{
          "messages": {
            "fields": { "room": "text", "body": "text", "amount": "real", "done": "bool" },
            "partition_by": "room"
          },
          "unpartitioned": { "fields": { "body": "text" } }
        }"#,
    )
    .expect("collection declarations");
    emit_forge_dts(&decls)
}

fn check(source: &str, imported: &str) {
    let Ok(tsc) = std::env::var("ALBEDO_TSC") else {
        eprintln!("tsc not available: set ALBEDO_TSC to prove these type assertions");
        return;
    };
    let dir = tempfile::tempdir().expect("type fixture");
    fs::write(
        dir.path().join("tsconfig.json"),
        r#"{"compilerOptions":{"target":"ES2020","module":"ESNext","moduleResolution":"bundler","strict":true,"noEmit":true,"skipLibCheck":false,"types":[]},"include":["*.ts"]}"#,
    )
    .expect("tsconfig");
    fs::write(
        dir.path().join("albedo-env.d.ts"),
        include_str!("../scaffold/src/albedo-env.d.ts"),
    )
    .expect("real scaffold declarations");
    fs::write(dir.path().join("forge.d.ts"), declarations()).expect("generated declarations");
    fs::write(dir.path().join("predicate.ts"), imported).expect("imported predicates");
    fs::write(dir.path().join("check.ts"), source).expect("type assertions");
    let output = Command::new("node")
        .arg(tsc)
        .arg("--noEmit")
        .arg("-p")
        .arg(dir.path().join("tsconfig.json"))
        .output()
        .expect("run local TypeScript");
    assert!(
        output.status.success(),
        "tsc rejected the acceptance fixture ({}):\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn imported_and_nonfresh_predicates_cannot_widen_the_legacy_partition_overload() {
    check(
        r#"import { useSharedSlot } from "albedo";
import { messages, unpartitioned } from "albedo/forge";
import { exact, wider } from "./predicate";
useSharedSlot(messages.where(exact));
const extra = { room: "r", body: "secret" };
// @ts-expect-error a non-fresh wider predicate is a new query, not a partition
useSharedSlot(messages.where(extra));
// @ts-expect-error the same restriction applies across a module import
useSharedSlot(messages.where(wider));
// @ts-expect-error an unpartitioned collection has no legacy where overload
useSharedSlot(unpartitioned.where({ body: "b" }));
const optionalKey: { room: string; body?: string } = { room: "r" };
// @ts-expect-error a possibly present extra key is still outside the partition contract
useSharedSlot(messages.where(optionalKey));
"#,
        "export const exact = { room: 'r' }; export const wider = { room: 'r', body: 'secret' };",
    );
}

#[test]
fn a_union_with_a_wide_branch_is_not_an_exact_legacy_partition_key() {
    check(
        r#"import { useSharedSlot } from "albedo";
import { messages } from "albedo/forge";
declare const key: { room: string } | { room: string; body: string };
// @ts-expect-error one branch can carry a non-partition predicate
useSharedSlot(messages.where(key));
"#,
        "",
    );
}

#[test]
fn row_results_aggregates_and_terminal_chain_states_remain_typed() {
    check(
        r#"import { messages } from "albedo/forge";
import type { MessagesRow } from "albedo/forge";
import { optional, or, count, sum, avg } from "albedo/query";
type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends (<T>() => T extends B ? 1 : 2) ? true : false;
function exact<T extends true>(): void {}
export async function run() {
  const rows = await messages.where({ amount: { in: [1, "2"] }, done: optional(true) });
  exact<Equal<typeof rows, MessagesRow[]>>();
  const groups = await messages.groupBy("room", { n: count(), total: sum("amount"), mean: avg("amount") });
  exact<Equal<(typeof groups)[number]["room"], string>>();
  exact<Equal<(typeof groups)[number]["n"], number>>();
  exact<Equal<(typeof groups)[number]["total"], number | null>>();
  exact<Equal<(typeof groups)[number]["mean"], number | null>>();
  await messages.where(or({ room: "a" }, { body: "b" }));
  // @ts-expect-error predicates cannot follow a sort
  messages.orderBy("room").where({ room: "a" });
  // @ts-expect-error a terminal limit is no longer a query builder
  messages.limit(1).count();
  // @ts-expect-error sorting directions are a closed vocabulary
  messages.orderBy("room", "descending");
  // @ts-expect-error a text field does not accept a number
  messages.where({ body: 42 });
  // @ts-expect-error a boolean field does not accept a number
  messages.where({ done: 1 });
}
"#,
        "",
    );
}
