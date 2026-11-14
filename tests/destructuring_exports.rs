//! Exported binding patterns must agree in project and npm module records.

use dom_render_compiler::bundler::npm::{bundle_npm_dependency, ShakeOptions};
use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;

fn check_exports(npm: bool, source: &str, expression: &str, expected: &str) {
    let mut engine = QuickJsEngine::new();
    engine
        .init(&BootstrapPayload::default())
        .expect("engine init");
    let fixture = tempfile::tempdir().expect("fixture");
    let specifier = if npm {
        let pkg = fixture.path().join("node_modules/bindings");
        std::fs::create_dir_all(&pkg).expect("package directory");
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"bindings","version":"1.0.0","type":"module","main":"index.js"}"#,
        )
        .expect("package manifest");
        std::fs::write(pkg.join("index.js"), source).expect("package source");
        let bundle = bundle_npm_dependency(fixture.path(), "bindings", &ShakeOptions::default())
            .expect("bundle binding patterns");
        for artifact in bundle.artifacts {
            engine
                .load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)
                .expect("load artifact");
        }
        "bindings"
    } else {
        engine
            .load_module("bindings.js", source)
            .expect("load local binding patterns");
        "./bindings.js"
    };
    // Consume the record from another module: reading the declaration's own
    // locals would miss absent exports and incorrectly collected property names.
    let consumer = format!(
        "import * as ns from '{specifier}'; export default function App() {{ \
         return Object.keys(ns).join(',') + ':' + ({expression}); }}"
    );
    engine
        .load_module("consumer.jsx", &consumer)
        .expect("load consumer");
    let rendered = engine
        .render_component("consumer.jsx", "{}")
        .expect("render exports");
    assert_eq!(rendered.html, expected);
}

macro_rules! binding_case {
    ($local:ident, $npm:ident, $source:expr, $expression:expr, $expected:expr) => {
        #[test]
        fn $local() {
            check_exports(false, $source, $expression, $expected);
        }
        #[test]
        fn $npm() {
            check_exports(true, $source, $expression, $expected);
        }
    };
}

binding_case!(
    local_renamed_object,
    npm_renamed_object,
    "const o = { a: 3, b: 4 }; export const { a, b: c } = o;",
    "ns.a + ',' + ns.c",
    "a,c:3,4"
);
binding_case!(
    local_object_default_and_rest,
    npm_object_default_and_rest,
    "const fallback = 1; const o = { extra: 8 }; export const { d = fallback, ...rest } = o;",
    "ns.d + ',' + Object.keys(ns.rest).join(',') + ',' + ns.rest.extra",
    "d,rest:1,extra,8"
);
binding_case!(
    local_array_holes_defaults_rest,
    npm_array_holes_defaults_rest,
    "const arr = [5, 99, undefined, 7, 8]; export let [x, , y = 2, ...zs] = arr;",
    "ns.x + ',' + ns.y + ',' + ns.zs.join(',')",
    "x,y,zs:5,2,7,8"
);
binding_case!(
    local_nested_object,
    npm_nested_object,
    "const o = { n: { deep: 9 } }; export const { n: { deep } } = o;",
    "ns.deep",
    "deep:9"
);
binding_case!(local_nested_default_and_declarators, npm_nested_default_and_declarators,
    "const key = 'value'; export const { [key]: [first, { leaf } = { leaf: 6 }] } = { value: [4] }, plain = 7;",
    "ns.first + ',' + ns.leaf + ',' + ns.plain", "first,leaf,plain:4,6,7");

