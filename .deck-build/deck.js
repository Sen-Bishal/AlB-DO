const pptxgen = require("pptxgenjs");
const { iconToBase64Png } = require("./icons.js");
const {
  FaWeightHanging, FaClock, FaCoins, FaFeatherPointed, FaBolt, FaShieldHalved,
  FaLayerGroup, FaCheckDouble, FaMicrochip, FaServer, FaRocket, FaBuilding,
  FaCloud, FaLeaf, FaChartLine, FaUserAstronaut, FaCode, FaInfinity,
  FaBrain, FaArrowsRotate, FaFlagCheckered, FaArrowRight, FaCircleNotch,
  FaLock, FaWandMagicSparkles,
} = require("react-icons/fa6");

// ----------------------------------------------------------------------------
// THEME — "Albedo" = reflectance. Obsidian dark with luminous solar light.
// ----------------------------------------------------------------------------
const BG      = "0A0B11";   // obsidian
const BG2     = "0E1018";   // panel base
const CARD    = "161925";   // card
const CARD2   = "1C2030";   // raised card
const LINE    = "2A2F42";   // hairline
const GOLD     = "F6B53D";  // solar primary
const GOLDBR  = "FFD06B";   // bright gold
const CYAN    = "5FE3CE";   // ice / reflected light
const CYANDK  = "2FB3A3";
const TEXT    = "F4F5FA";   // near white
const SUB     = "C3C7D6";   // soft
const MUTED   = "868CA3";   // muted
const RED      = "FF6B6B";

const HF = "Georgia";        // header font — character
const BF = "Calibri";        // body font — clean
const MF = "Consolas";       // mono accents

const W = 13.333, H = 7.5, M = 0.7;

const pres = new pptxgen();
pres.defineLayout({ name: "WIDE", width: W, height: H });
pres.layout = "WIDE";
pres.author = "AlB'DO";
pres.title = "AlB'DO — Pitch";

const shadow = () => ({ type: "outer", color: "000000", blur: 14, offset: 5, angle: 90, opacity: 0.45 });
const glow   = (c) => ({ type: "outer", color: c, blur: 22, offset: 0, angle: 0, opacity: 0.5 });

// luminous reflecting orb — the brand motif (an "albedo" sphere)
function orb(slide, cx, cy, r, color = GOLD) {
  const rings = [
    { f: 2.6, t: 93 }, { f: 2.0, t: 88 }, { f: 1.5, t: 80 },
    { f: 1.12, t: 66 }, { f: 0.78, t: 40 },
  ];
  rings.forEach(({ f, t }) => {
    const rr = r * f;
    slide.addShape(pres.shapes.OVAL, {
      x: cx - rr, y: cy - rr, w: rr * 2, h: rr * 2,
      fill: { color, transparency: t }, line: { type: "none" },
    });
  });
  // bright core
  slide.addShape(pres.shapes.OVAL, {
    x: cx - r * 0.5, y: cy - r * 0.5, w: r, h: r,
    fill: { color: GOLDBR, transparency: 8 }, line: { type: "none" },
  });
}

function bg(slide, withOrb = true) {
  slide.background = { color: BG };
  if (withOrb) orb(slide, W - 0.6, -0.4, 0.95);
}

// small footer brand bar
function foot(slide, n) {
  slide.addText([
    { text: "AlB", options: { color: TEXT, bold: true } },
    { text: "’", options: { color: GOLD, bold: true } },
    { text: "DO", options: { color: TEXT, bold: true } },
  ], { x: M, y: H - 0.46, w: 3, h: 0.3, fontFace: HF, fontSize: 10, charSpacing: 1, margin: 0 });
  slide.addText(`${String(n).padStart(2, "0")} / 10`, {
    x: W - 1.7, y: H - 0.46, w: 1.0, h: 0.3, align: "right",
    fontFace: MF, fontSize: 9, color: MUTED, margin: 0,
  });
}

function eyebrow(slide, txt, x = M, y = M, color = GOLD) {
  slide.addText(txt.toUpperCase(), {
    x, y, w: 8, h: 0.3, fontFace: MF, fontSize: 11, bold: true,
    color, charSpacing: 3, margin: 0,
  });
}

function title(slide, runs, x = M, y = 1.02, w = 11.9, size = 38) {
  slide.addText(runs, {
    x, y, w, h: 1.0, fontFace: HF, fontSize: size, bold: true,
    color: TEXT, lineSpacing: size * 1.05, margin: 0,
  });
}

