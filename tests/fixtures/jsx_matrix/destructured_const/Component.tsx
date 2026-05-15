export default function Component() {
  const { label, count } = { label: "hits", count: 7 };
  return <span>{label}:{count}</span>;
}
