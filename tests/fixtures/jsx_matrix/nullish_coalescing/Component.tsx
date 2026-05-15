export default function Component() {
  const a = null;
  const b = "fallback";
  return <span>{a ?? b}</span>;
}