const ICONS = {};
async function loadIcons() {
  const map = {
    weight: [FaWeightHanging, GOLD], clock: [FaClock, GOLD], coins: [FaCoins, GOLD],
    feather: [FaFeatherPointed, GOLD], bolt: [FaBolt, GOLD], shield: [FaShieldHalved, CYAN],
    layers: [FaLayerGroup, GOLD], checks: [FaCheckDouble, CYAN], chip: [FaMicrochip, GOLD],
    server: [FaServer, CYAN], rocket: [FaRocket, GOLD], building: [FaBuilding, GOLD],
    cloud: [FaCloud, CYAN], leaf: [FaLeaf, CYAN], chart: [FaChartLine, GOLD],
    astro: [FaUserAstronaut, GOLD], code: [FaCode, CYAN], infinity: [FaInfinity, CYAN],
    brain: [FaBrain, GOLD], sync: [FaArrowsRotate, CYAN], flag: [FaFlagCheckered, GOLD],
    arrow: [FaArrowRight, GOLD], notch: [FaCircleNotch, CYAN], lock: [FaLock, CYAN],
    magic: [FaWandMagicSparkles, GOLD],
    arrowDark: [FaArrowRight, BG],
  };
  for (const [k, [C, col]] of Object.entries(map)) ICONS[k] = await iconToBase64Png(C, "#" + col, 256);
}

// icon inside a soft circle
function iconChip(slide, key, x, y, d = 0.62, ring = GOLD) {
  slide.addShape(pres.shapes.OVAL, {
    x, y, w: d, h: d, fill: { color: CARD2 }, line: { color: ring, width: 1 },
  });
  const pad = d * 0.27;
  slide.addImage({ data: ICONS[key], x: x + pad, y: y + pad, w: d - pad * 2, h: d - pad * 2 });
}

// =====================================================================
// SLIDE 1 — COVER
// =====================================================================
function slide1() {
  const s = pres.addSlide();
  s.background = { color: BG };
  // big reflecting sphere on the right — the hero motif
  orb(s, 10.7, 3.6, 1.7);
  // faint grid-light streaks
  s.addShape(pres.shapes.LINE, { x: 0, y: 5.55, w: W, h: 0, line: { color: LINE, width: 1 } });

  s.addText("THE RUST-NATIVE REACT RUNTIME", {
    x: M, y: 1.35, w: 9, h: 0.35, fontFace: MF, fontSize: 13, bold: true,
    color: GOLD, charSpacing: 4, margin: 0,
  });

  s.addText([
    { text: "AlB", options: { color: TEXT } },
    { text: "’", options: { color: GOLD } },
    { text: "DO", options: { color: TEXT } },
  ], { x: M - 0.05, y: 1.85, w: 10, h: 1.9, fontFace: HF, fontSize: 130, bold: true, charSpacing: 1, margin: 0 });

  s.addText("Write the React you already know.  Ship almost no JavaScript.", {
    x: M, y: 3.95, w: 9.2, h: 0.5, fontFace: HF, italic: true, fontSize: 23, color: GOLDBR, margin: 0,
  });

  s.addText(
    "A compiler that proves which parts of your app are alive — and ships only those. The rest arrives as finished light.",
    { x: M, y: 4.62, w: 8.6, h: 0.8, fontFace: BF, fontSize: 15, color: SUB, lineSpacing: 22, margin: 0 }
  );

  s.addText("INVESTOR BRIEF · CONFIDENTIAL", {
    x: M, y: 5.75, w: 6, h: 0.3, fontFace: MF, fontSize: 10, color: MUTED, charSpacing: 2, margin: 0,
  });
  s.addText("/ albedo — the fraction of light a surface reflects /", {
    x: W - 6.7, y: 5.75, w: 6, h: 0.3, align: "right", fontFace: MF, italic: true, fontSize: 10, color: MUTED, margin: 0,
  });
}

