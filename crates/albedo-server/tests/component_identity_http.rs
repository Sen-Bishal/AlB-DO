//! T-0044 · Two route modules may export a component with the same function
//! name — `export default function Page()` in every route is the ordinary
//! habit. Each route must render its *own* code with its *own* shared-slot
//! data. Before the fix, the Tier B plan was keyed by the bare name, so one
//! route rendered the other's component, and the topic lookup picked a module
//! at random per process.

use std::fs;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn albedo(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd);
    cmd.env("USERPROFILE", home);
    cmd.env("HOME", home);
    cmd.env("NO_COLOR", "1");
    cmd.env("ALBEDO_NO_TUI", "1");
    cmd
}

const ALPHA: &str = r#"import { useSharedSlot } from "albedo";

interface Entry {
  id: number;
  author: string;
  message: string;
}

export default function Page() {
  const entries = useSharedSlot<Entry[]>("guestbook");
  return (
    <section>
      <p>alpha-own</p>
      <ul>
        {entries.map((entry) => (
          <li key={entry.id}>{entry.author}</li>
        ))}
      </ul>
    </section>
  );
}
"#;

const BETA: &str = r#"export default async function Page() {
  return (
    <section>
      <p>beta-own</p>
    </section>
  );
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn same_named_route_components_render_their_own_code_and_data() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let out = albedo(home.path(), work.path())
        .args(["init", "app"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let app = work.path().join("app");
    fs::write(app.join("src/routes/alpha.tsx"), ALPHA).unwrap();
    fs::write(app.join("src/routes/beta.tsx"), BETA).unwrap();

    let rows = app.join("rows.jsonl");
    fs::write(
        &rows,
        "{\"id\":1,\"author\":\"author-one\",\"message\":\"m1\"}\n{\"id\":2,\"author\":\"author-two\",\"message\":\"m2\"}\n",
    )
    .unwrap();
    let imported = albedo(home.path(), &app)
        .args(["import", "guestbook", rows.to_str().unwrap(), "."])
        .output()
        .unwrap();
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );

    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let stderr = app.join("serve.stderr.log");
    let stdout = app.join("serve.stdout.log");
    let _server = Server {
        child: albedo(home.path(), &app)
            .args(["serve", ".", "--port", &port.to_string()])
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    };
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    while client
        .get(format!("{base}/_albedo/ready"))
        .send()
        .await
        .is_err()
    {
        assert!(
            Instant::now() < deadline,
            "serve never answered: {}",
            fs::read_to_string(&stderr).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let alpha = client.get(format!("{base}/alpha")).send().await.unwrap();
    assert_eq!(alpha.status().as_u16(), 200);
    let alpha = alpha.text().await.unwrap();
    let beta = client.get(format!("{base}/beta")).send().await.unwrap();
    assert_eq!(beta.status().as_u16(), 200);
    let beta = beta.text().await.unwrap();
    let log = fs::read_to_string(&stdout).unwrap_or_default();

    assert!(
        !alpha.contains("data-albedo-error=\"error\"")
            && !beta.contains("data-albedo-error=\"error\""),
        "a route rendered the error stub; server output:\n{log}"
    );
    assert!(
        alpha.contains("alpha-own"),
        "/alpha lost its own code:\n{alpha}"
    );
    assert!(
        !alpha.contains("beta-own"),
        "/alpha rendered beta's component:\n{alpha}"
    );
    assert!(
        alpha.contains("author-one") && alpha.contains("author-two"),
        "/alpha lost its shared-slot rows:\n{alpha}"
    );
    assert!(
        beta.contains("beta-own"),
        "/beta lost its own code:\n{beta}"
    );
    assert!(
        !beta.contains("alpha-own"),
        "/beta rendered alpha's component:\n{beta}"
    );
    assert!(
        !beta.contains("data-albedo-list-slot"),
        "the no-data route borrowed shared-slot metadata: {beta}"
    );

    // Overlap renders so pool reuse cannot swap either code or topic seeds.
    let mut requests = tokio::task::JoinSet::new();
    for i in 0..16 {
        let client = client.clone();
        let base = base.clone();
        requests.spawn(async move {
            let (route, own, other) = if i % 2 == 0 {
                ("alpha", "alpha-own", "beta-own")
            } else {
                ("beta", "beta-own", "alpha-own")
            };
            let response = client.get(format!("{base}/{route}")).send().await.unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let body = response.text().await.unwrap();
            assert!(
                body.contains(own) && !body.contains(other),
                "{route}: {body}"
            );
            assert!(
                !body.contains("data-albedo-error=\"error\""),
                "{route}: {body}"
            );
            if route == "alpha" {
                assert!(
                    body.contains("author-one") && body.contains("author-two"),
                    "{body}"
                );
            } else {
                assert!(!body.contains("data-albedo-list-slot"), "{body}");
            }
        });
    }
    while let Some(result) = requests.join_next().await {
        result.unwrap();
    }

    // A hard stop discards all boot/pool state; the same persisted app must
    // rebuild both identities without inheriting a previous process's choice.
    drop(_server);
    let _restarted_server = Server {
        child: albedo(home.path(), &app)
            .args(["serve", ".", "--port", &port.to_string()])
            .stdout(fs::File::options().append(true).open(&stdout).unwrap())
            .stderr(fs::File::options().append(true).open(&stderr).unwrap())
            .spawn()
            .unwrap(),
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    while client
        .get(format!("{base}/_albedo/ready"))
        .send()
        .await
        .is_err()
    {
        assert!(Instant::now() < deadline, "restart never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for (route, own, other) in [
        ("alpha", "alpha-own", "beta-own"),
        ("beta", "beta-own", "alpha-own"),
    ] {
        let response = client.get(format!("{base}/{route}")).send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let body = response.text().await.unwrap();
        assert!(
            body.contains(own) && !body.contains(other),
            "{route}: {body}"
        );
        assert!(!body.contains("data-albedo-error=\"error\""), "{body}");
        if route == "alpha" {
            assert!(
                body.contains("author-one") && body.contains("author-two"),
                "{body}"
            );
        } else {
            assert!(!body.contains("data-albedo-list-slot"), "{body}");
        }
    }
}
