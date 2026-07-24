// A per-row edit form: one `<form action="action:set_score">` rendered inside a
// list `.map()`, so it repeats once per row. Its per-field `data-albedo-error`
// ids are constant per (action, field), so seeding them would stamp duplicate
// `data-albedo-id`s across rows — the compiled manifest must suppress the spans
// (and the submit projection) for this action. Metadata-only fixture: `rows`
// need not resolve, the extraction is syntactic.
export default function Rows() {
  return (
    <ul>
      {rows.map((row) => (
        <li key={row.id}>
          <form action="action:set_score">
            <input name="id" type="hidden" />
            <input name="score" />
            <button type="submit">save</button>
          </form>
        </li>
      ))}
    </ul>
  );
}
