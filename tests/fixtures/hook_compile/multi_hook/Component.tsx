import { useState } from "react";

export default function Form() {
  const [name, setName] = useState("anon");
  const [count, setCount] = useState(0);
  return (
    <div>
      <span>{name}</span>
      <span>{count}</span>
      <button onClick={() => setCount(count + 1)}>bump</button>
      <button onClick={() => setName("alice")}>rename</button>
    </div>
  );
}
