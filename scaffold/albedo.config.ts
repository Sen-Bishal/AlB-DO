export default {
  contract_version: 1,
  root: "src",
  entry: "App.tsx",
  server: { host: "127.0.0.1", port: 3000 },
  watch: { debounce_ms: 75, ignore: ["**/.git/**", "**/node_modules/**"] },
  hmr: { enabled: true, transport: "sse" },
  hot_set: [],
  static_slice: { enabled: true, opt_out: [] },
};
