import { useState } from "react";

export default function StringState() {
  const [label, setLabel] = useState("idle");
  return (
    <button onClick={() => setLabel("ready")}>{label}</button>
  );
}
