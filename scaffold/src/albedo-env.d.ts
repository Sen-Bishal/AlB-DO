// ALBEDO ambient JSX types.
//
// Replaces the previous `[k: string]: any` placeholder with concrete
// per-element attribute shapes so authors get autocompletion, error-checked
// event handlers, and red squiggles when a typo lands. The DOM surface is
// narrowed to what bakabox actually applies; properties the client can't
// render are intentionally absent so TypeScript fails fast.

declare namespace JSX {
  // Every JSX render path produces an opaque "element"; userland never
  // needs to access internals, so the alias stays empty.
  interface Element {}
  interface ElementClass {}
  interface ElementAttributesProperty {}
  interface ElementChildrenAttribute {
    children: AlbedoChildren;
  }

  // ── Shared types ────────────────────────────────────────────────

  type AlbedoChild =
    | string
    | number
    | boolean
    | null
    | undefined
    | Element
    | AlbedoChild[];
  type AlbedoChildren = AlbedoChild | AlbedoChild[];

  // Link/form data attributes are reserved by the renderer.
  // Authors may add custom `data-*` attributes via the index signature.
  type DataAttributes = { [key: `data-${string}`]: string | number | boolean };
  type AriaAttributes = { [key: `aria-${string}`]: string | number | boolean };

  // Event handler shape used across every host element.
  type EventHandler<E extends Event = Event> = (event: E) => void;

  // The base properties every JSX element accepts.
  interface AlbedoBaseAttributes extends DataAttributes, AriaAttributes {
    // Framework-reserved props: exactly `key`, `ref` and `children`. The
    // renderer consumes them and they never reach the DOM. `key` in
    // particular is not decoration: keyed-list reconciliation uses it to
    // keep untouched rows' DOM nodes across a write, so a `.map()` over a
    // slot needs it.
    key?: string | number;
    ref?: unknown;
    children?: AlbedoChildren;
    id?: string;
    class?: string;
    className?: string;
    style?: string | Record<string, string | number>;
    title?: string;
    role?: string;
    tabIndex?: number;
    hidden?: boolean;
    lang?: string;
    dir?: 'ltr' | 'rtl' | 'auto';
    // Mouse + pointer events — the subset bakabox dispatches today.
    onClick?: EventHandler<MouseEvent>;
    onDblClick?: EventHandler<MouseEvent>;
    onMouseDown?: EventHandler<MouseEvent>;
    onMouseUp?: EventHandler<MouseEvent>;
    onMouseEnter?: EventHandler<MouseEvent>;
    onMouseLeave?: EventHandler<MouseEvent>;
    onMouseMove?: EventHandler<MouseEvent>;
    onPointerDown?: EventHandler<PointerEvent>;
    onPointerUp?: EventHandler<PointerEvent>;
    // Keyboard events.
    onKeyDown?: EventHandler<KeyboardEvent>;
    onKeyUp?: EventHandler<KeyboardEvent>;
    onKeyPress?: EventHandler<KeyboardEvent>;
    // Focus events.
    onFocus?: EventHandler<FocusEvent>;
    onBlur?: EventHandler<FocusEvent>;
    // Touch events.
    onTouchStart?: EventHandler<TouchEvent>;
    onTouchEnd?: EventHandler<TouchEvent>;
    onTouchMove?: EventHandler<TouchEvent>;
  }

  // ── Form-specific attribute groups ──────────────────────────────

  interface InputAttributes extends AlbedoBaseAttributes {
    type?:
      | 'text'
      | 'password'
      | 'email'
      | 'number'
      | 'tel'
      | 'url'
      | 'search'
      | 'hidden'
      | 'checkbox'
      | 'radio'
      | 'submit'
      | 'reset'
      | 'button'
      | 'file'
      | 'date'
      | 'time'
      | 'datetime-local'
      | 'month'
      | 'week'
      | 'color'
      | 'range';
    name?: string;
    value?: string | number;
    placeholder?: string;
    required?: boolean;
    disabled?: boolean;
    readonly?: boolean;
    checked?: boolean;
    autocomplete?: string;
    autofocus?: boolean;
    min?: string | number;
    max?: string | number;
    step?: string | number;
    minLength?: number;
    maxLength?: number;
    pattern?: string;
    multiple?: boolean;
    accept?: string;
    onChange?: EventHandler<Event>;
    onInput?: EventHandler<InputEvent>;
  }

