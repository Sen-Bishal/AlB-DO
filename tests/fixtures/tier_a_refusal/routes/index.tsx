const rest = { id: "x" };

// A spread attribute. `read_attrs` refuses it outright ("spread attributes are
// not supported"), so this route's static render raises — no npm, no hooks, no
// imports involved, which keeps the refusal path itself under test rather than
// the tiering rule that now keeps npm away from it.
export default function SpreadRoute() {
  return (
    <main>
      <h1>spread</h1>
      <p {...rest}>body text</p>
    </main>
  );
}
