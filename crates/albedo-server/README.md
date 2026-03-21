# albedo-server

`albedo-server` is the runtime/server layer for ALBEDO.

This crate provides:

- validated app/server config contracts
- deterministic route definitions
- radix-tree route matching (static + dynamic params)
- request lifecycle context + response payload model
- middleware and auth provider interfaces
- runtime server builder and HTTP dispatch
- full or chunked response body support for stream-oriented handlers

## Core Contracts

### App Config

```json
{
  "server": {
    "host": "127.0.0.1",
    "port": 3000,
    "request_timeout_ms": 15000,
    "shutdown_timeout_ms": 5000
  },
  "layouts": [
    {
      "name": "root",
      "path": "/",
      "handler": "layout.root"
    }
  ],
  "routes": [
    {
      "name": "users.show",
      "method": "GET",
      "path": "/users/{id}",
      "handler": "users.show",
      "middleware": ["request_id", "audit_log"],
      "auth": "required"
    }
  ]
}
```

### Route Handler Interface

- async handler receives `RequestContext`
- returns `ResponsePayload`

### Layout Handler Interface

- async layout receives `(RequestContext, inner_html)`
- returns wrapped HTML string
- applied from innermost matched layout to outermost path prefix

### Middleware Interface

- `on_request` hook before handler execution
- `on_response` hook after handler execution (reverse order)

### Auth Interface

- pluggable `AuthProvider`
- per-route auth policy support: `optional`, `required`, `role(<name>)`

## Routing Behavior

- route matching uses a compiled radix tree (`matchit`)
- supports static and dynamic segments:
  - legacy style: `/users/{id}`
  - Next-style dynamic: `/users/[id]`
  - Next-style catch-all: `/docs/[...slug]`
  - Next-style optional catch-all: `/docs/[[...slug]]`
- route layouts are resolved by path-prefix hierarchy (`/` -> `/dashboard` -> `/dashboard/settings`)
- method guards are enforced per route path
- method mismatches produce HTTP `405` + `Allow` header
- unknown paths produce HTTP `404`

## Quick Example

```rust
use albedo_server::{
    AlbedoServerBuilder, AppConfig, HttpMethod, RequestContext, ResponsePayload, RouteSpec,
    ServerConfig,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = AppConfig {
        server: ServerConfig::default(),
        layouts: Vec::new(),
        routes: vec![RouteSpec {
            name: "health".to_string(),
            method: HttpMethod::Get,
            path: "/health".to_string(),
            handler: "health.handler".to_string(),
            middleware: Vec::new(),
            auth: None,
        }],
    };

    let server = AlbedoServerBuilder::new(config)
        .register_handler("health.handler", |_ctx: RequestContext| async move {
            Ok(ResponsePayload::ok_text("ok"))
        })
        .build()?;

    server.run().await?;
    Ok(())
}
```

## Runnable Standalone Showcase App

Run a standalone showcase app (no Next.js bridge) with:

- nested layouts
- dynamic + catch-all + optional catch-all routes
- stream-oriented HTML response example
- auth policies (`required`, `role(ops)`)
- JSON API endpoints (public + protected)

```bash
cargo run --manifest-path crates/albedo-server/Cargo.toml --bin albedo-server-demo
```

Optional port override:

```bash
$env:ALBEDO_DEMO_PORT=4100
cargo run --manifest-path crates/albedo-server/Cargo.toml --bin albedo-server-demo
```

Optional auth token override:

```bash
$env:ALBEDO_DEMO_TOKEN="my-showcase-token"
cargo run --manifest-path crates/albedo-server/Cargo.toml --bin albedo-server-demo
```

Key pages:

- `http://127.0.0.1:4000/`
- `http://127.0.0.1:4000/showcase/capabilities`
- `http://127.0.0.1:4000/showcase/stream`
- `http://127.0.0.1:4000/users/42`
- `http://127.0.0.1:4000/docs/routing/catch-all`
- `http://127.0.0.1:4000/blog`

Public APIs:

- `GET /api/health`
- `GET /api/showcase`
- `GET /api/echo`
- `POST /api/echo`

Protected examples:

- `GET /admin`
- `GET /admin/ops`
- `GET /api/admin/metrics`

Required headers (default token):

- `x-albedo-demo-token: albedo-demo`
- `x-albedo-role: ops` (for `/admin/ops`)

Example calls:

```bash
curl http://127.0.0.1:4000/api/showcase
curl -X POST http://127.0.0.1:4000/api/echo -H "content-type: application/json" -d "{\"message\":\"hello\"}"
curl http://127.0.0.1:4000/admin -H "x-albedo-demo-token: albedo-demo"
curl http://127.0.0.1:4000/admin/ops -H "x-albedo-demo-token: albedo-demo" -H "x-albedo-role: ops"
```
