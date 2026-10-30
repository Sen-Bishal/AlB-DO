import { test, expect, render, visit } from "albedo/test";
import Guestbook from "./guestbook";

// A page, tested through the server `albedo serve` runs.
//
// Every test file gets its own server and its own throwaway database, seeded
// from `albedo.config.ts` — your real `forge.db` is never opened. Tests in one
// file share that database in order, the way requests to one server do.

test("the guestbook starts empty", () => {
  const page = visit("/guestbook");

  expect(page.status).toBe(200);
  expect(page.findAll(".ledger .row")).toHaveLength(0);
});

// `submit()` posts the form the way a browser does: its hidden fields (the
// CSRF token among them), the values you give, this visitor's cookies, and the
// redirect followed. A field the form does not have is an error, so a typo
// fails here instead of posting nothing.
test("signing the guestbook adds a row", () => {
  const page = visit("/guestbook");

  const after = page.submit("sign_guestbook", {
    author: "Ada",
    message: "hello from a test",
  });

  expect(after.url).toBe("/guestbook");
  expect(after.findAll(".ledger .row")).toHaveLength(1);
  expect(after).toContain("hello from a test");
});

// The component on its own, with rows handed to it instead of read from the
// database: `shared` seeds what `useSharedSlot("guestbook")` returns.
test("each entry is one row, in order", () => {
  const view = render(<Guestbook />, {
    shared: {
      guestbook: [
        { id: 1, author: "Ada", message: "first" },
        { id: 2, author: "Grace", message: "second" },
      ],
    },
  });

  const authors = view.findAll(".row-who").map((who) => who.text);
  expect(authors).toEqual(["Ada", "Grace"]);
});