// =====================================================================
// SLIDE 2 — THE PROBLEM
// =====================================================================
function slide2() {
  const s = pres.addSlide();
  bg(s);
  eyebrow(s, "The problem");
  title(s, [
    { text: "The web learned to ship the ", options: {} },
    { text: "engine", options: { color: GOLD, italic: true } },
    { text: ",", options: {} },
    { text: "\nnot just the car", options: {} },
  ], M, 1.0, 11.5, 36);

  s.addText(
    "Every modern React/Next app builds your UI twice — once as HTML on the server, then again as a megabyte of JavaScript that re-runs in the browser to “hydrate” what was already drawn. Users pay for it in seconds; companies pay for it in cloud bills.",
    { x: M, y: 2.35, w: 7.0, h: 1.4, fontFace: BF, fontSize: 15.5, color: SUB, lineSpacing: 24, margin: 0 }
  );

  const cards = [
    ["weight", "~90%", "of shipped JavaScript is framework & component code the visible page never needed."],
    ["clock", "2–6s", "to interactive on real mobile devices — hydration blocks the main thread, frame by frame."],
    ["coins", "+1% lost", "in conversion for every 100ms of added latency. Slowness is a line item."],
  ];
  const cw = 3.78, gap = 0.32, x0 = M, y0 = 4.05, ch = 2.45;
  cards.forEach(([ic, big, txt], i) => {
    const x = x0 + i * (cw + gap);
    s.addShape(pres.shapes.RECTANGLE, { x, y: y0, w: cw, h: ch, fill: { color: CARD }, line: { color: LINE, width: 1 }, shadow: shadow() });
    s.addShape(pres.shapes.RECTANGLE, { x, y: y0, w: cw, h: 0.07, fill: { color: GOLD }, line: { type: "none" } });
    iconChip(s, ic, x + 0.3, y0 + 0.32);
    s.addText(big, { x: x + 0.3, y: y0 + 1.02, w: cw - 0.6, h: 0.7, fontFace: HF, bold: true, fontSize: 34, color: GOLDBR, margin: 0 });
    s.addText(txt, { x: x + 0.3, y: y0 + 1.72, w: cw - 0.6, h: 0.65, fontFace: BF, fontSize: 12.5, color: SUB, lineSpacing: 17, margin: 0 });
  });

  // right rail — "paying twice" motif (sits beside the intro paragraph)
  const rx = 8.45;
  s.addShape(pres.shapes.RECTANGLE, { x: rx - 0.3, y: 2.2, w: W - M - rx + 0.3, h: 1.55, fill: { color: BG2 }, line: { color: LINE, width: 1 } });
  s.addText("Paying twice for one page", {
    x: rx, y: 2.4, w: 4.0, h: 0.35, fontFace: HF, italic: true, fontSize: 15, color: TEXT, margin: 0,
  });
  ["Render on server", "Send HTML", "Re-send the whole framework", "Re-run it to “hydrate”"].forEach((t, i) => {
    const yy = 2.88 + i * 0.2;
    s.addShape(pres.shapes.OVAL, { x: rx, y: yy + 0.03, w: 0.12, h: 0.12, fill: { color: i >= 2 ? RED : MUTED }, line: { type: "none" } });
    s.addText(t, { x: rx + 0.25, y: yy - 0.05, w: 3.7, h: 0.3, fontFace: BF, fontSize: 11, color: i >= 2 ? TEXT : MUTED, margin: 0 });
  });
  foot(s, 2);
}

// =====================================================================
// SLIDE 3 — THE SOLUTION
// =====================================================================
function slide3() {
  const s = pres.addSlide();
  bg(s);
  eyebrow(s, "The solution");
  title(s, [
    { text: "AlB", options: { color: TEXT } },
    { text: "’", options: { color: GOLD } },
    { text: "DO ships the ", options: { color: TEXT } },
    { text: "app", options: { color: GOLD, italic: true } },
    { text: " — never the engine", options: { color: TEXT } },
  ], M, 1.0, 12, 36);

  s.addText(
    "A Rust-native compiler reads your React and proves, component by component, what is truly static, what is data-bound, and what is genuinely interactive. Static and data-bound parts ship as finished HTML. Only real interactivity ships code — surgically.",
    { x: M, y: 1.95, w: 11.9, h: 0.9, fontFace: BF, fontSize: 16, color: SUB, lineSpacing: 24, margin: 0 }
  );

  // three pillars
  const pillars = [
    ["feather", "Same code", "Author in ordinary React/TSX. No new mental model, no annotations, no rewrite."],
    ["shield", "Provably correct", "If it can’t prove the cheaper path is safe, it falls back. A broken UI is impossible."],
    ["bolt", "A fraction of the weight", "Most pages reach interactive with kilobytes — or zero — of client JavaScript."],
  ];
  const cw = 3.84, gap = 0.3, y0 = 3.1, ch = 1.85;
  pillars.forEach(([ic, hd, tx], i) => {
    const x = M + i * (cw + gap);
    s.addShape(pres.shapes.RECTANGLE, { x, y: y0, w: cw, h: ch, fill: { color: CARD }, line: { color: LINE, width: 1 }, shadow: shadow() });
    iconChip(s, ic, x + 0.3, y0 + 0.3, 0.6, CYAN);
    s.addText(hd, { x: x + 1.05, y: y0 + 0.34, w: cw - 1.2, h: 0.5, fontFace: HF, bold: true, fontSize: 18, color: TEXT, margin: 0, valign: "middle" });
    s.addText(tx, { x: x + 0.3, y: y0 + 1.05, w: cw - 0.6, h: 0.7, fontFace: BF, fontSize: 12.5, color: SUB, lineSpacing: 17, margin: 0 });
  });

  // before / after bytes-to-interactive bar
  const by = 5.35;
  s.addText("BYTES TO INTERACTIVE — TYPICAL CONTENT PAGE", {
    x: M, y: by, w: 8, h: 0.3, fontFace: MF, fontSize: 10.5, bold: true, color: MUTED, charSpacing: 2, margin: 0,
  });
  const barX = M, barW = 11.9;
  // Next.js bar
  s.addShape(pres.shapes.RECTANGLE, { x: barX, y: by + 0.45, w: barW, h: 0.42, fill: { color: CARD2 }, line: { type: "none" } });
  s.addShape(pres.shapes.RECTANGLE, { x: barX, y: by + 0.45, w: barW, h: 0.42, fill: { color: RED, transparency: 30 }, line: { type: "none" } });
  s.addText("Conventional React/Next  ·  ~480 KB JS", { x: barX + 0.15, y: by + 0.45, w: barW - 0.3, h: 0.42, fontFace: BF, bold: true, fontSize: 12, color: TEXT, valign: "middle", margin: 0 });
  // AlB'DO bar
  s.addShape(pres.shapes.RECTANGLE, { x: barX, y: by + 0.98, w: barW, h: 0.42, fill: { color: CARD2 }, line: { type: "none" } });
  s.addShape(pres.shapes.RECTANGLE, { x: barX, y: by + 0.98, w: barW * 0.045, h: 0.42, fill: { color: GOLD }, line: { type: "none" } });
  s.addText([
    { text: "AlB’DO", options: { bold: true, color: GOLDBR } },
    { text: "   ·   ~0–18 KB JS", options: { bold: true, color: SUB } },
  ], { x: barX + barW * 0.045 + 0.2, y: by + 0.98, w: barW - 0.3, h: 0.42, fontFace: BF, fontSize: 12, valign: "middle", margin: 0 });
  foot(s, 3);
}