  interface FormAttributes extends AlbedoBaseAttributes {
    // The renderer recognises `action="action:NAME"` as a sentinel
    // and rewrites the element to `data-albedo-action="NAME"`.
    action?: string;
    method?: 'get' | 'post' | 'GET' | 'POST';
    enctype?: 'application/x-www-form-urlencoded' | 'multipart/form-data' | 'text/plain';
    autocomplete?: 'on' | 'off';
    onSubmit?: EventHandler<SubmitEvent>;
    onReset?: EventHandler<Event>;
  }

  interface ButtonAttributes extends AlbedoBaseAttributes {
    type?: 'button' | 'submit' | 'reset';
    name?: string;
    value?: string;
    disabled?: boolean;
    autofocus?: boolean;
    form?: string;
  }

  interface SelectAttributes extends AlbedoBaseAttributes {
    name?: string;
    value?: string | number;
    multiple?: boolean;
    required?: boolean;
    disabled?: boolean;
    size?: number;
    onChange?: EventHandler<Event>;
  }

  interface OptionAttributes extends AlbedoBaseAttributes {
    value?: string | number;
    selected?: boolean;
    disabled?: boolean;
    label?: string;
  }

  interface TextareaAttributes extends AlbedoBaseAttributes {
    name?: string;
    value?: string;
    placeholder?: string;
    rows?: number;
    cols?: number;
    required?: boolean;
    disabled?: boolean;
    readonly?: boolean;
    maxLength?: number;
    onChange?: EventHandler<Event>;
    onInput?: EventHandler<InputEvent>;
  }

  interface LabelAttributes extends AlbedoBaseAttributes {
    for?: string;
    htmlFor?: string;
  }

  // ── Anchor + media + general element groups ─────────────────────

  interface AnchorAttributes extends AlbedoBaseAttributes {
    href?: string;
    target?: '_blank' | '_self' | '_parent' | '_top' | string;
    rel?: string;
    download?: string | boolean;
    type?: string;
  }

  interface ImgAttributes extends AlbedoBaseAttributes {
    src?: string;
    alt?: string;
    width?: number | string;
    height?: number | string;
    loading?: 'eager' | 'lazy';
    decoding?: 'sync' | 'async' | 'auto';
    srcset?: string;
    sizes?: string;
  }

  interface MetaAttributes extends AlbedoBaseAttributes {
    name?: string;
    content?: string;
    charset?: string;
    httpEquiv?: string;
  }

  interface LinkElementAttributes extends AlbedoBaseAttributes {
    href?: string;
    rel?: string;
    type?: string;
    sizes?: string;
    crossorigin?: 'anonymous' | 'use-credentials';
    integrity?: string;
  }

  interface ScriptAttributes extends AlbedoBaseAttributes {
    src?: string;
    type?: string;
    async?: boolean;
    defer?: boolean;
    crossorigin?: 'anonymous' | 'use-credentials';
    integrity?: string;
  }

  // ── Albedo Link component ───────────────────────────────────────
  // The compiler rewrites `<Link href="...">` to `<a href="..."
  // data-albedo-link>` so the client runtime intercepts the click.

  interface AlbedoLinkAttributes extends AlbedoBaseAttributes {
    href: string;
    target?: '_blank' | '_self' | '_parent' | '_top';
    rel?: string;
  }

  // ── IntrinsicElements ───────────────────────────────────────────
  // Tags not enumerated here fall through to `AlbedoBaseAttributes`
  // via the catch-all at the end, so authors get the shared
  // event-handler surface for any future element without explicit
  // declarations breaking the build.

  interface IntrinsicElements {
    // Document structure
    html: AlbedoBaseAttributes;
    head: AlbedoBaseAttributes;
    body: AlbedoBaseAttributes;
    title: AlbedoBaseAttributes;
    meta: MetaAttributes;
    link: LinkElementAttributes;
    style: AlbedoBaseAttributes;
    script: ScriptAttributes;

    // Sectioning
    div: AlbedoBaseAttributes;
    span: AlbedoBaseAttributes;
    header: AlbedoBaseAttributes;
    footer: AlbedoBaseAttributes;
    main: AlbedoBaseAttributes;
    section: AlbedoBaseAttributes;
    article: AlbedoBaseAttributes;
    aside: AlbedoBaseAttributes;
    nav: AlbedoBaseAttributes;