/// Prepare with `albedo init target/t0037-openai-probe`, then install openai
/// there. This uses the real CLI action door and never contacts OpenAI.
#[test]
#[ignore = "requires the debug albedo binary and npm-installed openai fixture"]
fn openai_client_action_against_local_mock() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let app = root.join("target/t0037-openai-probe");
    assert!(
        app.join("node_modules/openai/package.json").is_file(),
        "install openai in the probe app first"
    );
    // Surface module-initialization errors directly; the HTTP renderer may
    // otherwise report only a Tier-B render-error placeholder.
    let bundle = bundle_npm_dependency(
        &app,
        "openai",
        &dom_render_compiler::bundler::client_npm::server_shake_options(),
    )
    .expect("bundle SDK");
    let mut engine = QuickJsEngine::new();
    engine
        .init(&BootstrapPayload::default())
        .expect("engine init");
    for artifact in bundle.artifacts {
        engine
            .load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)
            .expect("load SDK artifact");
    }
    engine
        .load_module(
            "sdk_check.jsx",
            "import OpenAI from 'openai'; export default function App() { return typeof OpenAI; }",
        )
        .expect("initialize SDK import");
    engine
        .render_component("sdk_check.jsx", "{}")
        .expect("render SDK import");
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock listener");
    let address = listener.local_addr().expect("mock address");
    listener.set_nonblocking(true).expect("nonblocking mock");
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let mock = std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                        .expect("read timeout");
                    let mut request = Vec::new();
                    let mut chunk = [0; 4096];
                    loop {
                        let count = stream.read(&mut chunk).expect("read mock request");
                        assert_ne!(count, 0, "request ended early");
                        request.extend_from_slice(&chunk[..count]);
                        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                        {
                            let headers = String::from_utf8_lossy(&request[..end]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length").then(|| {
                                        value.trim().parse::<usize>().expect("body length")
                                    })
                                })
                                .expect("content-length");
                            if request.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    let body = r#"{"id":"chatcmpl-test","object":"chat.completion","created":1,"model":"mock","choices":[{"index":0,"message":{"role":"assistant","content":"T0037 mock response"},"finish_reason":"stop"}]}"#;
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("mock response");
                    return Some(String::from_utf8(request).expect("UTF-8 request"));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("mock accept: {error}"),
            }
        }
        None
    });
    let route = format!(
        r#"
        import OpenAI from "openai";
        import {{ action, useSharedSlot }} from "albedo";
        export const ask_openai = action(async () => {{
            const client = new OpenAI({{ baseURL: "http://{address}/v1", apiKey: "test" }});
            const result = await client.chat.completions.create({{ model: "mock", messages: [{{ role: "user", content: "hello" }}] }});
            append("guestbook", {{ author: "mock", message: result.choices[0].message.content }});
        }});
        export default function Probe() {{
            const entries = useSharedSlot("guestbook");
            return <section><form action="action:ask_openai" method="POST"><button type="submit">Ask</button></form>
                {{entries.map((entry) => <p key={{entry.id}}>{{entry.message}}</p>)}}</section>;
        }}
    "#
    );
    std::fs::write(app.join("src/routes/openai.tsx"), route).expect("probe route");
    std::fs::write(
        app.join("src/routes/openai.test.tsx"),
        r#"
        import { test, expect, visit } from "albedo/test";
        test("T0037 SDK action returns mock response", () => {
            const page = visit("/openai");
            expect(page.status).toBe(200);
            if (page.findAll("form").length === 0) { throw new Error(page.html); }
            const after = page.submit("ask_openai", {});
            expect(after.status).toBe(200);
            expect(after).toContain("T0037 mock response");
        });
    "#,
    )
    .expect("probe assertion");
    let output = std::process::Command::new(root.join(format!(
        "target/debug/albedo{}",
        std::env::consts::EXE_SUFFIX
    )))
    .arg("test")
    .arg(&app)
    .args(["--filter", "T0037", "--timeout", "20"])
    .output();
    done.store(true, Ordering::Release);
    let request = mock.join().expect("mock thread");
    let output = output.expect("run CLI probe");
    assert!(
        output.status.success(),
        "SDK action probe failed ({}):\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let request = request.expect("SDK reached the mock");
    assert!(
        request.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer test\r\n"),
        "{request}"
    );
    assert!(
        request.contains("\"model\":\"mock\"") || request.contains("\"model\": \"mock\""),
        "{request}"
    );
}