// =====================================================================
// SLIDE 4 — CORE FEATURE 1: SOUNDNESS ENGINE
// =====================================================================
function slide4() {
  const s = pres.addSlide();
  bg(s);
  eyebrow(s, "Core feature · 01");
  title(s, [
    { text: "Correct ", options: {} },
    { text: "by construction", options: { color: GOLD, italic: true } },
  ], M, 1.0, 11, 38);
  s.addText(
    "At the heart of AlB’DO is a soundness lattice — a static dataflow analysis that, for every component, computes the complete set of provably-correct ways to ship it. The framework only ever chooses from inside that set.",
    { x: M, y: 1.95, w: 7.1, h: 1.1, fontFace: BF, fontSize: 15.5, color: SUB, lineSpacing: 23, margin: 0 }
  );

  // the three tiers as a ladder of cards (left column)
  const tiers = [
    ["A", "feather", "Tier A — Pure", "Provably static. Renders to plain HTML. Zero client code, forever.", GOLD],
    ["B", "layers", "Tier B — Bound", "Server-rendered & data-bound. Updates by surgical patch, not by re-running the app.", CYAN],
    ["C", "bolt", "Tier C — Live", "Genuinely interactive. Gets a real runtime — and only this tier does.", GOLDBR],
  ];
  const ty = 3.2, th = 1.18, tw = 7.1;
  tiers.forEach(([badge, ic, hd, tx, col], i) => {
    const y = ty + i * (th + 0.16);
    s.addShape(pres.shapes.RECTANGLE, { x: M, y, w: tw, h: th, fill: { color: CARD }, line: { color: LINE, width: 1 } });
    s.addShape(pres.shapes.RECTANGLE, { x: M, y, w: 0.09, h: th, fill: { color: col }, line: { type: "none" } });
    s.addShape(pres.shapes.OVAL, { x: M + 0.32, y: y + 0.3, w: 0.58, h: 0.58, fill: { color: BG2 }, line: { color: col, width: 1.5 } });
    s.addText(badge, { x: M + 0.32, y: y + 0.3, w: 0.58, h: 0.58, align: "center", valign: "middle", fontFace: HF, bold: true, fontSize: 22, color: col, margin: 0 });
    s.addText(hd, { x: M + 1.12, y: y + 0.2, w: tw - 1.3, h: 0.4, fontFace: HF, bold: true, fontSize: 17, color: TEXT, margin: 0 });
    s.addText(tx, { x: M + 1.12, y: y + 0.58, w: tw - 1.35, h: 0.55, fontFace: BF, fontSize: 12.5, color: SUB, lineSpacing: 16, margin: 0 });
  });

  // right column — the guarantee panel
  const px = M + tw + 0.35, pw = W - px - M;
  s.addShape(pres.shapes.RECTANGLE, { x: px, y: ty, w: pw, h: th * 3 + 0.32, fill: { color: BG2 }, line: { color: GOLD, width: 1 }, shadow: shadow() });
  iconChip(s, "checks", px + 0.4, ty + 0.4, 0.7, GOLD);
  s.addText("The line we never cross", { x: px + 0.4, y: ty + 1.25, w: pw - 0.8, h: 0.5, fontFace: HF, bold: true, fontSize: 19, color: GOLDBR, margin: 0 });
  s.addText(
    "A learned model may rank options. It may never invent one. The analysis is inviolable — so the worst a wrong choice can cost is a few milliseconds, never a broken app.",
    { x: px + 0.4, y: ty + 1.85, w: pw - 0.8, h: 1.2, fontFace: BF, fontSize: 13.5, color: SUB, lineSpacing: 20, margin: 0 }
  );
  s.addText("SILENT-WRONG IS IMPOSSIBLE.", { x: px + 0.4, y: ty + 3.05, w: pw - 0.8, h: 0.4, fontFace: MF, bold: true, fontSize: 12, color: CYAN, charSpacing: 1, margin: 0 });
  foot(s, 4);
}

