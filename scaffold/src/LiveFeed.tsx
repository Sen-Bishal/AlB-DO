export default async function LiveFeed() {
  // Tier C: async fetch at render time — AlBDO streams this card after
  // the static shell arrives, so first paint is never blocked on IO.
  let posts: Array<{ id: number; title: string }> = [];
  try {
    const response = await fetch(
      "https://jsonplaceholder.typicode.com/posts?_limit=3"
    );
    posts = (await response.json()) as Array<{ id: number; title: string }>;
  } catch {
    posts = [
      { id: 1, title: "Offline — showing a local fallback" },
      { id: 2, title: "Tier C streams data without blocking shell paint" },
      { id: 3, title: "Swap this fetch for your own API to see it live" },
    ];
  }

  return (
    <>
      <div className="card-head">
        <span className="tier-badge">
          <span>tier c</span>
        </span>
        <span className="card-meta">
          <span>streamed</span>
        </span>
      </div>
      <h3 className="card-title">Streamed from server</h3>
      <p className="card-body">
        Async boundaries flush independently. AlBDO renders the shell first
        and pipes this card into its own stream slot the moment data lands.
      </p>
      <ul className="feed-list">
        {posts.map((post, i) => (
          <li key={post.id} className="feed-item">
            <span className="index">{String(i + 1).padStart(2, "0")}</span>
            <span>{post.title}</span>
          </li>
        ))}
      </ul>
    </>
  );
}
