mod load;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Options {
    scenario: String,
    binary: PathBuf,
    port: u16,
    rows: usize,
    conns: Vec<usize>,
    seconds: Duration,
    rounds: usize,
    warmup: Duration,
    routes: Vec<String>,
    pair: Option<(String, String)>,
    proxy_trust: bool,
    allow_debug: bool,
}

fn positive<T: std::str::FromStr + PartialOrd + Default>(s: &str) -> Result<T> {
    let n = s
        .parse::<T>()
        .map_err(|_| format!("invalid positive number: {s}"))?;
    if n <= T::default() {
        return Err(format!("expected positive number: {s}").into());
    }
    Ok(n)
}

fn seconds(s: &str, zero: bool) -> Result<Duration> {
    let n: f64 = s.parse()?;
    if !n.is_finite() || n < 0.0 || (!zero && n == 0.0) || n > 86400.0 {
        return Err(
            "seconds must be finite and between 0 and 86400 (measurement must be >0)".into(),
        );
    }
    Ok(Duration::from_secs_f64(n))
}

fn paths(value: &str) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for path in value.split(',') {
        if !path.starts_with('/')
            || path
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(format!("invalid route: {path:?}").into());
        }
        if paths.iter().any(|p| p == path) {
            return Err("duplicate route".into());
        }
        paths.push(path.to_owned());
    }
    Ok(paths)
}

fn options() -> Result<Option<Options>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "albedo-http-bench <scaffold|live-vs-control> [--albedo PATH] [--rows N]\n\
            [--port 3911] [--conns 1,50] [--seconds 5] [--rounds 3] [--warmup-seconds 2]\n\
            [--routes /,/guestbook,/_albedo/health] [--pair /guestbook,/gbnodata]\n\
            [--no-proxy-trust] [--allow-debug]"
        );
        return Ok(None);
    }
    let scenario = args
        .first()
        .ok_or("name a scenario: scaffold or live-vs-control")?
        .clone();
    if scenario != "scaffold" && scenario != "live-vs-control" {
        return Err("unknown scenario".into());
    }
    let live = scenario == "live-vs-control";
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut o = Options {
        scenario,
        binary: root.join("target/release").join(if cfg!(windows) {
            "albedo.exe"
        } else {
            "albedo"
        }),
        port: 3911,
        rows: 0,
        conns: vec![1, 50],
        seconds: Duration::from_secs(5),
        rounds: 3,
        warmup: Duration::from_secs(2),
        routes: paths(if live {
            "/,/guestbook,/gbnodata,/_albedo/health"
        } else {
            "/,/guestbook,/_albedo/health"
        })?,
        pair: live.then(|| ("/guestbook".into(), "/gbnodata".into())),
        proxy_trust: true,
        allow_debug: false,
    };
    let mut i = 1;
    while i < args.len() {
        let flag = &args[i];
        if flag == "--no-proxy-trust" {
            o.proxy_trust = false;
            i += 1;
            continue;
        }
        if flag == "--allow-debug" {
            o.allow_debug = true;
            i += 1;
            continue;
        }
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--albedo" => o.binary = value.into(),
            "--port" => o.port = positive(value)?,
            "--rows" if live => o.rows = value.parse()?,
            "--conns" => o.conns = value.split(',').map(positive).collect::<Result<_>>()?,
            "--seconds" => o.seconds = seconds(value, false)?,
            "--rounds" => o.rounds = positive(value)?,
            "--warmup-seconds" => o.warmup = seconds(value, true)?,
            "--routes" => o.routes = paths(value)?,
            "--pair" => {
                let pair = paths(value)?;
                if pair.len() != 2 {
                    return Err("--pair needs two distinct routes".into());
                }
                o.pair = Some((pair[0].clone(), pair[1].clone()));
            }
            _ => return Err(format!("unknown option: {flag}").into()),
        }
        i += 2;
    }
    if o.conns.iter().any(|n| *n > 4096) {
        return Err("maximum concurrency is 4096".into());
    }
    let mut unique = o.conns.clone();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() != o.conns.len() {
        return Err("duplicate concurrency".into());
    }
    if let Some((a, b)) = &o.pair {
        if !o.routes.contains(a) || !o.routes.contains(b) {
            return Err("both --pair routes must be measured".into());
        }
    }
    Ok(Some(o))
}

struct TempApp(PathBuf);
impl TempApp {
    fn new() -> Result<Self> {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!("albedo-http-{}-{stamp}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}
impl Drop for TempApp {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("temp cleanup {}: {error}", self.0.display());
        }
    }
}

// Every spawned process, including init/import/version, is owned and reaped on
// unwinding or cancellation. Never discover or kill processes by image name.
struct OwnedChild(Child);
impl OwnedChild {
    fn stop(&mut self) -> std::io::Result<()> {
        if self.0.try_wait()?.is_none() {
            self.0.kill()?;
        }
        self.0.wait()?;
        Ok(())
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("failed to stop/reap owned child {}: {error}", self.0.id());
        }
    }
}