// =====================================================================
// SLIDE 5 — CORE FEATURE 2: THE RUNTIME
// =====================================================================
function slide5() {
  const s = pres.addSlide();
  bg(s);
  eyebrow(s, "Core feature · 02");
  title(s, [
    { text: "An engine built for ", options: {} },
    { text: "this decade", options: { color: GOLD, italic: true } },
  ], M, 1.0, 11.5, 38);
  s.addText(
    "Underneath the elegance is a Rust core engineered to disappear: a per-request arena allocator with no garbage collector, embedded QuickJS for server rendering, and a near-zero client runtime. It is the fastest path from request to pixels.",
    { x: M, y: 1.95, w: 11.9, h: 0.85, fontFace: BF, fontSize: 15.5, color: SUB, lineSpacing: 23, margin: 0 }
  );

  // four big stat callouts
  const stats = [
    ["chip", "0–18 KB", "client JS on a typical page", GOLD],
    ["rocket", "<1 ms", "cold first render, warm pool", CYAN],
    ["server", "~0.24 ms", "server action round-trip, p50", GOLD],
    ["leaf", "Zero", "GC pauses — arena reset per request", CYAN],
  ];
  const cw = 2.86, gap = 0.28, y0 = 3.05, ch = 1.75;
  stats.forEach(([ic, big, lbl, col], i) => {
    const x = M + i * (cw + gap);
    s.addShape(pres.shapes.RECTANGLE, { x, y: y0, w: cw, h: ch, fill: { color: CARD }, line: { color: LINE, width: 1 }, shadow: shadow() });
    iconChip(s, ic, x + 0.28, y0 + 0.26, 0.54, col);
    s.addText(big, { x: x + 0.28, y: y0 + 0.86, w: cw - 0.5, h: 0.55, fontFace: HF, bold: true, fontSize: 30, color: col, margin: 0 });
    s.addText(lbl, { x: x + 0.28, y: y0 + 1.42, w: cw - 0.5, h: 0.32, fontFace: BF, fontSize: 11.5, color: SUB, margin: 0 });
  });

  // bottom — the feature pipeline strip
  const py = 5.2;
  s.addText("ONE PASS, REQUEST TO PIXELS", { x: M, y: py, w: 8, h: 0.3, fontFace: MF, fontSize: 10.5, bold: true, color: MUTED, charSpacing: 2, margin: 0 });
  const steps = ["Your React/TSX", "Rust analysis", "Tier + bind", "QuickJS render", "Finished HTML + surgical JS"];
  const sw = 2.18, sgap = 0.22, sy = py + 0.42;
  steps.forEach((t, i) => {
    const x = M + i * (sw + sgap);
    s.addShape(pres.shapes.RECTANGLE, { x, y: sy, w: sw, h: 0.7, fill: { color: CARD2 }, line: { color: LINE, width: 1 } });
    s.addText(t, { x: x + 0.1, y: sy, w: sw - 0.2, h: 0.7, align: "center", valign: "middle", fontFace: BF, bold: true, fontSize: 11.5, color: i === steps.length - 1 ? GOLDBR : TEXT, margin: 0 });
    if (i < steps.length - 1) s.addImage({ data: ICONS.arrow, x: x + sw + 0.01, y: sy + 0.26, w: 0.18, h: 0.18 });
  });
  foot(s, 5);
}

