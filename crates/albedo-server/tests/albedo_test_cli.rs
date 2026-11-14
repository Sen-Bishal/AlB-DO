//! 15.2 · `albedo test`, run the way an author runs it: `albedo init`, write a
//! test, `albedo test`. Every assertion goes through the real binary.
//!
//! Each test names the mutation of the runner it was watched failing against.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn albedo(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd);
    cmd.env("USERPROFILE", home);
    cmd.env("HOME", home);
    cmd.env("NO_COLOR", "1");
    cmd.env("ALBEDO_NO_TUI", "1");
    cmd
}

fn run(cmd: &mut Command, what: &str) -> (bool, String) {
    let out: Output = cmd.output().unwrap_or_else(|err| panic!("{what}: {err}"));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// `albedo init <name>` in a fresh directory; the app's path.
fn init(home: &Path, work: &Path, name: &str) -> PathBuf {
    let (ok, text) = run(albedo(home, work).args(["init", name]), "albedo init");
    assert!(ok, "albedo init failed\n{text}");
    work.join(name)
}

fn write(app: &Path, relative: &str, contents: &str) {
    let path = app.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// The scaffold's own tests pass — and pass again on a second run.
///
/// The second run is the isolation check: `guestbook.test.tsx` asserts the
/// guestbook starts empty and then adds a row, so a run that kept its rows
/// anywhere the next run reads would fail round two. Watched failing (round
/// two) with the runner pointed at the app's own `forge.db`.
#[test]
fn a_new_app_passes_its_own_tests_and_a_run_leaves_nothing_behind() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = init(home.path(), work.path(), "app");

    for round in 1..=2 {
        let (ok, text) = run(albedo(home.path(), &app).arg("test"), "albedo test");
        assert!(ok, "round {round}: albedo test failed\n{text}");
        assert!(text.contains("8 passed"), "round {round}\n{text}");
        assert!(!text.contains("failed"), "round {round}\n{text}");
    }

    // Test files are tests, not app: no route, no component.
    let manifest = std::fs::read_to_string(app.join(".albedo/dist/render-manifest.v2.json")).unwrap();
    assert!(!manifest.contains("guestbook.test"), "a test file was compiled into the app");
}

/// A failing test fails the run, and the output says which and why.
///
/// Watched failing with `toContain` mutated to always pass.
#[test]
fn a_failure_fails_the_run_and_says_why() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = init(home.path(), work.path(), "app");
    write(
        &app,
        "src/routes/verdicts.test.tsx",
        r#"import { test, expect, visit } from "albedo/test";

test("holds", () => {
  expect(1 + 1).toBe(2);
});

test("breaks", () => {
  expect(visit("/guestbook")).toContain("words that are not on the page");
});

test("misnames a field", () => {
  visit("/guestbook").submit("sign_guestbook", { autor: "x" });
});
"#,
    );

    let (ok, text) = run(
        albedo(home.path(), &app).args(["test", "--timeout", "5"]),
        "albedo test",
    );
    assert!(!ok, "a run with failures exited 0\n{text}");
    assert!(text.contains("✓ holds"), "{text}");
    assert!(text.contains("✗ breaks"), "{text}");
    assert!(
        text.contains("to contain \"words that are not on the page\""),
        "the failure did not say why\n{text}"
    );
    assert!(text.contains("has no field \"autor\""), "{text}");
    assert!(text.contains("2 failed"), "{text}");
}

