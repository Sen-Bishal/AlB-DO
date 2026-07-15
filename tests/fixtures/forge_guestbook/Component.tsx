import { useSharedSlot } from "albedo";

interface Entry {
  id: number;
  author: string;
  message: string;
}

export default function Guestbook() {
  const entries = useSharedSlot<Entry[]>("guestbook");
  return (
    <ul data-forge="guestbook">
      {entries.map((entry) => (
        <li>
          {entry.author}: {entry.message}
        </li>
      ))}
    </ul>
  );
}
