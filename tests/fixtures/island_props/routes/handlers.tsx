import Pressable from "../components/Pressable";

// A parent that passes its island a handler beside its data. The handler must
// be dropped from the captured props and the data kept — including `once`,
// which begins with "on" and is not a handler.
export default function Handlers() {
  return (
    <section className="handlers">
      <Pressable step={3} once="yes" onBump={() => 1} />
    </section>
  );
}