    // Headings + text
    h1: AlbedoBaseAttributes;
    h2: AlbedoBaseAttributes;
    h3: AlbedoBaseAttributes;
    h4: AlbedoBaseAttributes;
    h5: AlbedoBaseAttributes;
    h6: AlbedoBaseAttributes;
    p: AlbedoBaseAttributes;
    pre: AlbedoBaseAttributes;
    code: AlbedoBaseAttributes;
    blockquote: AlbedoBaseAttributes;
    em: AlbedoBaseAttributes;
    strong: AlbedoBaseAttributes;
    small: AlbedoBaseAttributes;
    br: AlbedoBaseAttributes;
    hr: AlbedoBaseAttributes;

    // Lists
    ul: AlbedoBaseAttributes;
    ol: AlbedoBaseAttributes;
    li: AlbedoBaseAttributes;
    dl: AlbedoBaseAttributes;
    dt: AlbedoBaseAttributes;
    dd: AlbedoBaseAttributes;

    // Tables
    table: AlbedoBaseAttributes;
    thead: AlbedoBaseAttributes;
    tbody: AlbedoBaseAttributes;
    tfoot: AlbedoBaseAttributes;
    tr: AlbedoBaseAttributes;
    th: AlbedoBaseAttributes;
    td: AlbedoBaseAttributes;
    caption: AlbedoBaseAttributes;

    // Forms
    form: FormAttributes;
    input: InputAttributes;
    textarea: TextareaAttributes;
    select: SelectAttributes;
    option: OptionAttributes;
    optgroup: AlbedoBaseAttributes;
    button: ButtonAttributes;
    label: LabelAttributes;
    fieldset: AlbedoBaseAttributes;
    legend: AlbedoBaseAttributes;
    output: AlbedoBaseAttributes;

    // Anchors + media
    a: AnchorAttributes;
    img: ImgAttributes;
    picture: AlbedoBaseAttributes;
    source: AlbedoBaseAttributes;
    video: AlbedoBaseAttributes;
    audio: AlbedoBaseAttributes;
    track: AlbedoBaseAttributes;
    iframe: AlbedoBaseAttributes;
    canvas: AlbedoBaseAttributes;
    svg: AlbedoBaseAttributes;
    path: AlbedoBaseAttributes;
    circle: AlbedoBaseAttributes;
    rect: AlbedoBaseAttributes;
    line: AlbedoBaseAttributes;
    polyline: AlbedoBaseAttributes;
    polygon: AlbedoBaseAttributes;
    g: AlbedoBaseAttributes;
    text: AlbedoBaseAttributes;

    // Albedo built-in components
    Link: AlbedoLinkAttributes;

    // `<children />` intrinsic marks the
    // substitution point inside a `routes/layout.tsx`. The
    // renderer emits a sentinel comment that the manifest builder
    // post-substitutes with the leaf route's HTML.
    children: AlbedoBaseAttributes;

    // Catch-all for tags not enumerated. Keeps the surface
    // permissive while signalling "you're outside the supported
    // tag set" via the more-specific entries above.
    [tagName: string]: AlbedoBaseAttributes;
  }
}

// ── `albedo` framework module surface ─────────────────
//
// The runtime recognises `useSharedSlot` + `action` only when they
// resolve to imports from `"albedo"`. The declarations below
// surface them to TypeScript so authors get autocomplete and the
// renderer's extractor sees the canonical binding source.

declare module "albedo" {
  // Read-only handle on a server-side broadcast topic.
  // The value flows in over the WT patches lane on first paint and
  // on every subsequent `broadcast()` write. `T` is whatever JSON
  // shape the action handlers write — strings, numbers, arrays,
  // structured objects all round-trip.
  export function useSharedSlot<T = unknown>(topic: string): T;

  // A collection (or one partition of one) imported from `albedo/forge`.
  //
  // Declared structurally rather than by importing the generated
  // `albedo/forge` types, and deliberately: that module's declarations are
  // emitted per project on build, so a type reference to them here would
  // make this file fail to resolve in a project that has not built yet —
  // which is every project on its first `npm run typecheck`. The `__row`
  // phantom is the same shape the generator emits, so inference still
  // carries the row type through.
  export interface SharedSlotSource<Row = unknown> {
    readonly __row?: Row;
  }