/// R1b (T-0026) · AC 12 — `grantRole` / `revokeRole` from `albedo/test`.
///
/// Falsifier 7: on the pre-T-0026 tree `grantRole` is not exported, so the
/// first role test fails with it undefined.
#[test]
fn a_test_can_grant_and_revoke_a_role() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = init(home.path(), work.path(), "app");
    for scaffold_test in [
        "src/routes/guestbook.test.tsx",
        "src/routes/sign-in.test.tsx",
        "src/components/Hero.test.tsx",
    ] {
        std::fs::remove_file(app.join(scaffold_test)).unwrap();
    }
    let config_path = app.join("albedo.config.ts");
    let config = std::fs::read_to_string(&config_path).unwrap();
    let declared = config.replacen("login: \"/sign-in\",", "login: \"/sign-in\",\n    roles: [\"payroll\"],", 1);
    assert_ne!(declared, config, "the scaffold config changed shape");
    std::fs::write(config_path, declared).unwrap();
    write(
        &app,
        "src/routes/payroll.tsx",
        r#"export const auth = { role: "payroll" };

export default function Payroll() {
  return <main><h1>the payroll room</h1></main>;
}
"#,
    );
    write(
        &app,
        "src/routes/roles.test.tsx",
        r#"import { test, expect, visit, signUp, signIn, signOut, grantRole, revokeRole } from "albedo/test";

test("a role opens the gate and revoking it closes it again", () => {
  const ada = signUp("ada@example.com", "correct horse battery");
  expect(visit("/payroll", { as: ada }).status).toBe(403);
  grantRole(ada, "payroll");
  expect(visit("/payroll", { as: ada })).toContain("the payroll room");
  revokeRole(ada, "payroll");
  expect(visit("/payroll", { as: ada }).status).toBe(403);
});

test("an undeclared role fails, naming it", () => {
  const grace = signUp("grace@example.com", "correct horse battery");
  let message = "";
  try { grantRole(grace, "nope"); } catch (error) { message = String(error); }
  expect(message).toContain("nope");
});

test("a signed-out user cannot be granted a role", () => {
  const sam = signUp("sam@example.com", "correct horse battery");
  signOut(sam);
  let message = "";
  try { grantRole(sam, "payroll"); } catch (error) { message = String(error); }
  expect(message).toContain("signed out");
});

test("the role goes to the session's own user, no one else", () => {
  const first = signUp("twin@example.com", "correct horse battery");
  const second = signUp("twin+b@example.com", "correct horse battery");
  grantRole(second, "payroll");
  expect(visit("/payroll", { as: first }).status).toBe(403);
  expect(visit("/payroll", { as: second }).status).toBe(200);
});

test("a signed-in jar grants and revokes the same principal across sessions", () => {
  const original = signUp("sessions@example.com", "correct horse battery");
  const current = signIn("sessions@example.com", "correct horse battery");
  signOut(original);
  grantRole(current, "payroll");
  grantRole(current, "payroll");
  expect(visit("/payroll", { as: current }).status).toBe(200);
  revokeRole(current, "payroll");
  revokeRole(current, "payroll");
  expect(visit("/payroll", { as: current }).status).toBe(403);
});

test("revoking through a signed-out jar fails without affecting another jar", () => {
  const ended = signUp("ended@example.com", "correct horse battery");
  const active = signUp("active@example.com", "correct horse battery");
  grantRole(active, "payroll");
  signOut(ended);
  let message = "";
  try { revokeRole(ended, "payroll"); } catch (error) { message = String(error); }
  expect(message).toContain("revokeRole");
  expect(message).toContain("signed out");
  expect(visit("/payroll", { as: active }).status).toBe(200);
});

test("both helpers reject a forged user and malformed roles", () => {
  const user = signUp("arguments@example.com", "correct horse battery");
  for (const change of [grantRole, revokeRole]) {
    let message = "";
    try { change({ email: user.email, jar: user.jar }, "payroll"); } catch (error) { message = String(error); }
    expect(message).toContain("user from signUp() or signIn()");
    for (const role of ["", null, 42]) {
      message = "";
      try { change(user, role); } catch (error) { message = String(error); }
      expect(message).toContain("role name");
    }
    message = "";
    try { change(user, "payroll\nnope"); } catch (error) { message = String(error); }
    expect(message).toContain("not a role this app declares");
  }
  expect(visit("/payroll", { as: user }).status).toBe(403);
});
"#,
    );
    let (ok, text) = run(albedo(home.path(), &app).arg("test"), "albedo test");
    assert!(ok, "albedo test failed\n{text}");
    assert!(text.contains("7 passed"), "{text}");
}

/// `src/middleware.ts` and `export const auth = "required"` answer a test's
/// `visit()` exactly as they answer a browser — because the request goes
/// through the router `serve` binds.
///
/// Watched failing with `visit()` not following redirects.
#[test]
fn middleware_and_route_gates_answer_a_test_as_they_answer_a_browser() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = init(home.path(), work.path(), "app");
    // The scaffold's tests visit /guestbook anonymously, which this middleware
    // refuses; this test replaces them.
    for scaffold_test in [
        "src/routes/guestbook.test.tsx",
        "src/routes/sign-in.test.tsx",
        "src/components/Hero.test.tsx",
    ] {
        std::fs::remove_file(app.join(scaffold_test)).unwrap();
    }
    write(
        &app,
        "src/middleware.ts",
        r#"import { next, respond } from "albedo/middleware";

export default function middleware(request, { user }) {
  if (request.path.startsWith("/guestbook") && !user) {
    return respond("members only", { status: 403 });
  }
  return next();
}
"#,
    );
    write(
        &app,
        "src/routes/members.tsx",
        r#"export const auth = "required";

export default function Members() {
  return <main><h1>the members room</h1></main>;
}
"#,
    );
    write(
        &app,
        "src/routes/access.test.tsx",
        r#"import { test, expect, visit, signUp } from "albedo/test";

test("the middleware refuses a stranger", () => {
  expect(visit("/guestbook").status).toBe(403);
});

test("the middleware lets a member through", () => {
  const ada = signUp("ada@example.com", "correct horse battery");
  expect(visit("/guestbook", { as: ada }).status).toBe(200);
});

test("the route gate sends a stranger to sign in", () => {
  const page = visit("/members");
  expect(page.url.startsWith("/sign-in")).toBe(true);
  expect(page.redirects.length).toBeGreaterThan(0);
  expect(page).not.toContain("the members room");
});

test("signed in, the gated page renders", () => {
  const grace = signUp("grace@example.com", "correct horse battery");
  expect(visit("/members", { as: grace })).toContain("the members room");
});
"#,
    );

    let (ok, text) = run(albedo(home.path(), &app).arg("test"), "albedo test");
    assert!(ok, "albedo test failed\n{text}");
    assert!(text.contains("4 passed"), "{text}");
}
