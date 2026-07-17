export default function Sign() {
  return (
    <form action="action:sign_guestbook">
      <input name="author" />
      <input name="message" />
      <button type="submit">Sign</button>
    </form>
  );
}
