export default function Component() {
  const n = 5;
  return <span>{n > 3 ? "big" : "small"}/{n <= 5 ? "fits" : "over"}</span>;
}
