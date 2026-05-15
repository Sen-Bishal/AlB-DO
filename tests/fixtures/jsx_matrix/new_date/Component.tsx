export default function Component() {
  const iso = new Date(0).toISOString();
  return <span>{iso}</span>;
}
