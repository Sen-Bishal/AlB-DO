// A plain data module — no components. Before cross-module value resolution
// existed, every one of these rendered as the empty string.
export const brand = "ALBDO";
export const site = { tagline: "emits your backend" };
export const items = ["Alpha", "Beta"];

// A constant built from another constant IN THIS FILE. It must resolve in the
// target module's own scope, not the importer's.
const suffix = " lit";
export const derived = brand + suffix;
