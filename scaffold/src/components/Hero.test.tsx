import { test, expect, render } from "albedo/test";
import Hero from "./Hero";
import Counter from "./Counter";

// `albedo test` runs every `*.test.tsx` in the project. This file tests two
// components on their own.
//
// `render()` does not run the component in some test-only DOM. It hands it to
// the renderer the app itself uses for that component's tier: the build's
// Tier-A renderer for Hero (the same function that bakes it into `/`), the
// server's renderer for Counter. What you assert against is the markup a
// visitor gets.

test("the masthead is static markup with the tagline", () => {
  const hero = render(<Hero />);

  expect(hero.tier).toBe("A");
  expect(hero.find("h1")?.text).toBe("ALB’DO");
  expect(hero).toContain("Evolution simplified");
});

test("the counter starts at zero", () => {
  const counter = render(<Counter />);

  expect(counter.find(".tally")?.text).toBe("0");
});

// `state` seeds a component's `useState` hooks, in the order it calls them —
// how you test what it looks like after some clicks, without the clicks.
test("the counter shows the count it holds", () => {
  const counter = render(<Counter />, { state: [41] });

  expect(counter.find(".tally")?.text).toBe("41");
});