fn command(o: &Options, cwd: &Path) -> Command {
    let mut cmd = Command::new(&o.binary);
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .env("ALBEDO_FORGE_DB", cwd.join("forge.db"));
    if o.proxy_trust {
        cmd.env("ALBEDO_TRUSTED_PROXIES", "127.0.0.1");
    } else {
        cmd.env_remove("ALBEDO_TRUSTED_PROXIES");
    }
    cmd
}

fn spawn(cmd: &mut Command, log: &Path) -> Result<OwnedChild> {
    let file = File::create(log)?;
    cmd.stdout(file.try_clone()?).stderr(file);
    Ok(OwnedChild(cmd.spawn()?))
}

fn log_tail(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .rev()
        .take(25)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

async fn checked(cmd: &mut Command, log: &Path) -> Result<String> {
    let mut child = spawn(cmd, log)?;
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(status) = child.0.try_wait()? {
            if !status.success() {
                return Err(format!("command failed ({status}):\n{}", log_tail(log)).into());
            }
            return Ok(fs::read_to_string(log)?);
        }
        if Instant::now() >= deadline {
            return Err(format!("command timed out:\n{}", log_tail(log)).into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn control(source: &str, rows: usize) -> Result<(String, String)> {
    let needle = "const entries = useSharedSlot<Entry[]>(\"guestbook\");";
    if source.matches(needle).count() != 1 {
        return Err("scaffold slot statement changed; refusing unequal control".into());
    }
    let records: Vec<_> = (0..rows)
        .map(|i| {
            format!(
                "{{\"id\":{},\"author\":\"u{i}\",\"message\":\"m{i}\"}}",
                i + 1
            )
        })
        .collect();
    let page = source.replace(
        needle,
        &format!("const entries: Entry[] = [{}];", records.join(",")),
    );
    // The copied action would otherwise collide with guestbook's wire id at
    // boot. Rename its declaration and form target together in this fixture.
    let page = page.replace("sign_guestbook", "sign_gbnodata");
    Ok((page, records.join("\n")))
}

async fn ready(child: &mut OwnedChild, addr: &str, log: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Err(format!("server exited ({status}):\n{}", log_tail(log)).into());
        }
        let probe = async {
            use tokio::io::{AsyncWriteExt, BufReader};
            let mut socket = tokio::net::TcpStream::connect(addr).await?;
            socket
                .write_all(
                    format!("GET /_albedo/ready HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes(),
                )
                .await?;
            load::response(&mut BufReader::new(socket)).await
        };
        if matches!(
            tokio::time::timeout(Duration::from_secs(1), probe).await,
            Ok(Ok((200, _, _)))
        ) {
            if child.0.try_wait()?.is_some() {
                return Err("owned server exited during readiness".into());
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("server readiness timed out:\n{}", log_tail(log)).into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn validate_page(body: &str, rows: usize) -> Result<()> {
    if body.contains("data-albedo-error=\"error\"") || body.contains("data-albedo-fallback=") {
        return Err(
            "HTTP 200 contains a render error/fallback; refusing to benchmark an error page".into(),
        );
    }
    let actual = body.matches("<li class=\"row\"").count();
    if !body.contains("class=\"ledger\"") || actual != rows {
        return Err(format!("expected guestbook ledger with {rows} rows, found {actual}").into());
    }
    for i in 0..rows {
        if !body.contains(&format!(">u{i}</span>")) || !body.contains(&format!(">m{i}</span>")) {
            return Err(format!("missing generated row u{i}/m{i}").into());
        }
    }
    Ok(())
}

async fn check_fixture(addr: &str, route: &str, rows: usize) -> Result<()> {
    use tokio::io::{AsyncWriteExt, BufReader};
    let probe = async {
        let mut socket = tokio::net::TcpStream::connect(addr).await?;
        socket
            .write_all(
                format!(
                    "GET {route} HTTP/1.1\r\nHost: {addr}\r\nX-Forwarded-For: 10.2.0.1\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;
        let mut body = Vec::new();
        let (status, _, _) =
            load::response_into(&mut BufReader::new(socket), Some(&mut body)).await?;
        eprintln!(
            "# fixture route={route} status={status} body_bytes={}",
            body.len()
        );
        if status != 200 {
            return Err(format!("fixture {route}: status counts {status}=1").into());
        }
        validate_page(std::str::from_utf8(&body)?, rows)
            .map_err(|e| format!("fixture {route}: {e}").into())
    };
    tokio::time::timeout(Duration::from_secs(30), probe).await?
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

async fn drive(mut o: Options) -> Result<()> {
    // Hold the reservation through setup, so a stale server cannot answer even
    // an init/version command. Recheck immediately before spawning serve.
    let reserve = || {
        TcpListener::bind(("127.0.0.1", o.port))
            .map_err(|e| format!("port {} is already bound or unavailable: {e}", o.port))
    };
    let reservation = reserve()?;
    o.binary =
        fs::canonicalize(&o.binary).map_err(|e| format!("albedo {}: {e}", o.binary.display()))?;
    let release = o
        .binary
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|p| p == "release");
    if !release && !o.allow_debug {
        return Err(
            "refusing debug/unverified build; use a target/release binary or --allow-debug".into(),
        );
    }
    let build = if release {
        "release"
    } else {
        "debug/unverified"
    };
    let temp = TempApp::new()?;
    let app = temp.0.join("app");
    let setup_log = temp.0.join("setup.log");
    let version = checked(command(&o, &temp.0).arg("--version"), &setup_log).await?;
    checked(command(&o, &temp.0).arg("init").arg(&app), &setup_log).await?;
    if o.scenario == "live-vs-control" {
        let source = fs::read_to_string(app.join("src/routes/guestbook.tsx"))?;
        let (page, rows) = control(&source, o.rows)?;
        fs::write(app.join("src/routes/gbnodata.tsx"), page)?;
        if o.rows != 0 {
            let input = app.join("rows.jsonl");
            fs::write(&input, rows)?;
            checked(
                command(&o, &app).arg("import").arg("guestbook").arg(&input),
                &setup_log,
            )
            .await?;
        }
    }
    let git = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")))
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_else(|| "unavailable".into());
    println!(
        "# albedo={} version={} git={} os={}-{} cores={} build={build}",
        o.binary.display(),
        version.trim().replace(['\n', '\r'], " "),
        git,
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()?.get()
    );
    println!("# scenario={} rows={} port={} conns={:?} seconds={} rounds={} warmup_seconds={} proxy_trust={}", o.scenario, o.rows, o.port, o.conns, o.seconds.as_secs_f64(), o.rounds, o.warmup.as_secs_f64(), o.proxy_trust);
    drop(reservation);
    drop(reserve()?);
    let server_log = temp.0.join("serve.log");
    let mut server = spawn(
        command(&o, &app)
            .arg("serve")
            .args(["--host", "127.0.0.1", "--port", &o.port.to_string()]),
        &server_log,
    )?;
    eprintln!("# owned server pid={} port={}", server.0.id(), o.port);
    let addr = format!("127.0.0.1:{}", o.port);
    ready(&mut server, &addr, &server_log).await?;
    if o.scenario == "live-vs-control" {
        // Status-only accounting can otherwise count HTTP-200 error stubs as
        // a fast 100-row render. This check is outside the measured hot path.
        for route in ["/guestbook", "/gbnodata"] {
            if let Err(error) = check_fixture(&addr, route, o.rows).await {
                server.stop()?;
                return Err(format!("{error}\nserver output:\n{}", log_tail(&server_log)).into());
            }
        }
    }
    let counter = Arc::new(AtomicU64::new(0));
    let mut valid = true;
    if !o.warmup.is_zero() {
        for route in &o.routes {
            let stats = load::run(&addr, route, 8, o.warmup, counter.clone()).await;
            eprintln!(
                "# warmup {route} statuses={} errors={} first_error={:?}",
                stats.counts(),
                stats.errors,
                stats.first_error
            );
            valid &= stats.valid();
        }
    }
    println!("build\troute\tconns\tround\treq_s\tok\tnon200\tmean_us\tbody_bytes");
    let mut rates: BTreeMap<(String, usize), Vec<f64>> = BTreeMap::new();
    for &conns in &o.conns {
        for round in 1..=o.rounds {
            for route in &o.routes {
                let stats = load::run(&addr, route, conns, o.seconds, counter.clone()).await;
                let total = stats.total().max(1);
                println!(
                    "{build}\t{route}\t{conns}\t{round}\t{:.2}\t{}\t{}\t{:.2}\t{}",
                    stats.rate(),
                    stats.ok(),
                    stats.non200(),
                    stats.latency_ns as f64 / total as f64 / 1000.0,
                    stats.bytes / total
                );
                eprintln!("# statuses route={route} conns={conns} round={round} {} errors={} first_error={:?}", stats.counts(), stats.errors, stats.first_error);
                valid &= stats.valid();
                rates
                    .entry((route.clone(), conns))
                    .or_default()
                    .push(stats.rate());
            }
        }
    }
    if server.0.try_wait()?.is_some() {
        valid = false;
        eprintln!("server exited:\n{}", log_tail(&server_log));
    }
    server.stop()?;
    drop(server);
    if !valid {
        return Err("invalid benchmark: non-200 responses, transport errors, or no successful responses; summary withheld".into());
    }
    let medians: BTreeMap<_, _> = rates
        .into_iter()
        .map(|(key, mut values)| (key, median(&mut values)))
        .collect();
    println!("# median\troute\tconns\treq_s");
    for ((route, conns), rate) in &medians {
        println!("# median\t{route}\t{conns}\t{rate:.2}");
    }
    if let Some((a, b)) = o.pair {
        for conns in o.conns {
            println!(
                "# ratio\t{a}/{b}\t{conns}\t{:.6}",
                medians[&(a.clone(), conns)] / medians[&(b.clone(), conns)]
            );
        }
    }
    Ok(())
}

fn main() {
    let result = (|| -> Result<()> {
        let Some(o) = options()? else {
            return Ok(());
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?;
        runtime.block_on(async {
            tokio::select! {
                result = drive(o) => result,
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    Err("interrupted; owned child stopped".into())
                }
            }
        })
    })();
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "subprocess fixture used by owned_child_cleanup"]
    fn owned_child_fixture() {
        let port: u16 = std::env::var("ALBEDO_HTTP_FIXTURE_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let _listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    fn owned_child_cleanup_on_normal_panic_and_future_cancellation() {
        for mode in 0..3 {
            let port = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tests::owned_child_fixture", "--ignored"])
                .env("ALBEDO_HTTP_FIXTURE_PORT", port.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut child = OwnedChild(child);
            let deadline = Instant::now() + Duration::from_secs(10);
            while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
                assert!(Instant::now() < deadline, "fixture did not bind");
                assert!(child.0.try_wait().unwrap().is_none(), "fixture exited");
                std::thread::sleep(Duration::from_millis(10));
            }
            match mode {
                0 => child.stop().unwrap(),
                1 => assert!(std::panic::catch_unwind(move || {
                    let _child = child;
                    panic!("exercise unwind cleanup");
                })
                .is_err()),
                _ => {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()
                        .unwrap();
                    runtime.block_on(async move {
                        let future = async move {
                            let _child = child;
                            std::future::pending::<()>().await;
                        };
                        assert!(tokio::time::timeout(Duration::from_millis(1), future)
                            .await
                            .is_err());
                    });
                }
            }
            assert!(
                TcpListener::bind(("127.0.0.1", port)).is_ok(),
                "fixture child survived mode {mode}"
            );
        }
    }

    #[test]
    fn control_changes_only_the_slot_statement_and_matches_import() {
        let source = "before\nconst entries = useSharedSlot<Entry[]>(\"guestbook\");\nafter";
        let (page, rows) = control(source, 2).unwrap();
        assert_eq!(rows.lines().count(), 2);
        assert!(page.starts_with("before\nconst entries: Entry[] = ["));
        assert!(page.ends_with("];\nafter"));
        assert!(page.contains(&rows.replace('\n', ",")));
        assert!(control("changed scaffold", 0).is_err());
        assert!(control(source, 0).unwrap().0.contains("= [];"));
    }
    #[test]
    fn control_action_has_a_distinct_wire_name() {
        let source = include_str!("../../../scaffold/src/routes/guestbook.tsx");
        let (page, _) = control(source, 0).unwrap();
        assert!(!page.contains("sign_guestbook"));
        assert!(page.contains("export const sign_gbnodata = action("));
        assert!(page.contains("action:sign_gbnodata"));
    }
    #[test]
    fn median_uses_middle_or_average_of_middle_values() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 2.0, 3.0]), 2.5);
    }
    #[test]
    fn route_and_duration_validation() {
        for path in ["relative", "/\r\nInjected:yes", "/a,/a", ""] {
            assert!(paths(path).is_err());
        }
        for value in ["NaN", "inf", "-1", "0", "86401"] {
            assert!(seconds(value, false).is_err());
        }
        assert!(seconds("0", true).is_ok());
    }

    #[test]
    fn http_200_error_stubs_and_wrong_row_counts_are_not_valid_fixtures() {
        let error = "<div data-albedo-tier=\"b\" data-albedo-error=\"error\"></div>";
        assert!(validate_page(error, 100).is_err());
        assert!(validate_page("<ul class=\"ledger\"></ul>", 100).is_err());
        assert!(validate_page("<ul class=\"ledger\"></ul>", 0).is_ok());
        assert!(validate_page(
            "<ul class=\"ledger\"><li class=\"row\"><span>u0</span><span>m0</span></li></ul>",
            1
        )
        .is_ok());
    }
}
