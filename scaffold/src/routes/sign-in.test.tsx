import { test, expect, visit, signUp, signIn } from "albedo/test";

// Accounts go through the app's own sign-up and sign-in endpoints — the ones
// the forms on /sign-in post to. `visit(path, { as: user })` asks for a page
// with that user's session; without `as`, a test is an anonymous visitor.

test("a new account is signed in", () => {
  const ada = signUp("ada@example.com", "correct horse battery");

  expect(visit("/sign-in", { as: ada })).toContain("Signed in");
  expect(visit("/sign-in")).not.toContain("Signed in");
});

// A refusal throws with the server's own sentence.
test("a wrong password is refused", () => {
  signUp("grace@example.com", "correct horse battery");

  expect(() => signIn("grace@example.com", "not the password")).toThrow(
    "do not match an account",
  );
});
