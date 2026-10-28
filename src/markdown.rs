//! Markdown pages — a `.md` file under `routes/` is a route.
//!
//! ## The design, in one sentence
//!
//! A Markdown page is lowered, at build time, to a **JSX module**, and from that
//! point on it is an ordinary component: route discovery, the layout chain,
//! `export const metadata`, the static evaluator and its escaping all see a
//! component and nothing else, so none of them needed a Markdown case.
//!
//! 🔒 **No HTML is produced here.** Every run of text becomes a JS string literal
//! inside a JSX expression container (`{"a < b"}`), and every attribute value the
//! same, so the only path a character takes to the wire is the evaluator's own
//! escaping (the path `tests/text_escaping_parity.rs` gates). That is also why raw
//! HTML in a page is refused rather than passed through: passing it through needs
//! a second, unescaped path, and every byte an author types could take it.
//!
//! ## Where the source comes from
//!
//! Every reader of a component's source goes through [`read_module_source`]: the
//! scanner, the evaluator's project load and its dev-time `patch`, `albedo build`'s
//! artifact emitter, and the server's boot-time module registry. 🪤 A reader that
//! calls `std::fs::read_to_string` on a page directly gets Markdown where it
//! expects JavaScript. Grep for the next one before adding it.
//!
//! ## What a page renders
//!
//! ```html
//! <div class="md-page">
//!   <article class="md">…</article>
//!   <nav class="md-toc" aria-label="On this page"><ol>…</ol></nav>
//! </div>
//! ```
//!
//! - The contents list holds the page's `##` and `###` headings, and is emitted
//!   when there are at least two. Frontmatter `toc: false` drops it; `toc: true`
//!   keeps it for a single heading.
//! - Headings get GitHub's ids (`## Two words` → `two-words`, a repeat gets `-1`,
//!   `-2`), or the one written as `{#id}`. `##` and below link to themselves.
//! - Fenced code keeps its language as `data-lang` on the `<pre class="md-code">`.
//! - GitHub alerts (`> [!NOTE]`) become `<blockquote class="md-callout"
//!   data-kind="note">` opening with a visible `<p class="md-callout-label">`.
//! - Tables are wrapped in `<div class="md-table">`, so a wide one can scroll
//!   inside its own box instead of widening the page.
//!
//! Styling is the app's. This module emits structure and class names, nothing else.
//!
//! ## Frontmatter
//!
//! ```markdown
//! ---
//! title: Routing
//! description: File-based routes, layouts and dynamic segments.
//! ---
//! ```
//!
//! `title` falls back to the page's first `#` heading. Values are one line, bare
//! or quoted; this is the flat subset of YAML a page needs, not YAML.
//!
//! ## Links
//!
//! A link to another page is written as its file (`./routing.md`), so the source
//! also reads correctly on GitHub, and is rewritten to that page's route. A link
//! to a page that does not exist fails the build: a docs site finds its dead links
//! at build time or in production.
//!
//! ## Refused, with the file and the line
//!
//! - raw HTML, block or inline (comments are dropped, not refused);
//! - a relative link to a `.md` file that does not exist or is not a page;
//! - a frontmatter key this module does not read;
//! - a Markdown file at a dynamic segment (`[slug].md`), or named like a
//!   convention file (`layout.md`, `error.md`, `loading.md`).
//!
//! `_draft.md` is not a page — the rule `discover_routes` applies to `_Private.tsx`.

use crate::manifest::route_path_from_component;
use crate::routing::ROUTES_DIRNAME;
use crate::runtime::eval::component::{fnv1a_32, normalize_specifier};
use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser,
    Tag, TagEnd,
};
use std::collections::HashSet;
use std::fmt;
use std::path::{Component, Path};

/// The extension that makes a file under `routes/` a Markdown page.
pub const MARKDOWN_EXTENSION: &str = "md";

/// Stems that name a convention file rather than a route. Mirrors
/// `routing::file_based::CONVENTION_STEMS`: a Markdown file cannot wrap anything.
const CONVENTION_STEMS: [&str; 3] = ["layout", "error", "loading"];

const RAW_HTML_REFUSED: &str = "raw HTML is not allowed in a Markdown page. The page is \
     lowered to JSX so every character goes through the renderer's escaping; passing HTML \
     through would need a path that skips it. Write it in Markdown, or make this page a \
     `.tsx` component.";

/// A page that cannot be lowered. Printed as `path:line: message`, the shape every
/// editor and terminal turns into a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownError {
    pub path: String,
    pub line: usize,
    pub message: String,
}

impl fmt::Display for MarkdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.path, self.line, self.message)
    }
}

impl std::error::Error for MarkdownError {}

/// True when `path` is a Markdown page: a `.md` file with a `routes` directory
/// among its ancestors, whose name does not start with `_`.
///
/// The `routes` test is the one [`route_path_from_component`] already applies to
/// every route, so the two cannot disagree about which files are pages. Without
/// it, a `README.md` beside a component would be compiled as one.
#[must_use]
pub fn is_markdown_page(path: &Path) -> bool {
    // Case-sensitive, as `.tsx` is everywhere else: route discovery matches the
    // extension exactly, and a `.MD` it skips must not become a page anyway.
    let is_markdown = path.extension().is_some_and(|ext| ext == MARKDOWN_EXTENSION);
    if !is_markdown {
        return false;
    }
    let is_private = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.starts_with('_'));
    if is_private {
        return false;
    }
    path.parent().is_some_and(|parent| {
        parent
            .components()
            .any(|part| matches!(part, Component::Normal(name) if name == ROUTES_DIRNAME))
    })
}

