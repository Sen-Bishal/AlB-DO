# Vendored npm packages, verbatim

Real, unmodified npm packages, committed so a test can start from **what a
package actually produces** rather than from a paraphrase of it.

| package | version | why it is here |
|---|---|---|
| `@radix-ui/react-slot` | 1.3.3 | `asChild`. Its `mergeProps` + `cloneElement` path is what puts a component library's props — including boolean `aria-*` — onto a real host tag. |
| `@radix-ui/react-compose-refs` | 1.1.5 | `react-slot`'s only runtime dependency (`useComposedRefs`). |

Each directory holds the package's `package.json` (with its real `exports` map),
`dist/index.mjs`, and its `LICENSE`. Both packages are MIT-licensed; the licence
files are copied with them, unmodified. No `.map` files: sourcemaps are not part
of what runs, and this repo has already once counted sourcemap entries and
mistaken them for call sites.

## Why vendored rather than resolved from a corpus

`tests/npm_coverage_probe.rs` reads an external tree at
`C:/Development/albedo-corpus`, and is `#[ignore]`d for exactly that reason — a
test that needs a machine-local `npm install` is not a gate, it is a script.
These two files are 9 kB and turn "Radix's real prop merge lands real aria
state" into something CI can fail on.

## Updating

Copy the files again from a real install and re-run the tests. If a new Radix
version changes `mergeProps`, that is a fact the tests should see, not something
to paper over — the point of vendoring the real thing is that it can surprise us.
