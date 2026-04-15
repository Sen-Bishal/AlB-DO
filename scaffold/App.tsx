import Hero from "./components/Hero";
import Counter from "./components/Counter";
import LiveFeed from "./components/LiveFeed";

export default function App() {
  // Tier A: App is pure layout composition. No hooks, no IO, no side effects.
  return (
    <main>
      <h1>AlBDO Tier Pipeline Demo</h1>
      <p>
        This starter includes one component in each effect tier. Run
        <code> albedo dev </code>
        to see the compiler decisions in your terminal.
      </p>

      <Hero />
      <Counter />
      <LiveFeed />
    </main>
  );
}
