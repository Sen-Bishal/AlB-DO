export default function RootLayout({ route }) {
  return (
    <div class="layout-root" data-route={route}>
      <nav>
        NAV <a href="/nested" aria-current={route === "/nested" ? "page" : undefined}>nested</a>
      </nav>
      <children />
    </div>
  );
}