// =====================================================================
// SLIDE 6 — ENTERPRISE IMPACT
// =====================================================================
function slide6() {
  const s = pres.addSlide();
  bg(s);
  eyebrow(s, "Impact · enterprise");
  title(s, [
    { text: "At scale, the savings ", options: {} },
    { text: "compound", options: { color: GOLD, italic: true } },
  ], M, 1.0, 11.5, 36);
  s.addText(
    "Less JavaScript on the wire is less compute to render, less egress to bill, fewer servers to run, and faster pages that convert. The same product, on a materially smaller infrastructure footprint.",
    { x: M, y: 1.9, w: 7.1, h: 1.0, fontFace: BF, fontSize: 15, color: SUB, lineSpacing: 22, margin: 0 }
  );

  // four impact stat cards (left grid 2x2)
  const cards = [
    ["coins", "Up to 60%", "lower render & edge compute spend", GOLD],
    ["cloud", "70–95%", "less client JavaScript egress", CYAN],
    ["building", "Fewer servers", "headroom reclaimed across the fleet", GOLD],
    ["leaf", "Lower carbon", "less compute is less energy, measurably", CYAN],
  ];
  const cw = 3.45, gap = 0.3, x0 = M, y0 = 3.05, ch = 1.55;
  cards.forEach(([ic, big, lbl, col], i) => {
    const x = x0 + (i % 2) * (cw + gap);
    const y = y0 + Math.floor(i / 2) * (ch + 0.25);
    s.addShape(pres.shapes.RECTANGLE, { x, y, w: cw, h: ch, fill: { color: CARD }, line: { color: LINE, width: 1 }, shadow: shadow() });
    iconChip(s, ic, x + 0.26, y + 0.26, 0.5, col);
    s.addText(big, { x: x + 0.9, y: y + 0.22, w: cw - 1.0, h: 0.5, fontFace: HF, bold: true, fontSize: 23, color: col, margin: 0, valign: "middle" });
    s.addText(lbl, { x: x + 0.26, y: y + 0.92, w: cw - 0.5, h: 0.5, fontFace: BF, fontSize: 12, color: SUB, lineSpacing: 16, margin: 0 });
  });

  // right — cost comparison chart
  const rx = x0 + 2 * (cw + gap) + 0.05, rw = W - rx - M;
  s.addShape(pres.shapes.RECTANGLE, { x: rx, y: y0, w: rw, h: ch * 2 + 0.25, fill: { color: BG2 }, line: { color: LINE, width: 1 }, shadow: shadow() });
  s.addText("ANNUAL FRONT-END CLOUD SPEND (INDEXED)", { x: rx + 0.3, y: y0 + 0.22, w: rw - 0.6, h: 0.3, fontFace: MF, fontSize: 10, bold: true, color: MUTED, charSpacing: 1, margin: 0 });
  s.addChart(pres.charts.BAR, [
    { name: "Spend", labels: ["Today's stack", "On AlB’DO"], values: [100, 42] },
  ], {
    x: rx + 0.15, y: y0 + 0.55, w: rw - 0.3, h: ch * 2 - 0.55, barDir: "col",
    chartColors: [RED, GOLD], chartColorsOpacity: [55, 100],
    showValue: true, dataLabelPosition: "outEnd", dataLabelColor: TEXT, dataLabelFontFace: HF, dataLabelFontSize: 16, dataLabelFontBold: true,
    catAxisLabelColor: SUB, catAxisLabelFontSize: 11, catAxisLabelFontBold: true,
    valAxisHidden: true, valGridLine: { style: "none" }, catGridLine: { style: "none" },
    showLegend: false, showTitle: false, barGapWidthPct: 60,
    plotArea: { fill: { color: BG2 } }, chartArea: { fill: { color: BG2 } },
  });
  foot(s, 6);
}

// =====================================================================
// SLIDE 7 — INDIE IMPACT
// =====================================================================
function slide7() {
  const s = pres.addSlide();
  bg(s);
  eyebrow(s, "Impact · indie & solo builders");
  title(s, [
    { text: "One developer. ", options: {} },
    { text: "Production-grade by default", options: { color: GOLD, italic: true } },
    { text: ".", options: {} },
  ], M, 1.0, 12, 34);
  s.addText(
    "The performance work that used to need a platform team is now the framework’s job. You write features; AlB’DO makes them fast. The gap between a side project and a serious product disappears.",
    { x: M, y: 1.92, w: 11.9, h: 0.85, fontFace: BF, fontSize: 15.5, color: SUB, lineSpacing: 23, margin: 0 }
  );

  const rows = [
    ["code", "Just write React", "No perf budget rituals, no manual code-splitting, no “use client” archaeology. Ship the feature."],
    ["rocket", "Elite performance, free", "Your weekend project loads like a flagship app — because the compiler does the hard part."],
    ["infinity", "Scale without a rewrite", "From first user to viral spike, the same code holds. No re-architecture tax when you grow."],
    ["coins", "Hosting that stays cheap", "Tiny payloads and a lean runtime mean a real product can live on a hobby-tier bill."],
  ];
  const y0 = 2.95, rh = 0.86, rw = 11.9;
  rows.forEach(([ic, hd, tx], i) => {
    const y = y0 + i * (rh + 0.12);
    s.addShape(pres.shapes.RECTANGLE, { x: M, y, w: rw, h: rh, fill: { color: i % 2 ? CARD : CARD2 }, line: { color: LINE, width: 1 } });
    iconChip(s, ic, M + 0.28, y + 0.2, 0.52, i % 2 ? GOLD : CYAN);
    s.addText(hd, { x: M + 1.05, y: y + 0.14, w: 3.4, h: rh - 0.28, fontFace: HF, bold: true, fontSize: 17, color: TEXT, valign: "middle", margin: 0 });
    s.addText(tx, { x: M + 4.5, y: y + 0.14, w: rw - 4.8, h: rh - 0.28, fontFace: BF, fontSize: 13, color: SUB, valign: "middle", lineSpacing: 17, margin: 0 });
  });
  foot(s, 7);
}

