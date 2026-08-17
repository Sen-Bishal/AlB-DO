import { brand, site, items, derived } from "./config";

export default function Component() {
  return (
    <div>
      <h1>{brand}</h1>
      <p>{site.tagline}</p>
      <span>{derived}</span>
      <ul>
        {items.map((label) => (
          <li>{label}</li>
        ))}
      </ul>
    </div>
  );
}
