import { action } from "albedo";

export const post_chat_message = action(async ({ form, broadcast }) => {
  if (!form.text.trim()) return { error: { text: "say something" } };
  await broadcast(`chat:${form.room}`, msgs => [...msgs, { from: "anon", text: form.text }]);
});

export const ping = action(() => "pong");

export default function ChatRoom() {
  return <div>chat-room</div>;
}