/// A component's source as the compiler reads it: the file itself, or — for a
/// Markdown page — the JSX module it lowers to.
///
/// A page that cannot be lowered is an `InvalidData` error carrying the
/// `path:line: message`, so every reader that already refuses an unreadable
/// module refuses a broken page the same way, with no Markdown case of its own.
pub fn read_module_source(path: &Path) -> std::io::Result<String> {
    let source = std::fs::read_to_string(path)?;
    if !is_markdown_page(path) {
        return Ok(source);
    }
    compile(path, &source)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))
}

/// Lower the Markdown page at `path` (whose text is `source`) to a JSX module.
///
/// `path` is used for three things only: the page's route (and so its component
/// name), resolving relative `.md` links against the directory it sits in, and
/// the location in an error. Nothing is read or written besides the link targets'
/// existence.
pub fn compile(path: &Path, source: &str) -> Result<String, MarkdownError> {
    let file = path.to_string_lossy().replace('\\', "/");
    // CommonMark treats `\r\n` as a line ending, but a fenced block's text keeps
    // what it was given — a Windows checkout would put a `\r` at the end of every
    // code line on the page.
    let source = source.replace("\r\n", "\n");
    let route = page_route(&file)?;

    let mut lowering = Lowering::new(path, &file, &source);
    for (event, range) in Parser::new_ext(&source, parser_options()).into_offset_iter() {
        lowering.event(event, range.start)?;
    }
    lowering.finish(&route)
}

fn parser_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_GFM
}

/// The route a page serves, refusing the two places a page cannot be.
fn page_route(file: &str) -> Result<String, MarkdownError> {
    let stem = Path::new(file)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    if CONVENTION_STEMS.iter().any(|conv| stem.eq_ignore_ascii_case(conv)) {
        return Err(error_at(
            file,
            1,
            format!(
                "`{stem}` wraps other routes, so it must be a component (`{stem}.tsx`); \
                 a Markdown page is only ever a leaf."
            ),
        ));
    }
    let Some(route) = route_path_from_component(file) else {
        return Err(error_at(
            file,
            1,
            "a Markdown page must sit under `routes/`.".to_string(),
        ));
    };
    if route.contains('[') {
        return Err(error_at(
            file,
            1,
            "a Markdown page is fixed text, so it cannot sit at a dynamic segment (`[slug]`). \
             Make this route a `.tsx` component."
                .to_string(),
        ));
    }
    Ok(route)
}

/// The component a page lowers to is named after its route — `/docs/routing` is
/// `MdDocsRouting`, `/` is `MdIndex` — because the name is what the build prints
/// and every registry keys on. A route with characters that do not survive into
/// an identifier gets a hash of the whole route appended, so `/docs/über` and
/// `/docs/ber` cannot become one component.
fn component_name(route: &str) -> String {
    let mut name = String::from("Md");
    for word in route
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            name.push(first.to_ascii_uppercase());
            name.push_str(chars.as_str());
        }
    }
    if name == "Md" {
        name.push_str("Index");
    }
    let lossy = route
        .chars()
        .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '_' | '.')));
    if lossy {
        name.push_str(&format!("_{:08x}", fnv1a_32(route.as_bytes())));
    }
    name
}

/// A JS string literal. JSON's string grammar is a subset of JavaScript's, so this
/// is exact — quotes, backslashes, control characters and U+2028/9 included.
fn js_string(value: &str) -> String {
    serde_json::to_string(value).expect("a Rust string always serializes to JSON")
}

/// Text in child position: `{"…"}`. Never bare JSX text, which the evaluator
/// would collapse by JSX's whitespace rules and could not hold a `{` or `<`.
fn text_child(value: &str) -> String {
    format!("{{{}}}", js_string(value))
}

fn is_void(tag: &str) -> bool {
    matches!(tag, "br" | "hr" | "img" | "input")
}

/// One JSX element, rendered. `attrs` are `(name, JS expression)` pairs.
fn element(tag: &str, attrs: &[(&str, String)], children: &str) -> String {
    let mut out = format!("<{tag}");
    for (name, value) in attrs {
        out.push_str(&format!(" {name}={{{value}}}"));
    }
    if children.is_empty() && is_void(tag) {
        out.push_str(" />");
    } else {
        out.push('>');
        out.push_str(children);
        out.push_str(&format!("</{tag}>"));
    }
    out
}

fn error_at(file: &str, line: usize, message: String) -> MarkdownError {
    MarkdownError {
        path: file.to_string(),
        line,
        message,
    }
}

/// GitHub's heading id: lowercased, letters, digits, `-` and `_` kept, whitespace
/// to `-`, everything else dropped. Runs of `-` are kept, as GitHub keeps them, so
/// an id copied from a GitHub render of the same file still lands.
fn slug(text: &str) -> String {
    let mut out = String::new();
    for ch in text.trim().to_lowercase().chars() {
        if ch.is_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }
    out
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The info string's first word, restricted to what a language name is made of.
fn code_language(info: &str) -> Option<String> {
    let word: String = info
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '+' | '#' | '.'))
        .collect();
    (!word.is_empty()).then_some(word)
}

