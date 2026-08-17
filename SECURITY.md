# Security Policy

## Supported versions

AlBDO is pre-1.0 and under active development. Only the latest release on
`main` receives fixes of any kind, including security fixes.

| Version | Supported |
| ------- | --------- |
| `0.1.0-alpha.x` (latest) | ✅ |
| anything older | ❌ |

There is no long-term support branch and no backporting. If you are running
an older build, the fix is to update.

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.**

Report it through GitHub's
[private vulnerability reporting](https://github.com/Sen-Bishal/AlB-DO/security/advisories/new)
on this repository. The report is visible only to the maintainers.

Please include:

- what you can do with it, not just what is wrong
- the steps or a minimal project that reproduces it
- the AlBDO version (`albedo --version`) and your OS

**What to expect.** This is a small project, so the honest answer is that
response times depend on availability rather than a staffed rota — expect an
acknowledgement within a few days. You will be told whether the report is
accepted, and credited in the advisory when a fix ships unless you ask not
to be.

## Scope

The parts of AlBDO where a report is most likely to be a real vulnerability:

- **The wire decoders** (`src/ir/wire.rs`, `src/ir/opcode.rs`) — these parse
  untrusted bytes. Any input that panics, hangs, or reads out of bounds is a
  bug; there are `cargo fuzz` targets under `fuzz/` for exactly this.
- **The action path** (`POST /_albedo/action`) — CSRF handling, argument
  decoding, and anything that lets a caller invoke a handler it should not.
- **Topic naming and authorization** — AlBDO derives read authorization from
  the topic a read mints. Any read path that reaches stored data *without*
  minting a topic is outside that guarantee, and is the highest-value thing
  to look for.
- **The embedded JS engine** (QuickJS) — anything that escapes the per-request
  boundary or reaches state belonging to another request.
- **The dev server** — it is intended for `localhost` only. Findings that
  require exposing `albedo dev` to a network are in scope only if the fix is
  to refuse that configuration.

## Out of scope

- Denial of service through simple resource exhaustion (a large request body,
  an enormous component tree). These are known and unhardened pre-1.0.
- Anything requiring a malicious dependency already present in the user's own
  `package.json`, or malicious source in the project being compiled. AlBDO
  compiles and runs the code it is given; that is the job, not a bypass.
- Missing hardening headers on the scaffold's starter pages.
