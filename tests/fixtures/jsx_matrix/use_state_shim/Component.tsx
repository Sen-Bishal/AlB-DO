import { useState } from "react";

export default function Component() {
  const [count, setCount] = useState(0);
  const [label, setLabel] = useState("idle");
  return <span>{label}:{count}</span>;
}
