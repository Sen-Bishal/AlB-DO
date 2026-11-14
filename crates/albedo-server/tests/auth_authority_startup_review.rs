//! T-0026: an unreadable authority watermark must prevent actual server readiness.

#![cfg(feature = "forge")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use albedo_server::{boot_production_server, ProductionServerOptions, RuntimeError};
use dom_render_compiler::auth::declare::{AuthDeclaration, ProviderDecl};
use dom_render_compiler::auth::schema;
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate};

#[tokio::test]
async fn corrupt_authority_clock_refuses_real_server_start_before_readiness() {
    for corrupt in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path();
        let source = project.join("src");
        let dist = project.join(".albedo/dist");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(dist.join("_albedo")).unwrap();
        std::fs::write(
            source.join("Home.tsx"),
            "export default function Home() { return <p>home</p>; }\n",
        )
        .unwrap();
        std::fs::write(
            dist.join("render-manifest.v2.json"),
            serde_json::json!({
                "version": 2, "build_id": "authority-startup-review", "routes": {},
                "assets": { "chunks": {}, "css": [], "runtime": "/_albedo/runtime.js" },
                "schema_version": "2.0", "generated_at": "", "components": [],
                "parallel_batches": [], "critical_path": [], "vendor_chunks": [], "wt_streams": []
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(dist.join("_albedo/runtime.js"), "// fixture\n").unwrap();
        let db_path = project.join("authority.db");
        let db = LibSqlSubstrate::open_local(&db_path).await.unwrap();
        let tables = schema::augment(&ForgeSchema::build(Vec::new()).unwrap()).unwrap();
        bootstrap_schema(&db, &tables).await.unwrap();
        if corrupt {
            // Keep the schema valid so boot reaches the watermark read itself.
            db.execute(
                "UPDATE albedo_authority_clock SET seq = 'not-an-integer' WHERE id = 1",
                &[],
            )
            .await
            .unwrap();
        }
        drop(db);
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let options = ProductionServerOptions {
            project_dir: project.to_path_buf(),
            source_root: source,
            dist_dir: dist,
            host: "127.0.0.1".to_string(),
            port,
            dev_mode: false,
            hmr: false,
            forge: Default::default(),
            sources: Default::default(),
            uploads: Default::default(),
            email: None,
            redirects: Vec::new(),
            webhooks: Default::default(),
            auth: AuthDeclaration {
                providers: BTreeMap::from([("password".to_string(), ProviderDecl::default())]),
                ..Default::default()
            },
            tls: Default::default(),
            forge_db: Some(db_path),
        };
        let server = boot_production_server(&options).expect("synchronous wiring accepts fixture");
        let ready = Arc::new(AtomicBool::new(false));
        let ready_signal = ready.clone();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            server.run_until(
                move |_| {
                    ready_signal.store(true, Ordering::SeqCst);
                    let _ = shutdown_tx.send(());
                },
                async move {
                    let _ = shutdown_rx.await;
                },
            ),
        )
        .await
        .expect("startup or shutdown exceeded its bound");
        if corrupt {
            let error = result.expect_err("corrupt authority clock must fail server start");
            assert!(matches!(error, RuntimeError::ServerStartup(_)), "{error}");
            assert!(error.to_string().contains("authority clock"), "{error}");
            assert!(
                !ready.load(Ordering::SeqCst),
                "readiness ran before failed clock read"
            );
            assert!(
                tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_err(),
                "failed server left a listener open"
            );
        } else {
            result.expect("healthy authority clock starts and shuts down normally");
            assert!(
                ready.load(Ordering::SeqCst),
                "healthy control never became ready"
            );
        }
    }
}
