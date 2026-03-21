# albedo-node

`albedo-node` is the N-API bridge crate for ALBEDO.

It exposes three baseline APIs:

- `analyzeProject(path, options)`
- `optimizeManifest(manifest, options)`
- `getCacheStats()`

For stricter TypeScript contracts, import from `@albedo/node/typed`.

## Build and Verify

```bash
cd crates/albedo-node
npm install
npm run verify
```

`verify` runs:
- native bridge build (`napi build --platform --release`)
- runtime smoke test against `test-app/src/components`
- TypeScript contract checks (`tsc --noEmit`)

The build emits a native `.node` artifact in this directory for the local platform.

## Local Pack

```bash
cd crates/albedo-node
npm run pack:local
```

This creates an installable local tarball for integration testing.

## Release Flow

Use GitHub Actions release workflow:

1. Build and verify bridge package on Linux, macOS, and Windows.
2. Generate npm tarball artifacts for each platform run.
3. Publish from tagged builds using `NPM_TOKEN`.
