use albedo_server::{
    AlbedoServerBuilder, AppConfig, AuthDecision, AuthPolicy, AuthProvider, HttpMethod, LayoutSpec,
    RequestContext, ResponsePayload, RouteSpec, RuntimeError, RuntimeMiddleware, ServerConfig,
};
use async_trait::async_trait;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

const AUTH_TOKEN_HEADER: &str = "x-albedo-demo-token";
const ROLE_HEADER: &str = "x-albedo-role";

struct ServerTimingMiddleware;

#[async_trait]
impl RuntimeMiddleware for ServerTimingMiddleware {
    async fn on_request(&self, ctx: &mut RequestContext) -> Result<(), RuntimeError> {
        let started_at_ms = now_millis_u64();
        ctx.metadata
            .insert("request_start_ms".to_string(), json!(started_at_ms));
        Ok(())
    }

    async fn on_response(
        &self,
        ctx: &RequestContext,
        response: &mut ResponsePayload,
    ) -> Result<(), RuntimeError> {
        let started_at_ms = ctx
            .metadata
            .get("request_start_ms")
            .and_then(|value| value.as_u64())
            .unwrap_or_else(now_millis_u64);
        let finished_at_ms = now_millis_u64();
        let elapsed_ms = finished_at_ms.saturating_sub(started_at_ms);
        response.headers.insert(
            "server-timing".to_string(),
            format!("albedo;dur={elapsed_ms}"),
        );
        Ok(())
    }
}

#[derive(Clone)]
struct DemoAuthProvider {
    expected_token: String,
}

