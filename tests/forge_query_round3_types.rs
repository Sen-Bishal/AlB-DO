//! T-0033: generated row nullability and exact partition keys with exotic keys.

use std::collections::BTreeMap;
use std::fs;
use std::process::Command;

use dom_render_compiler::forge::declare::CollectionDecl;
use dom_render_compiler::forge::emit_forge_dts;

#[test]
fn custom_keys_nullable_groups_and_nonstring_partition_keys_remain_typed() {
    let Ok(tsc) = std::env::var("ALBEDO_TSC") else {
        eprintln!("tsc not available: set ALBEDO_TSC to prove these type assertions");
        return;
    };
    let declarations: BTreeMap<String, CollectionDecl> = serde_json::from_str(
        r#"{"events":{"key":"event_id","partition_by":"room","fields":{"room":"text","tag":"text?","amount":"real?"}}}"#,
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
import { events } from "albedo/forge";
import type { EventsRow } from "albedo/forge";
import { count, sum } from "albedo/query";
type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends (<T>() => T extends B ? 1 : 2) ? true : false;
function exact<T extends true>(): void {}
export async function results() {
  const one = await events.get("42");
  exact<Equal<typeof one, EventsRow | null>>();
  const first = await events.orderBy("event_id").first();
  exact<Equal<typeof first, EventsRow | null>>();
  const rows = await events.where({ tag: null }).limit(3);
  exact<Equal<(typeof rows)[number]["tag"], string | null>>();
  exact<Equal<(typeof rows)[number]["event_id"], number>>();
  const groups = await events.groupBy("tag", { n: count(), total: sum("amount") });
  exact<Equal<(typeof groups)[number]["tag"], string | null>>();
  exact<Equal<(typeof groups)[number]["n"], number>>();
  exact<Equal<(typeof groups)[number]["total"], number | null>>();
  // @ts-expect-error a custom key does not invent an id column
  events.orderBy("id");
}
declare const marker: unique symbol;
declare const symbolKey: { room: string; [marker]: string };
// @ts-expect-error the legacy overload must reject symbol keys too
useSharedSlot(events.where(symbolKey));
declare const numericKey: { room: string; 0: string };
// @ts-expect-error the legacy overload must reject numeric keys too
useSharedSlot(events.where(numericKey));
declare const absent: { room?: string };
// @ts-expect-error a possibly absent partition is a query, not a legacy partition
useSharedSlot(events.where(absent));
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
        "tsc rejected row types or accepted an inexact partition:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
