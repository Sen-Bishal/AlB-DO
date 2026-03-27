export function hydrate(componentId, placeholderId, props) {
  const target = document.getElementById(placeholderId);
  if (!target) {
    return;
  }
  target.setAttribute("data-albedo-hydrated", "true");
  if (typeof window.__ALBEDO_HYDRATE_ISLAND === "function") {
    try {
      window.__ALBEDO_HYDRATE_ISLAND({ componentId, placeholderId, props, target });
    } catch (err) {
      console.error(err);
    }
  }
}
