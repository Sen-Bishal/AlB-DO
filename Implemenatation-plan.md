# Phase A — Opcode Set + Binary Frame Format

Server-driven UI instruction set and wire encoding for AlBDO. All DOM mutations are expressed as compact opcodes that the server serializes with Bincode v2 and the client decodes into slot-table operations — no JS function shipping, no HTML string diffing.

## Design Decisions 

1. **Intern table shipping** — Dedicated `INIT_INTERN_TABLE` opcode sent on the control stream, with incremental update support built in from day one. The opcode carries a `table_kind` discriminant (`Tag | Attr | Event`) and a list of `(id, value)` entries. The first send bootstraps the full table; subsequent sends append or overwrite individual entries, allowing hot-reload of new components without re-shipping the entire table.

2. **`value_ref` / `text_ref` semantics** — `SET_ATTR` and `SET_TEXT` carry **inline `Vec<u8>`** payloads. No value interning in v1. This keeps the encoder single-pass and the decoder stateless for attribute/text values.

3. **`instruction_range` in `PATCH`** — **`Range<u32>`** byte offsets into the current frame's instruction buffer. Cross-frame references are out of scope for v1.