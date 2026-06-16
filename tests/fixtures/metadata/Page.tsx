export const metadata = {
  title: "Home — ALBEDO",
  description: "The fastest way to ship.",
  keywords: ["albedo", "ssr", "rust"],
  openGraph: {
    title: "Home OG",
    url: "https://albedo.dev",
    siteName: "ALBEDO",
    type: "website",
    images: "https://albedo.dev/og.png",
  },
  twitter: {
    card: "summary_large_image",
    title: "Home on Twitter",
  },
};

export default function Page() {
  return <main><h1>Home</h1></main>;
}