  // Reading a collection gives you its rows, typed from your
  // `albedo.config.ts` — you do not annotate them.
  export function useSharedSlot<Row>(source: SharedSlotSource<Row>): Row[];

  // The submitted fields of a `<form action="action:NAME" method="POST">`,
  // keyed by each input's `name` attribute, passed as the handler's
  // argument. A field shadows a module constant or prop of the same name.
  // Values arrive as strings: an `<input name="id">` is `"3"`, not `3`.
  export type ActionForm = Record<string, string>;

  // Who is asking — the same value a component's `user` prop, a middleware's
  // `context.user` and a job's `context.user` receive. Profile fields the
  // provider did not supply are `null`, never missing, so `user.name ?? …`
  // means the same thing however the person signed in.
  //
  // Only `id` may key a partition (`.where({ owner: user.id })`): an email
  // or a name changes when the person does, and an owner key must not.
  export interface AlbedoUser {
    readonly id: string;
    readonly email: string | null;
    readonly name: string | null;
    readonly image: string | null;
    // The provider THIS session signed in with; `null` for a run nothing
    // signed in — a job's, or a webhook's.
    readonly provider: string | null;
    // Every provider connected to the account ("password", "github", …).
    // Connect one by POSTing to `/_albedo/auth/oauth/<provider>/link`,
    // disconnect by POSTing `provider` to `/_albedo/auth/unlink`; the last
    // way in cannot be disconnected.
    readonly providers: readonly string[];
  }

  // What every action handler receives. `user` is `null` for an anonymous
  // request, exactly as a component's prop is.
  //
  // `origin` is where this app is reached — "https://your.domain", with no
  // trailing slash — for an absolute URL another service sends people back to:
  // `${origin}/account`. It comes from ALBEDO_PUBLIC_ORIGIN (or the ACME
  // domain, or localhost in development) and never from the request's Host
  // header, which the caller controls. `null` when nothing configures one.
  //
  // `fetch()` works in an action body. A call aimed at a source declared in
  // albedo.config.ts carries that source's `auth` and `headers`, attached by
  // the server — so a secret key never appears in your code:
  //
  //   sources: { stripe: { base: "https://api.stripe.com", auth: { bearerEnv: "STRIPE_SECRET_KEY" } } }
  //
  //   const res = await fetch("https://api.stripe.com/v1/checkout/sessions", { method: "POST", body });
  //
  // Don't set that header yourself: a call that brings its own is refused.
  export interface ActionArgs {
    form: ActionForm;
    user: AlbedoUser | null;
    origin: string | null;
  }

  // What a FORM's action may return. It is written into slots the compiler
  // reserved inside that form — no client code, no state to manage:
  //
  //   return { error: { email: "Enter a real address." } }  // that field's slot
  //   return { ok: "You're on the list." }                  // the form's success slot
  //   return {}                                             // clears them all
  //
  // One or the other: a failing result clears a stale success message, and a
  // succeeding one clears stale errors. It works with JavaScript off too — the
  // message rides the redirect back and the page lands on the form.
  // Style the slots with `[data-albedo-error]` and `[data-albedo-ok]`; the
  // success slot is a `role="status"` live region, so it is also announced.
  //
  //   return { redirect: "/account" }                        // go somewhere else
  //
  // A path in this app always may. Another origin only when it is listed in
  // `redirects` in albedo.config.ts, and it must be `https://` — anything else
  // fails the action rather than sending anyone there, so no URL built from a
  // form field or an upstream reply can become an open redirect. The check
  // runs AFTER the body, so a write the body made has already committed.
  // A result that also carries a non-empty `error` stays on the form.
  export type FormResult =
    | { error: Record<string, string> }
    | { ok: string | true }
    | { redirect: string }
    | Record<string, never>
    | void;

  // Declare an HTTP action handler. Body
  // runs server-side when bakabox POSTs `/_albedo/action` for this
  // declaration's `action_id` (FNV-1a-32 of the export name).
  //
  // `Args` defaults to the concrete `ActionArgs` rather than `unknown` so
  // that `action(({ form }) => …)` infers without an annotation — a
  // destructuring pattern gives TypeScript nothing to infer from, so an
  // `unknown` default makes the framework's own idiom fail to compile.
  // Narrow it when the field set is known:
  //
  //   action<{ form: { author: string; message: string } }>(({ form }) => …)
  export function action<Args extends ActionArgs = ActionArgs, R = void>(
    handler: (args: Args) => R | Promise<R>,
  ): (args: Args) => Promise<R>;
}

