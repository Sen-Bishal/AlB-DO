export default async function LiveFeed() {
  // Tier C: async fetch introduces an IO boundary, so this is streamed from server.
  const response = await fetch("https://jsonplaceholder.typicode.com/posts?_limit=3");
  const posts = (await response.json()) as Array<{ id: number; title: string }>;

  return (
    <section>
      <h2>Tier C - Streamed Feed</h2>
      <p>
        Async data work is intentionally placed here to demonstrate non-blocking
        streamed output.
      </p>
      <ul>
        {posts.map((post) => (
          <li key={post.id}>{post.title}</li>
        ))}
      </ul>
    </section>
  );
}
