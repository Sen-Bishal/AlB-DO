const q = Object.create(null);

function flush() {
  for (const id in q) {
    const el = document.getElementById(id);
    if (!el) {
      continue;
    }
    const pending = q[id];
    delete q[id];
    apply(el, pending.html, pending.status);
  }
}

function apply(el, html, status) {
  if (html === null) {
    el.setAttribute("data-albedo-error", status || "error");
    return;
  }
  el.outerHTML = html;
  flush();
}

window.__albedo_inject = function (id, html, status) {
  const el = document.getElementById(id);
  if (!el) {
    q[id] = { html, status };
    return;
  }
  apply(el, html, status);
};

if (typeof MutationObserver !== "undefined") {
  new MutationObserver(flush).observe(document.documentElement, {
    childList: true,
    subtree: true,
  });
}

window.__albedo_hydrate = function (componentId, placeholderId, props) {
  import("/_albedo/hydration.js").then((rt) => {
    if (rt && typeof rt.hydrate === "function") {
      rt.hydrate(componentId, placeholderId, props);
    }
  });
};