// ── `src/middleware.ts` ─────────────────────────────────────────────
//
// One file, run on every app request its `config.matcher` admits —
// pages, `public/` files, actions and uploads. The body can return
// nothing (continue), or one of the four decisions below. It can refuse,
// redirect or rewrite; it can never grant — a route's own
// `export const auth` still runs on whatever path the request lands on.
//
// The server validates every field in Rust, so these types describe what
// is accepted rather than being the check.
//
// `fetch()` works inside a middleware — the global one, with `.json()` and
// `.text()` — and goes out through the same egress policy an action's does:
// a private or loopback host must be declared as a `sources` base. Every call
// counts against the visitor's outbound rate limit, and every call must be
// awaited: one still in flight when the middleware returns is an error.
declare module "albedo/middleware" {
  // The request as the middleware sees it. `cookies` never includes the
  // framework's own session cookies, and there is no raw `cookie` header:
  // the identity a session proves arrives already resolved, as `user`.
  export interface MiddlewareRequest {
    readonly method: string;
    readonly path: string;
    readonly query: string | null;
    readonly headers: Readonly<Record<string, string>>;
    readonly cookies: Readonly<Record<string, string>>;
  }

  export interface MiddlewareContext {
    // The same `user` every render sees; `null` when anonymous.
    readonly user: import("albedo").AlbedoUser | null;
  }

  // `set-cookie` may repeat; every other header is a single string.
  export type MiddlewareHeaders = Record<string, string | string[]>;

  export interface MiddlewareDecision {
    readonly __albedo_middleware: "next" | "redirect" | "rewrite" | "respond";
  }

  // Continue to the route, adding `headers` to its response.
  export function next(init?: { headers?: MiddlewareHeaders }): MiddlewareDecision;
  // A path starting with `/`, or an absolute http(s) URL. Defaults to 307.
  export function redirect(
    location: string,
    init?: 301 | 302 | 303 | 307 | 308 | { status?: 301 | 302 | 303 | 307 | 308; headers?: MiddlewareHeaders },
  ): MiddlewareDecision;
  // Serve another path in this app instead (never under `/_albedo/`).
  export function rewrite(path: string, init?: { headers?: MiddlewareHeaders }): MiddlewareDecision;
  // Answer now. `body` is a string — JSON.stringify an object first.
  export function respond(
    body?: string,
    init?: { status?: number; headers?: MiddlewareHeaders },
  ): MiddlewareDecision;

  export type Middleware = (
    request: MiddlewareRequest,
    context: MiddlewareContext,
  ) => MiddlewareDecision | void | Promise<MiddlewareDecision | void>;
}

// ── JOBS — scheduled and enqueued background work ─────────────
//
// One `src/jobs.ts`, one export per job. The options are read from the
// SOURCE at build time, not at runtime, so every value here must be a
// literal — a computed schedule is a build error, because a schedule the
// compiler cannot read is one it cannot refuse.
declare module "albedo/jobs" {
  export interface JobContext {
    // Who this run is FOR. `null` on a plain scheduled run, which is
    // anonymous exactly like an unauthenticated request — an
    // identity-partitioned collection is refused to it.
    //
    // Non-null on a run reached by `over: "users"`, where the scheduled
    // fire expands into one run per principal. There is no third case:
    // ALBEDO has no system identity that sees every user's rows, which is
    // why a per-user job is a fan-out rather than a privileged read.
    //
    // A job enqueued by a request, or by a webhook that names its principal
    // (`principal:` in the webhook's declaration), runs FOR that person too.
    readonly user: import("albedo").AlbedoUser | null;
  }

