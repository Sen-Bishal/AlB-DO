export default function Component() {
  const items = [{ label: "a" }, { label: "b" }];
  return <ul>{items.map((it) => <li>{it.label}</li>)}</ul>;
}
