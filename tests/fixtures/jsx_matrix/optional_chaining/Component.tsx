export default function Component() {
  const u: { name?: string } | null = null;
  return <span>{u?.name ?? "anon"}</span>;
}