  export interface JobOptions {
    // When it runs on its own. Omit for a job that only runs when
    // enqueued. Five-field cron ("0 3 * * *"), an alias ("@daily",
    // "@hourly", "@weekly", "@monthly", "@yearly", "@minutely"), or an
    // interval ("every 30s", "every 5m", "every 2h").
    //
    // Five fields, as in a crontab — NOT the six-field seconds-first form.
    readonly schedule?: string;
    // Expand each scheduled fire into one run per registered principal,
    // each receiving that principal as `context.user`. Requires
    // `schedule`; an enqueued job already carries the principal that
    // enqueued it.
    readonly over?: "users";
    // Attempts AFTER the first, with exponential backoff and jitter.
    // Defaults to 0 — a body that is not idempotent must not be retried
    // by accident, so retrying is something you ask for.
    readonly retries?: number;
    // How long one run may take before it is interrupted. "30s" by
    // default; "15m" is the ceiling. Work that needs longer wants to be
    // several enqueued jobs, so a restart costs one step instead of all
    // of it.
    readonly timeout?: string;
  }

  // Declare a job. Returns the handler; the options are build-time facts.
  export function job<Args = Record<string, never>, R = void>(
    options: JobOptions,
    handler: (args: Args, context: JobContext) => R | Promise<R>,
  ): (args: Args, context: JobContext) => R | Promise<R>;

  // ── Writes, imported rather than ambient ──────────────────────
  //
  // A job body imports what it may do. That is not style: `append` and its
  // siblings are ambient globals inside an ACTION body only because they are
  // locals of the per-request handler, unreachable from any package sharing
  // the realm. A job body is an ordinary module, so making them global would
  // hand every package in the realm a reachable database write. Importing
  // them keeps that surface closed.
  //
  // They may only be called while a job is running; calling one at module
  // top level throws.
  export function append<T extends Record<string, unknown>>(
    collection: string,
    record: T,
  ): void;
  export function remove(collection: string, key: string | number): void;
  export function update<T extends Record<string, unknown>>(
    collection: string,
    key: string | number,
    fields: T,
  ): void;

  // Queue another job. It inherits THIS run's principal: work enqueued by
  // one user's fan-out run stays that user's. Nothing widens.
  export function enqueue<T extends Record<string, unknown>>(
    name: string,
    args?: T,
    options?: { id?: string; delay?: string },
  ): void;

  // Queue an email. See the global `sendEmail` below for the shape.
  export function sendEmail(
    message: AlbedoEmail,
    options?: { id?: string; delay?: string },
  ): void;
}

// ── The supported React surface ─────────────────────────────────
//
// ALBEDO is React-shaped, and `useState` is only recognised when it
// resolves to an import from `"react"` — the compiler checks the binding's
// source, exactly as `useSharedSlot` is checked against `"albedo"`. So this
// import is load-bearing, not a compatibility shim, and it has to be
// declared for the scaffold's own Counter to type-check.
//
// What is declared here is what the runtime actually implements, on the
// server (where effects are no-ops) and in the browser (where `useEffect`
// runs after paint). Anything absent from this list is absent
// on purpose — a missing export is a build error, which is the honest
// signal. Do not widen it to match React's API without a runtime path to
// match; a declaration the interpreter cannot honour turns a compile-time
// error into a silent no-op at runtime.
declare module "react" {
  // Slot-backed state. The setter takes a VALUE, not an updater function:
  // it evaluates its argument and stores it as JSON, so
  // `setCount(c => c + 1)` would store a function and is deliberately not
  // typed. Read the current value and pass the next one —
  // `setCount(count + 1)`.
  export function useState<S>(initial: S): [S, (next: S) => void];

  // Runs after the component is painted on the client; a no-op on the
  // server. Returning a function registers teardown. A component that
  // calls this hydrates eagerly rather than on interaction — a passive
  // effect would otherwise never run.
  export function useEffect(
    effect: () => void | (() => void),
    deps?: readonly unknown[],
  ): void;

  export function useRef<T>(initial: T): { current: T };

  export function useMemo<T>(factory: () => T, deps?: readonly unknown[]): T;

  export function useCallback<T extends (...args: never[]) => unknown>(
    callback: T,
    deps?: readonly unknown[],
  ): T;
}

// `broadcast(topic, updater)` is a free
// ident the interpreter intercepts inside action handler bodies.
// The TypeScript declaration mirrors what the interpreter expects.
declare function broadcast<T>(
  topic: string,
  updater: (current: T) => T,
): Promise<void>;