#[async_trait]
impl AuthProvider for DemoAuthProvider {
    async fn authorize(
        &self,
        ctx: &RequestContext,
        policy: &AuthPolicy,
    ) -> Result<AuthDecision, RuntimeError> {
        let provided_token = ctx
            .headers
            .get(AUTH_TOKEN_HEADER)
            .map(String::as_str)
            .unwrap_or("");
        let authenticated = provided_token == self.expected_token;

        match policy {
            AuthPolicy::Optional => Ok(AuthDecision::Allow),
            AuthPolicy::Required => {
                if authenticated {
                    Ok(AuthDecision::Allow)
                } else {
                    Ok(AuthDecision::Deny {
                        reason: format!(
                            "missing or invalid '{}' header. set '{}: {}'",
                            AUTH_TOKEN_HEADER, AUTH_TOKEN_HEADER, self.expected_token
                        ),
                    })
                }
            }
            AuthPolicy::Role(required_role) => {
                if !authenticated {
                    return Ok(AuthDecision::Deny {
                        reason: format!(
                            "missing or invalid '{}' header. set '{}: {}'",
                            AUTH_TOKEN_HEADER, AUTH_TOKEN_HEADER, self.expected_token
                        ),
                    });
                }

                let provided_role = ctx
                    .headers
                    .get(ROLE_HEADER)
                    .map(String::as_str)
                    .unwrap_or("");
                if provided_role == required_role {
                    Ok(AuthDecision::Allow)
                } else {
                    Ok(AuthDecision::Deny {
                        reason: format!(
                            "route requires role '{}'; provide '{}: {}'",
                            required_role, ROLE_HEADER, required_role
                        ),
                    })
                }
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct EchoRequest {
    message: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let demo_token = demo_token();
    let config = showcase_config();
    let port = config.server.port;

    let server = AlbedoServerBuilder::new(config)
        .with_auth_provider(DemoAuthProvider {
            expected_token: demo_token.clone(),
        })
        .register_middleware("timing", ServerTimingMiddleware)
        .register_layout("layout.root", |ctx: RequestContext, inner: String| async move {
            Ok(render_root_layout(ctx.path.as_str(), inner.as_str()))
        })
        .register_layout("layout.showcase", |_ctx: RequestContext, inner: String| async move {
            Ok(format!(
                "<section class=\"scope-panel\">\
                 <header><p class=\"chip\">showcase scope</p><h2>Standalone Runtime Features</h2></header>\
                 {inner}</section>"
            ))
        })
        .register_layout("layout.admin", |_ctx: RequestContext, inner: String| async move {
            Ok(format!(
                "<section class=\"scope-panel admin-scope\">\
                 <header><p class=\"chip\">protected scope</p><h2>Admin Surface</h2></header>\
                 {inner}</section>"
            ))
        })
        .register_handler("page.home", |_ctx: RequestContext| async move {
            Ok(ResponsePayload::ok_html(render_home_page()))
        })
        .register_handler("page.capabilities", |_ctx: RequestContext| async move {
            Ok(ResponsePayload::ok_html(render_capabilities_page()))
        })
        .register_handler("page.users.show", |ctx: RequestContext| async move {
            let user_id = ctx
                .params
                .get("id")
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            Ok(ResponsePayload::ok_html(render_user_page(user_id.as_str())))
        })
        .register_handler("page.docs", |ctx: RequestContext| async move {
            let slug = ctx
                .params
                .get("slug")
                .cloned()
                .unwrap_or_else(|| "index".to_string());
            Ok(ResponsePayload::ok_html(render_docs_page(slug.as_str())))
        })
        .register_handler("page.blog", |ctx: RequestContext| async move {
            let slug = ctx
                .params
                .get("slug")
                .cloned()
                .unwrap_or_else(|| "home".to_string());
            Ok(ResponsePayload::ok_html(render_blog_page(slug.as_str())))
        })
        .register_handler("page.stream", |_ctx: RequestContext| async move {
            Ok(ResponsePayload::ok_html_stream([
                "<main class=\"stream-view\">",
                "<h1>Streaming HTML Contract</h1>",
                "<p>Shell chunk flushed first.</p>",
                "<section class=\"stream-block\"><h3>Chunk A</h3><p>Critical hero markup.</p></section>",
                "<section class=\"stream-block\"><h3>Chunk B</h3><p>Deferred analytics and add-ons.</p></section>",
                "<section class=\"stream-block\"><h3>Chunk C</h3><p>Hydration payload tags follow.</p></section>",
                "</main>",
            ]))
        })
        .register_handler("page.admin.dashboard", |_ctx: RequestContext| async move {
            Ok(ResponsePayload::ok_html(render_admin_page()))
        })
        .register_handler("page.admin.ops", |_ctx: RequestContext| async move {
            Ok(ResponsePayload::ok_html(
                "<main><h1>Ops Console</h1><p>Role-gated route reached with <code>x-albedo-role: ops</code>.</p></main>",
            ))
        })
        .register_handler("api.health", |_ctx: RequestContext| async move {
            ResponsePayload::json(&json!({
                "status": "ok",
                "runtime": "albedo-server",
                "showcase_mode": "standalone"
            }))
        })
        .register_handler("api.showcase", |ctx: RequestContext| async move {
            ResponsePayload::json(&json!({
                "request_id": ctx.request_id,
                "path": ctx.path,
                "features": [
                    "nested_layouts",
                    "dynamic_segments",
                    "catch_all_routes",
                    "stream_html_contract",
                    "middleware_timing",
                    "auth_policies",
                    "json_api_handlers"
                ]
            }))
        })
        .register_handler("api.echo.get", |_ctx: RequestContext| async move {
            ResponsePayload::json(&json!({
                "usage": "POST JSON to /api/echo with {\"message\":\"...\"}",
                "example_headers": {
                    "content-type": "application/json"
                }
            }))
        })
        .register_handler("api.echo.post", |ctx: RequestContext| async move {
            let payload: EchoRequest = ctx.parse_json_body()?;
            ResponsePayload::json(&json!({
                "received": payload.message,
                "request_id": ctx.request_id,
                "server_time_ms": now_millis_u64()
            }))
        })
        .register_handler("api.admin.metrics", |ctx: RequestContext| async move {
            let role = ctx
                .headers
                .get(ROLE_HEADER)
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            ResponsePayload::json(&json!({
                "scope": "admin",
                "role": role,
                "p95_ttfb_ms": 61.6,
                "module_cache_hit_ratio": 0.93,
                "stream_first_chunk_ms": 22.4
            }))
        })
        .build()?;

    print_startup_instructions(port, demo_token.as_str());
    server.run().await?;
    Ok(())
}

fn showcase_config() -> AppConfig {
    AppConfig {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: demo_port(),
            ..ServerConfig::default()
        },
        renderer: None,
        layouts: vec![
            LayoutSpec {
                name: "root".to_string(),
                path: "/".to_string(),
                handler: "layout.root".to_string(),
            },
            LayoutSpec {
                name: "showcase".to_string(),
                path: "/showcase".to_string(),
                handler: "layout.showcase".to_string(),
            },
            LayoutSpec {
                name: "admin".to_string(),
                path: "/admin".to_string(),
                handler: "layout.admin".to_string(),
            },
        ],
        routes: vec![
            route("page.home", HttpMethod::Get, "/", "page.home"),
            route(
                "page.capabilities",
                HttpMethod::Get,
                "/showcase/capabilities",
                "page.capabilities",
            ),
            route(
                "page.users.show",
                HttpMethod::Get,
                "/users/[id]",
                "page.users.show",
            ),
            route("page.docs", HttpMethod::Get, "/docs/[...slug]", "page.docs"),
            route(
                "page.blog",
                HttpMethod::Get,
                "/blog/[[...slug]]",
                "page.blog",
            ),
            route(
                "page.stream",
                HttpMethod::Get,
                "/showcase/stream",
                "page.stream",
            ),
            route_with_auth(
                "page.admin.dashboard",
                HttpMethod::Get,
                "/admin",
                "page.admin.dashboard",
                AuthPolicy::Required,
            ),
            route_with_auth(
                "page.admin.ops",
                HttpMethod::Get,
                "/admin/ops",
                "page.admin.ops",
                AuthPolicy::Role("ops".to_string()),
            ),
            route("api.health", HttpMethod::Get, "/api/health", "api.health"),
            route(
                "api.showcase",
                HttpMethod::Get,
                "/api/showcase",
                "api.showcase",
            ),
            route("api.echo.get", HttpMethod::Get, "/api/echo", "api.echo.get"),
            route(
                "api.echo.post",
                HttpMethod::Post,
                "/api/echo",
                "api.echo.post",
            ),
            route_with_auth(
                "api.admin.metrics",
                HttpMethod::Get,
                "/api/admin/metrics",
                "api.admin.metrics",
                AuthPolicy::Required,
            ),
        ],
    }
}

fn route(name: &str, method: HttpMethod, path: &str, handler: &str) -> RouteSpec {
    RouteSpec {
        name: name.to_string(),
        method,
        path: path.to_string(),
        handler: handler.to_string(),
        entry_module: None,
        props_loader: None,
        middleware: vec!["timing".to_string()],
        auth: None,
    }
}

fn route_with_auth(
    name: &str,
    method: HttpMethod,
    path: &str,
    handler: &str,
    auth: AuthPolicy,
) -> RouteSpec {
    RouteSpec {
        name: name.to_string(),
        method,
        path: path.to_string(),
        handler: handler.to_string(),
        entry_module: None,
        props_loader: None,
        middleware: vec!["timing".to_string()],
        auth: Some(auth),
    }
}

fn render_root_layout(path: &str, inner: &str) -> String {
    format!(
        "<!doctype html>\
         <html lang=\"en\">\
         <head>\
           <meta charset=\"utf-8\"/>\
           <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"/>\
           <title>ALBEDO Standalone Showcase</title>\
           <style>\
           :root{{--bg:#06131a;--bg2:#0f2733;--panel:#f2efe8;--ink:#111827;--muted:#5b6470;--line:rgba(17,24,39,.17);--accent:#12708d;--warn:#923535}}\
           *{{box-sizing:border-box}}\
           body{{margin:0;font-family:ui-sans-serif,Segoe UI,Arial,sans-serif;color:var(--ink);\
                background:radial-gradient(1100px 500px at 0% -10%,rgba(18,112,141,.35),transparent 60%),\
                          radial-gradient(900px 450px at 100% 0%,rgba(146,53,53,.24),transparent 62%),\
                          linear-gradient(150deg,var(--bg) 0%,var(--bg2) 58%,#1a3b4a 100%)}}\
           header,footer{{padding:14px 18px;background:#0a1118;color:#f8fafc}}\
           header strong{{letter-spacing:.02em}}\
           nav{{margin-top:8px;display:flex;flex-wrap:wrap;gap:8px}}\
           nav a{{display:inline-flex;text-decoration:none;border:1px solid rgba(248,250,252,.2);color:#dbeafe;padding:6px 10px;border-radius:999px;font-size:13px}}\
           nav a:hover{{background:rgba(219,234,254,.08)}}\
           main{{max-width:1040px;margin:22px auto;padding:20px;background:var(--panel);border-radius:16px;border:1px solid var(--line);box-shadow:0 22px 52px rgba(2,8,15,.28)}}\
           h1{{margin-top:0}}\
           p{{line-height:1.5}}\
           code{{font-family:ui-monospace,Consolas,monospace;background:rgba(17,24,39,.08);border-radius:6px;padding:1px 6px}}\
           .grid{{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:12px}}\
           .card{{padding:12px;border:1px solid var(--line);border-radius:12px;background:#fff}}\
           .hint{{color:var(--muted);font-size:14px}}\
           .chip{{display:inline-block;margin:0;padding:4px 9px;border-radius:999px;border:1px solid rgba(18,112,141,.25);font-size:12px;color:var(--accent);text-transform:uppercase;letter-spacing:.07em}}\
           .scope-panel{{border:1px dashed rgba(18,112,141,.35);padding:12px;border-radius:12px;background:rgba(255,255,255,.7)}}\
           .scope-panel header h2{{margin:.45rem 0 .7rem}}\
           .admin-scope{{border-color:rgba(146,53,53,.44);background:rgba(255,247,247,.85)}}\
           .stream-view{{padding:16px;border-radius:12px;background:#fff;border:1px solid var(--line)}}\
           .stream-block{{margin-top:10px;padding:10px;border-left:4px solid var(--accent);background:#f7fbfd;border-radius:8px}}\
           .warn{{color:var(--warn);font-weight:600}}\
           @media (max-width:900px){{.grid{{grid-template-columns:1fr}}main{{margin:14px auto;padding:14px}}}}\
           </style>\
         </head>\
         <body>\
           <header>\
             <strong>ALBEDO Standalone Renderer Showcase</strong>\
             <span class=\"hint\">path: {path}</span>\
             <nav>\
               <a href=\"/\">Home</a>\
               <a href=\"/showcase/capabilities\">Capabilities</a>\
               <a href=\"/showcase/stream\">Streaming</a>\
               <a href=\"/users/42\">Dynamic Route</a>\
               <a href=\"/docs/routing/catch-all\">Catch-all</a>\
               <a href=\"/blog\">Optional Catch-all</a>\
               <a href=\"/api/showcase\">API Showcase</a>\
               <a href=\"/admin\">Admin</a>\
             </nav>\
           </header>\
           {inner}\
           <footer>\
             standalone mode | rust runtime only | no next bridge\
           </footer>\
         </body>\
         </html>"
    )
}

fn render_home_page() -> String {
    "<main>\
      <h1>Rust-Native Standalone Runtime</h1>\
      <p>This showcase runs directly on <code>albedo-server</code> with no Next.js request path involvement.</p>\
      <div class=\"grid\">\
        <article class=\"card\"><h3>Routing Parity</h3><p>Dynamic segments, catch-all, optional catch-all, and method guards.</p></article>\
        <article class=\"card\"><h3>Lifecycle</h3><p>Middleware + auth policy enforcement + nested layout composition.</p></article>\
        <article class=\"card\"><h3>Streaming Contract</h3><p>Chunked HTML payload path exposed via <code>/showcase/stream</code>.</p></article>\
        <article class=\"card\"><h3>API Surface</h3><p>Typed JSON handlers with request context and protected admin endpoint.</p></article>\
      </div>\
      <p class=\"hint\">Try <code>curl http://127.0.0.1:4000/api/showcase</code> and <code>curl -X POST http://127.0.0.1:4000/api/echo -H \"content-type: application/json\" -d \"{\\\"message\\\":\\\"hello\\\"}\"</code>.</p>\
     </main>"
        .to_string()
}

fn render_capabilities_page() -> String {
    "<main>\
      <h1>Capability Matrix</h1>\
      <div class=\"grid\">\
        <article class=\"card\"><h3>HTTP Runtime</h3><p>Axum + deterministic route matcher + explicit contracts.</p></article>\
        <article class=\"card\"><h3>Layout Hierarchy</h3><p>Root and path-prefix layout handlers applied in stable order.</p></article>\
        <article class=\"card\"><h3>Auth Policies</h3><p><code>required</code> and <code>role(ops)</code> policies via pluggable provider.</p></article>\
        <article class=\"card\"><h3>Observability</h3><p>Per-request timing exported as <code>server-timing</code> header.</p></article>\
      </div>\
      <p class=\"hint\">Admin routes: <code>/admin</code> and <code>/admin/ops</code>.</p>\
     </main>"
        .to_string()
}

fn render_user_page(user_id: &str) -> String {
    format!(
        "<main>\
         <h1>User {user_id}</h1>\
         <p>Resolved from <code>/users/[id]</code> dynamic segment.</p>\
         <p class=\"hint\">Use this pattern for param-driven page handlers and typed API lookups.</p>\
         </main>"
    )
}

fn render_docs_page(slug: &str) -> String {
    format!(
        "<main>\
         <h1>Docs: {slug}</h1>\
         <p>Resolved from catch-all route <code>/docs/[...slug]</code>.</p>\
         <p class=\"hint\">Great for docs trees and nested knowledge bases.</p>\
         </main>"
    )
}

fn render_blog_page(slug: &str) -> String {
    format!(
        "<main>\
         <h1>Blog</h1>\
         <p>Resolved optional catch-all slug: <code>{slug}</code>.</p>\
         <p class=\"hint\">Route <code>/blog/[[...slug]]</code> supports both base and nested paths.</p>\
         </main>"
    )
}

fn render_admin_page() -> String {
    "<main>\
      <h1>Admin Dashboard</h1>\
      <p>Authenticated route reached. This page requires header <code>x-albedo-demo-token</code>.</p>\
      <p class=\"warn\">Role route <code>/admin/ops</code> also requires <code>x-albedo-role: ops</code>.</p>\
      <p class=\"hint\">Use this as a template for secure product areas and route-level auth semantics.</p>\
     </main>"
        .to_string()
}

fn print_startup_instructions(port: u16, demo_token: &str) {
    println!("ALBEDO standalone showcase running on http://127.0.0.1:{port}");
    println!("Public pages:");
    println!("  /");
    println!("  /showcase/capabilities");
    println!("  /showcase/stream");
    println!("  /users/42");
    println!("  /docs/routing/catch-all");
    println!("  /blog");
    println!("Public APIs:");
    println!("  /api/health");
    println!("  /api/showcase");
    println!("  GET  /api/echo");
    println!("  POST /api/echo");
    println!("Protected routes:");
    println!("  /admin (header: {}: {})", AUTH_TOKEN_HEADER, demo_token);
    println!(
        "  /admin/ops (headers: {}: {}, {}: ops)",
        AUTH_TOKEN_HEADER, demo_token, ROLE_HEADER
    );
    println!(
        "  /api/admin/metrics (header: {}: {})",
        AUTH_TOKEN_HEADER, demo_token
    );
}

fn now_millis_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn demo_port() -> u16 {
    std::env::var("ALBEDO_DEMO_PORT")
        .ok()
        .and_then(|raw| raw.parse::<u16>().ok())
        .unwrap_or(4000)
}

fn demo_token() -> String {
    std::env::var("ALBEDO_DEMO_TOKEN").unwrap_or_else(|_| "albedo-demo".to_string())
}
