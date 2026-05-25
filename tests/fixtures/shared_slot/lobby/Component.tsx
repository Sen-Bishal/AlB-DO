import { useSharedSlot } from "albedo";

export default function Lobby() {
  const messages = useSharedSlot("chat:lobby");
  return <ul data-room="lobby">{messages}</ul>;
}