// =====================================================================
// SLIDE 8 — AI / SELF-LEARNING LOOP
// =====================================================================
function slide8() {
  const s = pres.addSlide();
  bg(s);
  eyebrow(s, "The moat · self-learning");
  title(s, [
    { text: "A framework that ", options: {} },
    { text: "learns your fleet", options: { color: GOLD, italic: true } },
  ], M, 1.0, 11.5, 36);
  s.addText(
    "AlB’DO separates correctness from optimization. The static analysis decides what is legal. A learned policy — the same architecture that powers ML-guided compilers — chooses the best legal option, and it gets sharper with every page you ship.",
    { x: M, y: 1.9, w: 11.9, h: 0.9, fontFace: BF, fontSize: 15.5, color: SUB, lineSpacing: 23, margin: 0 }
  );

  // two-layer contract panel (left)
  const lx = M, lw = 5.5, ly = 3.1, lh = 3.35;
  s.addShape(pres.shapes.RECTANGLE, { x: lx, y: ly, w: lw, h: lh, fill: { color: BG2 }, line: { color: LINE, width: 1 }, shadow: shadow() });
  // policy layer
  s.addShape(pres.shapes.RECTANGLE, { x: lx + 0.3, y: ly + 0.3, w: lw - 0.6, h: 1.25, fill: { color: CARD }, line: { color: GOLD, width: 1 } });
  iconChip(s, "brain", lx + 0.55, ly + 0.55, 0.55, GOLD);
  s.addText("Policy layer — learned", { x: lx + 1.3, y: ly + 0.5, w: lw - 1.6, h: 0.4, fontFace: HF, bold: true, fontSize: 15, color: GOLDBR, margin: 0 });
  s.addText("Ranks legal options to minimize wire bytes, client JS & latency. Can be wrong about speed — never about correctness.", { x: lx + 0.55, y: ly + 1.0, w: lw - 1.0, h: 0.55, fontFace: BF, fontSize: 11, color: SUB, lineSpacing: 14, margin: 0 });
  // arrow down
  s.addImage({ data: ICONS.lock, x: lx + lw / 2 - 0.16, y: ly + 1.68, w: 0.32, h: 0.32 });
  // soundness layer
  s.addShape(pres.shapes.RECTANGLE, { x: lx + 0.3, y: ly + 2.1, w: lw - 0.6, h: 1.0, fill: { color: CARD }, line: { color: CYAN, width: 1 } });
  iconChip(s, "shield", lx + 0.55, ly + 2.3, 0.55, CYAN);
  s.addText("Soundness lattice — inviolable", { x: lx + 1.3, y: ly + 2.28, w: lw - 1.6, h: 0.4, fontFace: HF, bold: true, fontSize: 15, color: CYAN, margin: 0 });
  s.addText("Static proof of every legal tier/binding. The policy may only choose from inside this set.", { x: lx + 0.55, y: ly + 2.72, w: lw - 1.0, h: 0.4, fontFace: BF, fontSize: 11, color: SUB, lineSpacing: 14, margin: 0 });

  // right — the loop
  const rx = lx + lw + 0.45, rw = W - rx - M;
  s.addText("The flywheel", { x: rx, y: ly - 0.02, w: rw, h: 0.4, fontFace: HF, italic: true, fontSize: 18, color: TEXT, margin: 0 });
  const loop = [
    ["sync", "Ship", "Every build emits the choices the policy made."],
    ["chart", "Measure", "Real render, bytes & latency stream back as signal."],
    ["brain", "Learn", "The policy re-ranks — fleet-wide, across every app."],
    ["bolt", "Improve", "Tomorrow’s build is faster than today’s. Automatically."],
  ];
  const sy = ly + 0.5, sh = 0.66;
  loop.forEach(([ic, hd, tx], i) => {
    const y = sy + i * (sh + 0.05);
    iconChip(s, ic, rx, y, 0.5, i % 2 ? CYAN : GOLD);
    s.addText([
      { text: hd + "   ", options: { bold: true, color: TEXT, fontFace: HF, fontSize: 14 } },
      { text: tx, options: { color: SUB, fontFace: BF, fontSize: 11.5 } },
    ], { x: rx + 0.66, y: y - 0.05, w: rw - 0.66, h: 0.6, valign: "middle", lineSpacing: 15, margin: 0 });
  });
  s.addText("The advantage AlB’DO accrues — better perf per watt, per build — competitors can’t copy. It’s earned from data they don’t have.", {
    x: rx, y: sy + 4 * (sh + 0.05) + 0.05, w: rw, h: 0.55, fontFace: BF, italic: true, fontSize: 12, color: GOLDBR, lineSpacing: 16, margin: 0,
  });
  foot(s, 8);
}