/// True when `html` is nothing but HTML comments and whitespace. A comment is an
/// author's note to themselves; dropping it loses nothing a reader would see.
fn is_only_comments(html: &str) -> bool {
    let mut rest = html.trim();
    while let Some(after_open) = rest.strip_prefix("<!--") {
        let Some(close) = after_open.find("-->") else {
            return false;
        };
        rest = after_open[close + 3..].trim_start();
    }
    rest.is_empty()
}

/// `https:`, `mailto:`, `tel:` — anything RFC 3986 would call a scheme.
fn has_scheme(dest: &str) -> bool {
    let Some(colon) = dest.find(':') else {
        return false;
    };
    let scheme = &dest[..colon];
    scheme.starts_with(|ch: char| ch.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
}

/// A one-line frontmatter value: bare, `"double"` (JSON escapes) or `'single'`
/// (`''` for a quote). `None` for a quote that does not close.
fn unquote(value: &str) -> Option<String> {
    if value.starts_with('"') {
        return serde_json::from_str::<String>(value).ok();
    }
    if let Some(inner) = value.strip_prefix('\'') {
        return inner.strip_suffix('\'').map(|inner| inner.replace("''", "'"));
    }
    Some(value.to_string())
}

fn heading_rank(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn heading_tag(rank: u8) -> &'static str {
    match rank {
        1 => "h1",
        2 => "h2",
        3 => "h3",
        4 => "h4",
        5 => "h5",
        _ => "h6",
    }
}

/// `(data-kind, visible label)` for a GitHub alert.
fn callout(kind: BlockQuoteKind) -> (&'static str, &'static str) {
    match kind {
        BlockQuoteKind::Note => ("note", "Note"),
        BlockQuoteKind::Tip => ("tip", "Tip"),
        BlockQuoteKind::Important => ("important", "Important"),
        BlockQuoteKind::Warning => ("warning", "Warning"),
        BlockQuoteKind::Caution => ("caution", "Caution"),
    }
}

/// An element whose children are still arriving.
struct Open {
    tag: &'static str,
    attrs: Vec<(&'static str, String)>,
    children: String,
}

struct Heading {
    rank: u8,
    /// Written as `{#id}`.
    id: Option<String>,
    /// The heading's plain text — for its id and its contents-list entry.
    text: String,
    /// A heading that already holds a link is not wrapped in one: `<a>` in `<a>`
    /// is invalid, and the browser would split the heading apart to repair it.
    has_link: bool,
}

struct Table {
    alignments: Vec<Alignment>,
    in_head: bool,
    body_open: bool,
    cell: usize,
}

struct Image {
    src: String,
    title: String,
    alt: String,
    /// Images nested in alt text (legal, and meaningless) — only their text counts.
    depth: usize,
}

struct Code {
    language: Option<String>,
    text: String,
}

struct ContentsEntry {
    rank: u8,
    id: String,
    text: String,
}

#[derive(Default)]
struct Frontmatter {
    title: Option<String>,
    description: Option<String>,
    toc: Option<bool>,
}

struct Lowering<'a> {
    path: &'a Path,
    file: &'a str,
    line_starts: Vec<usize>,
    stack: Vec<Open>,
    /// Text not yet written to its element. The parser splits one run of text into
    /// several events (at `&`, `<`, escapes, dropped comments); joining them here
    /// makes each run one literal.
    pending_text: String,
    heading: Option<Heading>,
    table: Option<Table>,
    image: Option<Image>,
    code: Option<Code>,
    /// Raw text of the block, and the offset it opened at.
    html_block: Option<(String, usize)>,
    metadata: Option<(String, usize)>,
    contents: Vec<ContentsEntry>,
    ids: HashSet<String>,
    frontmatter: Frontmatter,
    first_h1: Option<String>,
}

impl<'a> Lowering<'a> {
    fn new(path: &'a Path, file: &'a str, source: &str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(source.match_indices('\n').map(|(index, _)| index + 1));
        Self {
            path,
            file,
            line_starts,
            stack: vec![Open {
                tag: "article",
                attrs: vec![("className", js_string("md"))],
                children: String::new(),
            }],
            pending_text: String::new(),
            heading: None,
            table: None,
            image: None,
            code: None,
            html_block: None,
            metadata: None,
            contents: Vec::new(),
            ids: HashSet::new(),
            frontmatter: Frontmatter::default(),
            first_h1: None,
        }
    }

    /// 1-based line of a byte offset.
    fn line_of(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|&start| start <= offset)
    }

    fn error(&self, offset: usize, message: impl Into<String>) -> MarkdownError {
        error_at(self.file, self.line_of(offset), message.into())
    }

    fn event(&mut self, event: Event<'_>, offset: usize) -> Result<(), MarkdownError> {
        // Inside these four, the parser sends text (and the matching end) only, and
        // it is collected raw rather than lowered.
        if self.code.is_some() {
            match event {
                Event::Text(text) => self.code.as_mut().expect("checked").text.push_str(&text),
                Event::End(TagEnd::CodeBlock) => self.close_code(),
                _ => {}
            }
            return Ok(());
        }
        if self.metadata.is_some() {
            match event {
                Event::Text(text) => self.metadata.as_mut().expect("checked").0.push_str(&text),
                Event::End(TagEnd::MetadataBlock(_)) => self.close_metadata()?,
                _ => {}
            }
            return Ok(());
        }
        if self.html_block.is_some() {
            match event {
                Event::Html(html) | Event::Text(html) => {
                    self.html_block.as_mut().expect("checked").0.push_str(&html);
                }
                Event::End(TagEnd::HtmlBlock) => self.close_html_block()?,
                _ => {}
            }
            return Ok(());
        }
        if let Some(image) = self.image.as_mut() {
            match event {
                Event::Start(Tag::Image { .. }) => image.depth += 1,
                Event::End(TagEnd::Image) if image.depth > 0 => image.depth -= 1,
                Event::End(TagEnd::Image) => self.close_image(),
                Event::Text(text) | Event::Code(text) => image.alt.push_str(&text),
                Event::SoftBreak | Event::HardBreak => image.alt.push(' '),
                _ => {}
            }
            return Ok(());
        }

        match event {
            Event::Start(tag) => self.start(tag, offset)?,
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            // Math is not enabled; were it, showing the source as code is honest.
            Event::Code(code) | Event::InlineMath(code) | Event::DisplayMath(code) => {
                if let Some(heading) = self.heading.as_mut() {
                    heading.text.push_str(&code);
                }
                self.push(element("code", &[], &text_child(&code)));
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                if !is_only_comments(&html) {
                    return Err(self.error(offset, RAW_HTML_REFUSED));
                }
            }
            // Footnotes are not enabled; were they, the reference shows as written.
            Event::FootnoteReference(label) => self.text(&format!("[^{label}]")),
            Event::SoftBreak => self.text("\n"),
            Event::HardBreak => self.push(element("br", &[], "")),
            Event::Rule => self.push(element("hr", &[], "")),
            Event::TaskListMarker(checked) => self.push(if checked {
                r#"<input type={"checkbox"} disabled checked />"#.to_string()
            } else {
                r#"<input type={"checkbox"} disabled />"#.to_string()
            }),
        }
        Ok(())
    }

    fn start(&mut self, tag: Tag<'_>, offset: usize) -> Result<(), MarkdownError> {
        match tag {
            Tag::Paragraph => self.open("p", Vec::new()),
            Tag::Heading {
                level,
                id,
                classes,
                attrs,
            } => {
                if !attrs.is_empty() {
                    return Err(self.error(
                        offset,
                        "a heading takes `{#id}` and `{.class}`; other attributes are not supported.",
                    ));
                }
                let rank = heading_rank(level);
                let mut element_attrs = Vec::new();
                if !classes.is_empty() {
                    let classes: Vec<String> = classes.iter().map(|class| class.to_string()).collect();
                    element_attrs.push(("className", js_string(&classes.join(" "))));
                }
                self.heading = Some(Heading {
                    rank,
                    id: id.map(|id| id.to_string()),
                    text: String::new(),
                    has_link: false,
                });
                self.open(heading_tag(rank), element_attrs);
            }
            Tag::BlockQuote(None) => self.open("blockquote", Vec::new()),
            Tag::BlockQuote(Some(kind)) => {
                let (name, label) = callout(kind);
                self.open(
                    "blockquote",
                    vec![
                        ("className", js_string("md-callout")),
                        ("data-kind", js_string(name)),
                    ],
                );
                self.push(element(
                    "p",
                    &[("className", js_string("md-callout-label"))],
                    &text_child(label),
                ));
            }
            Tag::CodeBlock(kind) => {
                self.code = Some(Code {
                    language: match kind {
                        CodeBlockKind::Fenced(info) => code_language(&info),
                        CodeBlockKind::Indented => None,
                    },
                    text: String::new(),
                });
            }
            Tag::HtmlBlock => self.html_block = Some((String::new(), offset)),
            Tag::List(Some(first)) if first != 1 => self.open("ol", vec![("start", first.to_string())]),
            Tag::List(Some(_)) => self.open("ol", Vec::new()),
            Tag::List(None) => self.open("ul", Vec::new()),
            Tag::Item => self.open("li", Vec::new()),
            // Not enabled — reachable only if the options above change.
            Tag::FootnoteDefinition(_) => self.open("div", Vec::new()),
            Tag::DefinitionList => self.open("dl", Vec::new()),
            Tag::DefinitionListTitle => self.open("dt", Vec::new()),
            Tag::DefinitionListDefinition => self.open("dd", Vec::new()),
            Tag::Table(alignments) => {
                self.table = Some(Table {
                    alignments,
                    in_head: false,
                    body_open: false,
                    cell: 0,
                });
                self.open("table", Vec::new());
            }
            // The head holds its cells directly; the parser sends no row for it.
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_head = true;
                    table.cell = 0;
                }
                self.open("thead", Vec::new());
                self.open("tr", Vec::new());
            }
            Tag::TableRow => {
                let open_body = self.table.as_mut().is_some_and(|table| {
                    table.cell = 0;
                    !std::mem::replace(&mut table.body_open, true)
                });
                if open_body {
                    self.open("tbody", Vec::new());
                }
                self.open("tr", Vec::new());
            }
            Tag::TableCell => {
                let (tag, align) = match self.table.as_mut() {
                    Some(table) => {
                        let align = table.alignments.get(table.cell).copied();
                        table.cell += 1;
                        (if table.in_head { "th" } else { "td" }, align)
                    }
                    None => ("td", None),
                };
                let attrs = match align {
                    Some(Alignment::Left) => vec![("style", r#"{ textAlign: "left" }"#.to_string())],
                    Some(Alignment::Center) => {
                        vec![("style", r#"{ textAlign: "center" }"#.to_string())]
                    }
                    Some(Alignment::Right) => {
                        vec![("style", r#"{ textAlign: "right" }"#.to_string())]
                    }
                    Some(Alignment::None) | None => Vec::new(),
                };
                self.open(tag, attrs);
            }
            Tag::Emphasis => self.open("em", Vec::new()),
            Tag::Strong => self.open("strong", Vec::new()),
            Tag::Strikethrough => self.open("del", Vec::new()),
            Tag::Superscript => self.open("sup", Vec::new()),
            Tag::Subscript => self.open("sub", Vec::new()),
            Tag::Link {
                link_type,
                dest_url,
                title,
                ..
            } => {
                if let Some(heading) = self.heading.as_mut() {
                    heading.has_link = true;
                }
                let href = if link_type == LinkType::Email {
                    format!("mailto:{dest_url}")
                } else {
                    self.resolve_href(&dest_url, offset)?
                };
                let mut attrs = vec![("href", js_string(&href))];
                if !title.is_empty() {
                    attrs.push(("title", js_string(&title)));
                }
                self.open("a", attrs);
            }
            Tag::Image {
                dest_url, title, ..
            } => {
                self.image = Some(Image {
                    src: dest_url.to_string(),
                    title: title.to_string(),
                    alt: String::new(),
                    depth: 0,
                });
            }
            Tag::MetadataBlock(_) => self.metadata = Some((String::new(), offset)),
        }
        Ok(())
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => self.close_heading(),
            TagEnd::TableHead => {
                self.close(); // tr
                self.close(); // thead
                if let Some(table) = self.table.as_mut() {
                    table.in_head = false;
                }
            }
            TagEnd::Table => {
                if self.table.take().is_some_and(|table| table.body_open) {
                    self.close(); // tbody
                }
                let table = self.pop();
                let rendered = element(table.tag, &table.attrs, &table.children);
                self.push(element(
                    "div",
                    &[("className", js_string("md-table"))],
                    &rendered,
                ));
            }
            // Each of these opened no element — its text went to a buffer, which
            // the matching arm in `event` closes before this is ever reached.
            TagEnd::CodeBlock | TagEnd::HtmlBlock | TagEnd::MetadataBlock(_) | TagEnd::Image => {}
            _ => self.close(),
        }
    }

    fn open(&mut self, tag: &'static str, attrs: Vec<(&'static str, String)>) {
        self.flush_text();
        self.stack.push(Open {
            tag,
            attrs,
            children: String::new(),
        });
    }

    fn pop(&mut self) -> Open {
        self.flush_text();
        // The root `<article>` is never closed by an event — the parser balances
        // every start with an end — so there is always one below the element.
        debug_assert!(self.stack.len() > 1, "closed the page's root element");
        self.stack.pop().expect("an open element")
    }

    fn close(&mut self) {
        let open = self.pop();
        self.push(element(open.tag, &open.attrs, &open.children));
    }

    fn push(&mut self, jsx: String) {
        self.flush_text();
        self.append(&jsx);
    }

    fn append(&mut self, jsx: &str) {
        self.stack
            .last_mut()
            .expect("the page's root element is always open")
            .children
            .push_str(jsx);
    }

    fn flush_text(&mut self) {
        if self.pending_text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending_text);
        self.append(&text_child(&text));
    }

    fn text(&mut self, text: &str) {
        if let Some(heading) = self.heading.as_mut() {
            heading.text.push_str(text);
        }
        self.pending_text.push_str(text);
    }

    fn close_heading(&mut self) {
        let mut open = self.pop();
        let Some(heading) = self.heading.take() else {
            self.push(element(open.tag, &open.attrs, &open.children));
            return;
        };
        let text = collapse_whitespace(&heading.text);
        let id = self.claim_id(heading.id, &text);

        if heading.rank == 1 && self.first_h1.is_none() {
            self.first_h1 = Some(text.clone());
        }
        if matches!(heading.rank, 2 | 3) {
            self.contents.push(ContentsEntry {
                rank: heading.rank,
                id: id.clone(),
                text,
            });
        }

        let children = if heading.rank >= 2 && !heading.has_link {
            element("a", &[("href", js_string(&format!("#{id}")))], &open.children)
        } else {
            std::mem::take(&mut open.children)
        };
        open.attrs.insert(0, ("id", js_string(&id)));
        self.push(element(open.tag, &open.attrs, &children));
    }

    /// The heading's id: the one written as `{#id}`, or its slug. A slug already
    /// taken on this page gets the lowest free `-N`; a written id is used as
    /// written even if a slug took it first, because the author asked for it.
    fn claim_id(&mut self, written: Option<String>, text: &str) -> String {
        if let Some(id) = written {
            self.ids.insert(id.clone());
            return id;
        }
        let base = match slug(text) {
            base if base.is_empty() => "section".to_string(),
            base => base,
        };
        let mut candidate = base.clone();
        let mut n = 0;
        while self.ids.contains(&candidate) {
            n += 1;
            candidate = format!("{base}-{n}");
        }
        self.ids.insert(candidate.clone());
        candidate
    }

    fn close_code(&mut self) {
        let Some(code) = self.code.take() else {
            return;
        };
        // The parser ends a block's text with the newline before the closing fence;
        // kept, it renders as a blank last line inside `<pre>`.
        let text = code.text.strip_suffix('\n').unwrap_or(&code.text);
        let mut attrs = vec![("className", js_string("md-code"))];
        if let Some(language) = &code.language {
            attrs.push(("data-lang", js_string(language)));
        }
        self.push(element("pre", &attrs, &element("code", &[], &text_child(text))));
    }

    fn close_image(&mut self) {
        let Some(image) = self.image.take() else {
            return;
        };
        let mut attrs = vec![("src", js_string(&image.src)), ("alt", js_string(&image.alt))];
        if !image.title.is_empty() {
            attrs.push(("title", js_string(&image.title)));
        }
        self.push(element("img", &attrs, ""));
    }

    fn close_html_block(&mut self) -> Result<(), MarkdownError> {
        let Some((html, offset)) = self.html_block.take() else {
            return Ok(());
        };
        if is_only_comments(&html) {
            return Ok(());
        }
        Err(self.error(offset, RAW_HTML_REFUSED))
    }

    fn close_metadata(&mut self) -> Result<(), MarkdownError> {
        let Some((raw, offset)) = self.metadata.take() else {
            return Ok(());
        };
        // The block starts on its opening `---`; its first key is the line after.
        let first_line = self.line_of(offset) + 1;
        for (index, line) in raw.lines().enumerate() {
            let line_number = first_line + index;
            let fail = |message: String| Err(error_at(self.file, line_number, message));
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((key, value)) = trimmed.split_once(':') else {
                return fail(format!("frontmatter lines read `key: value`; `{trimmed}` does not."));
            };
            let key = key.trim();
            let Some(value) = unquote(value.trim()) else {
                return fail(format!("the quoted value of `{key}` does not close."));
            };
            if value.is_empty() {
                return fail(format!("`{key}` has no value."));
            }
            let repeated = match key {
                "title" => self.frontmatter.title.replace(value).is_some(),
                "description" => self.frontmatter.description.replace(value).is_some(),
                "toc" => {
                    let show = match value.as_str() {
                        "true" => true,
                        "false" => false,
                        _ => return fail(format!("`toc` is `true` or `false`, not `{value}`.")),
                    };
                    self.frontmatter.toc.replace(show).is_some()
                }
                _ => {
                    return fail(format!(
                        "unknown frontmatter key `{key}`. A Markdown page reads `title`, \
                         `description` and `toc`."
                    ))
                }
            };
            if repeated {
                return fail(format!("`{key}` is set twice."));
            }
        }
        Ok(())
    }

    /// A relative link to a `.md` file becomes that page's route; anything else is
    /// left exactly as written.
    fn resolve_href(&self, dest: &str, offset: usize) -> Result<String, MarkdownError> {
        if dest.is_empty() || dest.starts_with('#') || dest.starts_with('/') || has_scheme(dest) {
            return Ok(dest.to_string());
        }
        let split = dest.find(|ch| ch == '?' || ch == '#').unwrap_or(dest.len());
        let (target, suffix) = dest.split_at(split);
        let links_to_markdown = Path::new(target)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case(MARKDOWN_EXTENSION));
        if !links_to_markdown {
            return Ok(dest.to_string());
        }

        let resolved = self.path.parent().unwrap_or_else(|| Path::new("")).join(target);
        if !resolved.is_file() {
            return Err(self.error(offset, format!("links to `{dest}`, which does not exist.")));
        }
        let normalized = normalize_specifier(&resolved);
        let route = is_markdown_page(Path::new(&normalized))
            .then(|| route_path_from_component(&normalized))
            .flatten()
            .filter(|route| !route.contains('['));
        match route {
            Some(route) => Ok(format!("{route}{suffix}")),
            None => Err(self.error(
                offset,
                format!(
                    "links to `{dest}`, which is not a page: a page is a `.md` file under \
                     `routes/` whose name does not start with `_`."
                ),
            )),
        }
    }

    fn contents_list(&self) -> Option<String> {
        let wanted = match self.frontmatter.toc {
            Some(false) => return None,
            Some(true) => 1,
            None => 2,
        };
        if self.contents.len() < wanted {
            return None;
        }

        // `###` nests under the `##` before it; one before any `##` is top level.
        let mut list = String::from("<ol>");
        let mut item_open = false;
        let mut sublist_open = false;
        for entry in &self.contents {
            let link = element(
                "a",
                &[("href", js_string(&format!("#{}", entry.id)))],
                &text_child(&entry.text),
            );
            if entry.rank == 3 && item_open {
                if !sublist_open {
                    list.push_str("<ol>");
                    sublist_open = true;
                }
                list.push_str(&element("li", &[], &link));
                continue;
            }
            if sublist_open {
                list.push_str("</ol>");
                sublist_open = false;
            }
            if item_open {
                list.push_str("</li>");
            }
            list.push_str("<li>");
            list.push_str(&link);
            item_open = true;
        }
        if sublist_open {
            list.push_str("</ol>");
        }
        if item_open {
            list.push_str("</li>");
        }
        list.push_str("</ol>");

        Some(element(
            "nav",
            &[
                ("className", js_string("md-toc")),
                ("aria-label", js_string("On this page")),
            ],
            &list,
        ))
    }

    fn finish(mut self, route: &str) -> Result<String, MarkdownError> {
        let article = self.pop_root();
        let mut page = element(article.tag, &article.attrs, &article.children);
        if let Some(contents) = self.contents_list() {
            page.push_str(&contents);
        }
        let page = element("div", &[("className", js_string("md-page"))], &page);

        let title = self.frontmatter.title.take().or_else(|| self.first_h1.take());
        let mut metadata = Vec::new();
        if let Some(title) = title {
            metadata.push(format!("title: {}", js_string(&title)));
        }
        if let Some(description) = &self.frontmatter.description {
            metadata.push(format!("description: {}", js_string(description)));
        }

        // Project-relative in the header, never the absolute path: this module is
        // carried into build artifacts, and an absolute path there discloses the
        // author's directory layout to anyone holding the binary.
        let origin = self
            .file
            .split_once("/routes/")
            .map(|(_, tail)| format!("routes/{tail}"))
            .unwrap_or_else(|| self.file.to_string());
        let mut module = format!(
            "// Lowered from {origin} by ALB'DO (src/markdown.rs). Edit the Markdown: this \
             module is rebuilt on every build and never written to the project.\n"
        );
        if !metadata.is_empty() {
            module.push_str(&format!("export const metadata = {{ {} }};\n", metadata.join(", ")));
        }
        module.push_str(&format!(
            "export default function {}() {{\n  return {page};\n}}\n",
            component_name(route)
        ));
        Ok(module)
    }

    fn pop_root(&mut self) -> Open {
        self.flush_text();
        // Every element the parser opened it also closed, so only the root is left.
        debug_assert_eq!(self.stack.len(), 1, "unbalanced Markdown lowering");
        self.stack.swap_remove(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn page(rel: &str, source: &str) -> Result<String, MarkdownError> {
        compile(&PathBuf::from(format!("src/routes/{rel}")), source)
    }

    fn body(rel: &str, source: &str) -> String {
        page(rel, source).expect("the page lowers")
    }

    #[test]
    fn a_page_is_a_default_export_named_after_its_route() {
        let module = body("docs/getting-started.md", "# Getting started\n\nHello.\n");
        assert!(module.contains("export default function MdDocsGettingStarted()"), "{module}");
        assert!(body("index.md", "Hi").contains("function MdIndex()"));
        assert!(body("docs/index.md", "Hi").contains("function MdDocs()"));
    }

    #[test]
    fn a_route_that_loses_characters_is_made_unique_by_its_hash() {
        let accented = component_name("/docs/über");
        let plain = component_name("/docs/ber");
        assert_ne!(accented, plain);
        assert!(accented.starts_with("MdDocsBer_"), "{accented}");
    }

    #[test]
    fn text_is_a_string_literal_so_the_evaluator_escapes_it() {
        let module = body("x.md", "a < b & {c} \"d\"\n");
        assert!(module.contains(r#"<p>{"a < b & {c} \"d\""}</p>"#), "{module}");
    }

    #[test]
    fn frontmatter_becomes_metadata_and_the_title_falls_back_to_the_first_h1() {
        let with = body(
            "x.md",
            "---\ntitle: \"Routing: the basics\"\ndescription: Files become routes.\n---\n# Heading\n",
        );
        assert!(
            with.contains(r#"export const metadata = { title: "Routing: the basics", description: "Files become routes." };"#),
            "{with}"
        );
        let without = body("x.md", "# From the heading\n\nText.\n");
        assert!(without.contains(r#"title: "From the heading""#), "{without}");
    }

    #[test]
    fn an_unknown_frontmatter_key_is_refused_on_its_line() {
        let err = page("x.md", "---\ntitle: A\nauthor: me\n---\nText\n").unwrap_err();
        assert_eq!(err.line, 3, "{err}");
        assert!(err.message.contains("unknown frontmatter key `author`"), "{err}");
    }

    #[test]
    fn raw_html_is_refused_on_its_line_and_comments_are_dropped() {
        let err = page("x.md", "Intro\n\n<div>raw</div>\n").unwrap_err();
        assert_eq!(err.line, 3, "{err}");
        assert!(err.message.starts_with("raw HTML"), "{err}");

        let inline = page("x.md", "Press <kbd>K</kbd>\n").unwrap_err();
        assert_eq!(inline.line, 1);

        let module = body("x.md", "<!-- a note\nover two lines -->\n\nText <!-- inline --> here\n");
        assert!(!module.contains("note"), "{module}");
        assert!(module.contains(r#"{"Text  here"}"#), "{module}");
    }

    #[test]
    fn headings_get_github_ids_link_to_themselves_and_fill_the_contents_list() {
        let module = body(
            "x.md",
            "# Title\n\n## Two words\n\n### Nested `code`\n\n## Two words\n\n## Custom {#mine}\n",
        );
        assert!(module.contains(r#"<h1 id={"title"}>{"Title"}</h1>"#), "{module}");
        assert!(
            module.contains(r##"<h2 id={"two-words"}><a href={"#two-words"}>{"Two words"}</a></h2>"##),
            "{module}"
        );
        assert!(module.contains(r#"id={"two-words-1"}"#), "{module}");
        assert!(module.contains(r#"id={"mine"}"#), "{module}");
        assert!(module.contains(r#"id={"nested-code"}"#), "{module}");
        assert!(
            module.contains(r##"<nav className={"md-toc"} aria-label={"On this page"}><ol><li><a href={"#two-words"}>{"Two words"}</a><ol><li><a href={"#nested-code"}>{"Nested code"}</a></li></ol></li>"##),
            "{module}"
        );
    }

    #[test]
    fn the_contents_list_needs_two_headings_unless_frontmatter_says_otherwise() {
        assert!(!body("x.md", "## Only\n").contains("md-toc"));
        assert!(body("x.md", "---\ntoc: true\n---\n## Only\n").contains("md-toc"));
        assert!(!body("x.md", "---\ntoc: false\n---\n## A\n\n## B\n").contains("md-toc"));
    }

    #[test]
    fn a_heading_holding_a_link_is_not_wrapped_in_another() {
        let module = body("x.md", "## See [this](https://example.com)\n");
        assert_eq!(module.matches("<a ").count(), 1, "{module}");
    }

    #[test]
    fn code_blocks_keep_their_text_verbatim_and_their_language() {
        let module = body("x.md", "```tsx title=\"x\"\nconst a = <b>{c}</b>;\n```\n");
        assert!(
            module.contains(r#"<pre className={"md-code"} data-lang={"tsx"}><code>{"const a = <b>{c}</b>;"}</code></pre>"#),
            "{module}"
        );
        let crlf = body("x.md", "```\r\nline one\r\nline two\r\n```\r\n");
        assert!(crlf.contains(r#"{"line one\nline two"}"#), "{crlf}");
    }

    #[test]
    fn github_alerts_become_labelled_callouts() {
        let module = body("x.md", "> [!WARNING]\n> Mind the gap.\n");
        assert!(
            module.contains(r#"<blockquote className={"md-callout"} data-kind={"warning"}><p className={"md-callout-label"}>{"Warning"}</p>"#),
            "{module}"
        );
    }

    #[test]
    fn tables_are_wrapped_and_aligned() {
        let module = body("x.md", "| a | b |\n|:--|--:|\n| 1 | 2 |\n");
        assert!(module.contains(r#"<div className={"md-table"}><table><thead><tr><th style={{ textAlign: "left" }}>{"a"}</th>"#), "{module}");
        assert!(module.contains(r#"<tbody><tr><td style={{ textAlign: "left" }}>{"1"}</td><td style={{ textAlign: "right" }}>{"2"}</td></tr></tbody></table></div>"#), "{module}");
    }

    #[test]
    fn images_carry_their_alt_text_and_task_lists_their_state() {
        let module = body("x.md", "![A *diagram*](/d.png \"Fig\")\n\n- [x] done\n- [ ] not\n");
        assert!(module.contains(r#"<img src={"/d.png"} alt={"A diagram"} title={"Fig"} />"#), "{module}");
        assert!(module.contains(r#"<input type={"checkbox"} disabled checked />"#), "{module}");
    }

    #[test]
    fn relative_page_links_become_routes_and_dead_ones_fail_the_build() {
        let root = tempfile::tempdir().expect("tempdir");
        let docs = root.path().join("src/routes/docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("routing.md"), "# Routing\n").unwrap();
        std::fs::write(docs.join("index.md"), "# Docs\n").unwrap();
        std::fs::write(docs.join("_draft.md"), "# Draft\n").unwrap();
        let here = docs.join("start.md");

        let module = compile(
            &here,
            "[r](./routing.md#dynamic) [i](index.md) [ext](https://x.dev/a.md) [hash](#top)\n",
        )
        .unwrap();
        assert!(module.contains(r#"href={"/docs/routing#dynamic"}"#), "{module}");
        assert!(module.contains(r#"href={"/docs"}"#), "{module}");
        assert!(module.contains(r#"href={"https://x.dev/a.md"}"#), "{module}");
        assert!(module.contains(r##"href={"#top"}"##), "{module}");

        let dead = compile(&here, "Intro\n\nSee [gone](./gone.md).\n").unwrap_err();
        assert_eq!(dead.line, 3);
        assert!(dead.message.contains("does not exist"), "{dead}");

        let draft = compile(&here, "[d](./_draft.md)\n").unwrap_err();
        assert!(draft.message.contains("not a page"), "{draft}");
    }

    #[test]
    fn a_page_cannot_be_a_convention_file_or_a_dynamic_segment() {
        assert!(page("docs/layout.md", "x").unwrap_err().message.contains("layout.tsx"));
        assert!(page("docs/[slug].md", "x").unwrap_err().message.contains("dynamic segment"));
    }

    #[test]
    fn only_markdown_under_routes_is_a_page() {
        assert!(is_markdown_page(Path::new("src/routes/docs/a.md")));
        assert!(is_markdown_page(Path::new("C:/app/src/routes/a.md")));
        assert!(!is_markdown_page(Path::new("C:/app/src/routes/a.MD")));
        assert!(!is_markdown_page(Path::new("src/components/README.md")));
        assert!(!is_markdown_page(Path::new("src/routes/docs/_draft.md")));
        assert!(!is_markdown_page(Path::new("src/routes/docs/a.tsx")));
    }

    #[test]
    fn the_module_header_never_carries_an_absolute_path() {
        let module = compile(Path::new("C:/Users/someone/app/src/routes/docs/a.md"), "Hi").unwrap();
        assert!(module.starts_with("// Lowered from routes/docs/a.md "), "{module}");
        assert!(!module.contains("someone"), "{module}");
    }
}
