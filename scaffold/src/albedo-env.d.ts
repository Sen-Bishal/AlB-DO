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

  // Phase L Link/form data attributes are reserved by the renderer.
  // Authors may add custom `data-*` attributes via the index signature.
  type DataAttributes = { [key: `data-${string}`]: string | number | boolean };
  type AriaAttributes = { [key: `aria-${string}`]: string | number | boolean };

  // Event handler shape used across every host element.
  type EventHandler<E extends Event = Event> = (event: E) => void;

  // The base properties every JSX element accepts.
  interface AlbedoBaseAttributes extends DataAttributes, AriaAttributes {
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

    // Phase P · Stream E.1 — `<children />` intrinsic marks the
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

// ── Phase P · `albedo` framework module surface ─────────────────
//
// The runtime recognises `useSharedSlot` + `action` only when they
// resolve to imports from `"albedo"`. The declarations below
// surface them to TypeScript so authors get autocomplete and the
// renderer's extractor sees the canonical binding source.

declare module "albedo" {
  // Phase O.2 — read-only handle on a server-side broadcast topic.
  // The value flows in over the WT patches lane on first paint and
  // on every subsequent `broadcast()` write. `T` is whatever JSON
  // shape the action handlers write — strings, numbers, arrays,
  // structured objects all round-trip.
  export function useSharedSlot<T = unknown>(topic: string): T;

  // Phase P · Stream C.1 — declare an HTTP action handler. Body
  // runs server-side when bakabox POSTs `/_albedo/action` for this
  // declaration's `action_id` (FNV-1a-32 of the export name).
  export function action<Args = unknown, R = void>(
    handler: (args: Args) => R | Promise<R>,
  ): (args: Args) => Promise<R>;
}

// Phase P · Stream C.2 — `broadcast(topic, updater)` is a free
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

// Phase P · Stream E.1 — the `<children />` JSX intrinsic in
// `routes/layout.tsx` marks where the wrapped route renders.
// Declared here so the type-checker stops flagging the unknown
// tag; the renderer treats it as a sentinel-emitting host element.

// Side-channel globals the client runtime publishes for advanced
// userland integrations (e.g. instrumenting the WT debug slot).
declare global {
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
}

export {};
