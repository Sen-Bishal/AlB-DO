export default function Component() {
  const ratio = 0.4567;
  return <span>{(ratio * 100).toFixed(1)}%</span>;
}
