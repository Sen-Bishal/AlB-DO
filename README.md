# Albedo

**Albedo is a professional CLI product for building, previewing, and releasing modern web applications through one consistent workflow.**

Albedo focuses on what teams need most in production: fast local iteration, predictable build outputs, and clear release behavior. This document is intentionally product-facing and avoids internal implementation details.

## Release Overview

- Unified command experience from project setup to production build
- Fast feedback loop during development
- Reliable production build output for deployment pipelines
- Strong diagnostics for faster issue resolution
- Consistent behavior across local and CI environments

## Feature Set

### Project Initialization
- `albedo init [DIR]` scaffolds a ready-to-run project
- TypeScript starter by default, JavaScript starter with `--js`
- Safe overwrite controls with `--force`

### Development Experience
- `albedo dev [DIR]` starts local development mode
- Live update workflow while you edit
- Configurable host, port, entry point, and HMR behavior
- Optional strict and verbose modes for stronger validation

### Production Delivery
- `albedo build [DIR]` runs production build mode
- Output is generated in `.albedo/dist`
- Designed for CI and release automation

## Quick Start

### Prerequisites
- Rust (stable)
- Cargo

### Start a new app

```bash
cargo run --bin albedo -- init my-app
cd my-app
cargo run --bin albedo -- dev
```

### Build for release

```bash
cargo run --bin albedo -- build
```

### Use the compiled binary

```bash
cargo build --release
./target/release/albedo init my-app
./target/release/albedo dev my-app
./target/release/albedo build my-app
```

## CLI Reference

```text
albedo <COMMAND> [OPTIONS]

Commands
  init [DIR]            Create a new project scaffold
  dev [DIR]             Start development mode
  build [DIR]           Run production build mode
  run dev [DIR]         Run development workflow directly
  help                  Show command help
```

Common development flags:
- `--config <FILE>` use `albedo.config.json` or `albedo.config.ts`
- `--entry <FILE>` override entry module
- `--host <IP>` and `--port <PORT>` override server binding
- `--no-hmr` disable hot reload behavior
- `--strict` enable stricter startup checks
- `--verbose` or `-v` enable additional diagnostics
- `--open` open browser on startup
- `--print-contract` print resolved runtime configuration


## License

Licensed under MIT. See [LICENSE.md](LICENSE.md).
