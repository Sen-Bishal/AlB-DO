export default function Component() {
  const x: string | null = "ok";
  return <span>{x!}</span>;
}
