//! Additional accepted optional-value and exact-partition type cases for T-0033.

use std::collections::BTreeMap;
use std::fs;
use std::process::Command;

use dom_render_compiler::forge::declare::CollectionDecl;
use dom_render_compiler::forge::emit_forge_dts;

#[test]
fn optional_absent_values_and_readonly_partition_unions_are_typed() {
    let Ok(tsc) = std::env::var("ALBEDO_TSC") else {
        eprintln!("tsc not available: set ALBEDO_TSC to prove these type assertions");
        return;
    };
    let declarations: BTreeMap<String, CollectionDecl> = serde_json::from_str(
        r#"{"messages":{"fields":{"room":"text","body":"text","amount":"real"},"partition_by":"room"}}"#,
    )
    .expect("collection declarations");
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
    fs::write(dir.path().join("forge.d.ts"), emit_forge_dts(&declarations))
        .expect("generated declarations");
    fs::write(
        dir.path().join("check.ts"),
        r#"import { useSharedSlot } from "albedo";
import { messages } from "albedo/forge";
import { optional } from "albedo/query";
export async function absent() {
  await messages.where({ body: optional(null) });
  await messages.where({ amount: optional(undefined) });
  // @ts-expect-error optional presence does not make a number valid for text
  await messages.where({ body: optional(42) });
  // @ts-expect-error optional presence does not make a boolean valid for a number
  await messages.where({ amount: optional(true) });
}
declare const exact: Readonly<{room: string}> | Readonly<{room: number}>;
useSharedSlot(messages.where(exact));
declare const extra: Readonly<{room: string}> | Readonly<{room: string; body?: string}>;
// @ts-expect-error an optional key in a readonly union member is still wider than the partition
useSharedSlot(messages.where(extra));
declare const indexed: {room: string; [column: string]: string};
// @ts-expect-error an index signature can carry non-partition predicates
useSharedSlot(messages.where(indexed));
"#,
    )
    .expect("type assertions");
    let output = Command::new("node")
        .arg(tsc)
        .arg("--noEmit")
        .arg("-p")
        .arg(dir.path().join("tsconfig.json"))
        .output()
        .expect("run local TypeScript");
    assert!(
        output.status.success(),
        "tsc rejected valid optional predicates or accepted a wider partition:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