// =====================================================================
// SLIDE 9 — ROADMAP
// =====================================================================
function slide9() {
  const s = pres.addSlide();
  bg(s);
  eyebrow(s, "Where this goes");
  title(s, [
    { text: "From a faster framework to the ", options: {} },
    { text: "web’s substrate", options: { color: GOLD, italic: true } },
  ], M, 1.0, 12.2, 32);

  const phases = [
    ["I", "The runtime", "Rust core, soundness engine & near-zero client runtime — the fastest React you can write today.", GOLD],
    ["II", "The learning loop", "Fleet-scale policy that compounds: every app makes every other app faster.", CYAN],
    ["III", "Universal components", "Any component on npm — ShadCN, your own, AI-generated — runs natively, optimally, untouched.", GOLD],
    ["IV", "Edge-native streaming", "Columnar wire format & WebTransport: state streams to the edge, instantly, anywhere.", CYAN],
    ["V", "The default for AI-built apps", "When machines write the web, AlB’DO is the substrate that makes it correct and fast — by construction.", GOLDBR],
  ];
  const x0 = M, y0 = 2.35, cw = 2.3, gap = 0.18, ch = 4.0;
  phases.forEach(([num, hd, tx, col], i) => {
    const x = x0 + i * (cw + gap);
    s.addShape(pres.shapes.RECTANGLE, { x, y: y0, w: cw, h: ch, fill: { color: i === 4 ? BG2 : CARD }, line: { color: i === 4 ? GOLD : LINE, width: i === 4 ? 1.4 : 1 }, shadow: shadow() });
    s.addShape(pres.shapes.RECTANGLE, { x, y: y0, w: cw, h: 0.08, fill: { color: col }, line: { type: "none" } });
    s.addText(num, { x: x + 0.25, y: y0 + 0.28, w: cw - 0.5, h: 0.7, fontFace: HF, bold: true, fontSize: 40, color: col, margin: 0 });
    s.addText(hd, { x: x + 0.25, y: y0 + 1.05, w: cw - 0.45, h: 0.7, fontFace: HF, bold: true, fontSize: 15.5, color: TEXT, lineSpacing: 18, margin: 0 });
    s.addText(tx, { x: x + 0.25, y: y0 + 1.75, w: cw - 0.45, h: 2.1, fontFace: BF, fontSize: 11.5, color: SUB, lineSpacing: 16, margin: 0 });
  });
  // timeline arrow under
  s.addText("Each phase widens the market — and deepens a moat the data keeps refilling.", {
    x: M, y: y0 + ch + 0.18, w: 11.9, h: 0.35, fontFace: BF, italic: true, fontSize: 12.5, color: GOLDBR, margin: 0,
  });
  foot(s, 9);
}

// =====================================================================
// SLIDE 10 — CLOSE
// =====================================================================
function slide10() {
  const s = pres.addSlide();
  s.background = { color: BG };
  orb(s, 0.6, 7.6, 1.2);
  orb(s, 11.6, 1.0, 1.1);
  s.addShape(pres.shapes.LINE, { x: M, y: 2.0, w: W - 2 * M, h: 0, line: { color: LINE, width: 1 } });

  s.addText("THE WEB’S NEXT DEFAULT", {
    x: M, y: 1.45, w: 10, h: 0.35, fontFace: MF, fontSize: 13, bold: true, color: GOLD, charSpacing: 4, margin: 0,
  });
  s.addText([
    { text: "Write less.\n", options: { color: TEXT } },
    { text: "Ship light.", options: { color: GOLD } },
  ], { x: M - 0.03, y: 2.35, w: 11, h: 2.4, fontFace: HF, bold: true, fontSize: 76, lineSpacing: 78, margin: 0 });

  s.addText(
    "AlB’DO turns ordinary React into the fastest, leanest, provably-correct web — and learns from every page it serves. The framework gets better while you sleep.",
    { x: M, y: 4.95, w: 8.6, h: 0.9, fontFace: BF, fontSize: 16, color: SUB, lineSpacing: 24, margin: 0 }
  );

  // CTA chip
  s.addShape(pres.shapes.RECTANGLE, { x: M, y: 6.1, w: 4.2, h: 0.7, fill: { color: GOLD }, line: { type: "none" }, shadow: glow(GOLD) });
  s.addText("Let’s build the substrate.", { x: M, y: 6.1, w: 3.6, h: 0.7, align: "center", valign: "middle", fontFace: HF, bold: true, fontSize: 16, color: BG, margin: 0 });
  s.addImage({ data: ICONS.arrowDark, x: M + 3.55, y: 6.32, w: 0.26, h: 0.26 });

  s.addText("bishalsenpersonal@gmail.com", { x: W - 5.7, y: 6.3, w: 5, h: 0.4, align: "right", fontFace: MF, fontSize: 12, color: MUTED, margin: 0 });
}

(async () => {
  await loadIcons();
  slide1(); slide2(); slide3(); slide4(); slide5();
  slide6(); slide7(); slide8(); slide9(); slide10();
  await pres.writeFile({ fileName: "AlBDO_Pitch.pptx" });
  console.log("written");
})();
