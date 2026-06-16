export const metadata = {
  title: "Static Title",
  description: "static description",
};

export default function Page() {
  return (
    <main>
      <title>JSX Title Wins</title>
      <meta property="og:title" content="JSX OG Title" />
      <h1>Body content</h1>
    </main>
  );
}
