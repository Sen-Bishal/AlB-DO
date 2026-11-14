use std::net::TcpListener;
use std::process::Command;

fn driver() -> Command {
    Command::new(env!("CARGO_BIN_EXE_albedo-http-bench"))
}

#[test]
fn occupied_port_refuses_before_invoking_albedo() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let out = driver()
        .args(["scaffold", "--port", &port, "--albedo", "does-not-exist"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "occupied port was accepted");
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(error.contains(&port) && error.contains("bound"), "{error}");
    assert!(
        !error.contains("does-not-exist"),
        "albedo ran before port check"
    );
}

#[test]
fn debug_path_is_refused_before_version_or_init() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
        .to_string();
    let out = driver()
        .args([
            "scaffold",
            "--albedo",
            env!("CARGO_BIN_EXE_albedo-http-bench"),
            "--port",
            &port,
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("refusing debug/unverified build"));
    assert!(TcpListener::bind(format!("127.0.0.1:{port}")).is_ok());
}

#[test]
#[ignore = "requires freshly built release albedo; set ALBEDO_TEST_BINARY"]
fn untrusted_proxy_reports_429_and_fails() {
    let binary = std::env::var("ALBEDO_TEST_BINARY").expect("set ALBEDO_TEST_BINARY");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
        .to_string();
    let out = driver()
        .args([
            "scaffold",
            "--albedo",
            &binary,
            "--port",
            &port,
            "--no-proxy-trust",
            "--conns",
            "8",
            "--seconds",
            "1",
            "--rounds",
            "1",
            "--warmup-seconds",
            "0",
        ])
        // The flag must remove even an inherited trust setting.
        .env("ALBEDO_TRUSTED_PROXIES", "127.0.0.1")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "rate-limit refusals passed as throughput"
    );
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(output.contains("429="), "missing status counts: {output}");
    assert!(
        TcpListener::bind(format!("127.0.0.1:{port}")).is_ok(),
        "owned child survived"
    );
}