// ── FORGE writes ────────────────────────────────────────────────
//
// Like `broadcast`, these are free idents recognised inside action
// handler bodies — not imports. They record a durable write against
// a collection declared in the `forge` block of `albedo.config.ts`;
// the server applies it after the handler body returns, then
// rematerializes the collection and fans the change out to every
// subscribed client.

// Insert a record. `id` is implicit and assigned by the substrate —
// do not pass it.
declare function append<T extends Record<string, unknown>>(
  collection: string,
  record: T,
): Promise<void>;

// Retract the row identified by `key` (its `id`).
declare function remove(
  collection: string,
  key: string | number,
): Promise<void>;

// Update the row identified by `key` with the given partial fields.
declare function update<T extends Record<string, unknown>>(
  collection: string,
  key: string | number,
  fields: T,
): Promise<void>;

// JOBS — put background work on the queue from an action.
//
// The shape transactional email wants: the action returns immediately, the
// work retries on its own, and `options.id` makes a double-submit land once.
// The queued job inherits THIS request's principal, so it runs as the user
// who caused it — there is no system identity to escalate to.
//
// `name` is an export in `src/jobs.ts`; an unknown one fails the run loudly
// rather than queueing a row that could only ever dead-letter.
declare function enqueue<T extends Record<string, unknown>>(
  name: string,
  args?: T,
  options?: {
    // Makes the enqueue idempotent — a repeat with the same id is a no-op.
    id?: string;
    // Wait this long before it may run: "30s", "5m", "2h", "1d".
    delay?: string;
  },
): Promise<void>;

// ── EMAIL ──────────────────────────────────────────────────────
//
// `sendEmail` does not send: it validates the message and queues it, and
// the job runner delivers it with retries — so a provider outage delays
// the email instead of failing the action. Needs an `email` block in
// albedo.config. An invalid address or a line break in `subject` throws
// HERE, in the action, rather than dead-lettering an hour later.
//
// Addresses are "ada@example.com" or "Ada Lovelace <ada@example.com>".
// At most 50 recipients across to/cc/bcc; `text` + `html` at most 1 MiB.
interface AlbedoEmail {
  to: string | string[];
  subject: string;
  text?: string;
  html?: string;
  cc?: string | string[];
  bcc?: string | string[];
  replyTo?: string;
  // Overrides the block's `from`; must be a sender your provider accepts.
  from?: string;
  // Up to 10 files, 10 MiB in all. Inline content (base64) is capped at
  // 5 MiB because it waits in the queue row; for anything larger, attach a
  // stored upload by id. An upload is attachable only by the user who
  // uploaded it — an id is a name, not a permission.
  attachments?: Array<
    | { filename: string; content: string; contentType?: string }
    | { upload: string; filename?: string; contentType?: string }
  >;
}
declare function sendEmail(
  message: AlbedoEmail,
  options?: {
    // Makes it idempotent — a double-submitted form sends once.
    id?: string;
    // Wait this long before sending: "30s", "5m", "2h", "1d".
    delay?: string;
  },
): Promise<void>;

// The `<children />` JSX intrinsic in
// `routes/layout.tsx` marks where the wrapped route renders.
// Declared here so the type-checker stops flagging the unknown
// tag; the renderer treats it as a sentinel-emitting host element.

// Side-channel globals the client runtime publishes for advanced
// userland integrations (e.g. instrumenting the WT debug slot).
//
// Declared at top level, NOT inside `declare global { … }` — and this
// file must never gain a top-level `import`/`export`. A `.d.ts` with one
// becomes a *module*, at which point `declare namespace JSX` is only
// file-local (every JSX tag then errors TS7026), the free idents below
// stop being globals (TS2304), and `declare module "albedo"` is reparsed
// as an augmentation of a module that must already resolve (TS2307).
// That single trailing `export {}` cost 139 errors in a freshly
// generated app. In a script file `declare global` is itself an error,
// so `interface Window` augments the global scope directly.
interface Window {
  __ALBEDO_RUNTIME?: {
    applyFrameBytes?: (bytes: Uint8Array) => void;
    encodeActionEnvelope?: (envelope: {
      action_id: number;
      event_kind: number;
      payload: Uint8Array;
    }) => Uint8Array;
    requestRouteRefresh?: (path: string) => Promise<void>;
    registerInstructionHandler?: (
      name: string,
      handler: (instruction: unknown) => void,
    ) => void;
    hashActionName?: (name: string) => number;
  };
}
