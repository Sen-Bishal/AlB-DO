use crate::error::RuntimeError;
use crate::routing::{AuthPolicy, HttpMethod};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

// Runtime direction:
// - Apply ALBEDO plans to actual rendering behavior (critical-first SSR, deferred module loading,
//   selective hydration).
// - Keep standalone benchmark gates (TTFB, hydration, server CPU) as release blockers.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub renderer: Option<RendererConfig>,
    #[serde(default)]
    pub layouts: Vec<LayoutSpec>,
    #[serde(default)]
    pub routes: Vec<RouteSpec>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        }
    }
}

impl AppConfig {
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path).map_err(|err| RuntimeError::ConfigIo {
            path: path.display().to_string(),
            message: err.to_string(),
        })?;

        let mut config: AppConfig =
            serde_json::from_str(&contents).map_err(|err| RuntimeError::ConfigParse {
                path: path.display().to_string(),
                message: err.to_string(),
            })?;
        config.apply_env_overrides("ALBEDO_")?;
        config.validate()?;
        Ok(config)
    }

    pub fn apply_env_overrides(&mut self, prefix: &str) -> Result<(), RuntimeError> {
        if let Ok(host) = std::env::var(format!("{prefix}SERVER_HOST")) {
            self.server.host = host;
        }

        if let Ok(port) = std::env::var(format!("{prefix}SERVER_PORT")) {
            self.server.port = port.parse::<u16>().map_err(|err| {
                RuntimeError::InvalidConfig(format!(
                    "failed to parse {prefix}SERVER_PORT value '{port}': {err}"
                ))
            })?;
        }

        if let Ok(timeout_ms) = std::env::var(format!("{prefix}REQUEST_TIMEOUT_MS")) {
            self.server.request_timeout_ms = timeout_ms.parse::<u64>().map_err(|err| {
                RuntimeError::InvalidConfig(format!(
                    "failed to parse {prefix}REQUEST_TIMEOUT_MS value '{timeout_ms}': {err}"
                ))
            })?;
        }

        if let Ok(timeout_ms) = std::env::var(format!("{prefix}SHUTDOWN_TIMEOUT_MS")) {
            self.server.shutdown_timeout_ms = timeout_ms.parse::<u64>().map_err(|err| {
                RuntimeError::InvalidConfig(format!(
                    "failed to parse {prefix}SHUTDOWN_TIMEOUT_MS value '{timeout_ms}': {err}"
                ))
            })?;
        }

        if let Ok(enabled) = std::env::var(format!("{prefix}WEBTRANSPORT_ENABLED")) {
            self.server.webtransport.enabled = parse_bool_env(
                format!("{prefix}WEBTRANSPORT_ENABLED").as_str(),
                enabled.as_str(),
            )?;
        }

        if let Ok(cert_path) = std::env::var(format!("{prefix}WEBTRANSPORT_CERT_PATH")) {
            self.server.webtransport.cert_path = Some(cert_path);
        }

        if let Ok(key_path) = std::env::var(format!("{prefix}WEBTRANSPORT_KEY_PATH")) {
            self.server.webtransport.key_path = Some(key_path);
        }

        if let Ok(keepalive_ms) = std::env::var(format!("{prefix}WEBTRANSPORT_KEEPALIVE_MS")) {
            self.server.webtransport.keepalive_interval_ms =
                keepalive_ms.parse::<u64>().map_err(|err| {
                    RuntimeError::InvalidConfig(format!(
                        "failed to parse {prefix}WEBTRANSPORT_KEEPALIVE_MS value '{keepalive_ms}': {err}"
                    ))
                })?;
        }

        if let Ok(buffer_capacity) =
            std::env::var(format!("{prefix}WEBTRANSPORT_STREAM_BUFFER_CAPACITY"))
        {
            self.server.webtransport.stream_buffer_capacity =
                buffer_capacity.parse::<usize>().map_err(|err| {
                    RuntimeError::InvalidConfig(format!(
                        "failed to parse {prefix}WEBTRANSPORT_STREAM_BUFFER_CAPACITY value '{buffer_capacity}': {err}"
                    ))
                })?;
        }

        Ok(())
    }

    pub fn validate(&self) -> Result<(), RuntimeError> {
        self.server.validate()?;
        if let Some(renderer) = &self.renderer {
            renderer.validate()?;
        }

        let mut seen_layouts = std::collections::BTreeSet::new();
        for layout in &self.layouts {
            layout.validate()?;
            if !seen_layouts.insert(layout.path.clone()) {
                return Err(RuntimeError::InvalidConfig(format!(
                    "duplicate layout definition path: {}",
                    layout.path
                )));
            }
        }

        let mut seen = std::collections::BTreeSet::new();
        for route in &self.routes {
            route.validate()?;
            let key = format!("{} {}", route.method.as_str(), route.path);
            if !seen.insert(key.clone()) {
                return Err(RuntimeError::InvalidConfig(format!(
                    "duplicate route definition: {key}"
                )));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_shutdown_timeout_ms")]
    pub shutdown_timeout_ms: u64,
    #[serde(default)]
    pub webtransport: WebTransportConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            request_timeout_ms: default_request_timeout_ms(),
            shutdown_timeout_ms: default_shutdown_timeout_ms(),
            webtransport: WebTransportConfig::default(),
        }
    }
}

impl ServerConfig {
    pub fn socket_addr(&self) -> Result<SocketAddr, RuntimeError> {
        let ip: IpAddr = self.host.parse().map_err(|err| {
            RuntimeError::InvalidConfig(format!("invalid server host '{}': {err}", self.host))
        })?;
        Ok(SocketAddr::new(ip, self.port))
    }

    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.request_timeout_ms == 0 {
            return Err(RuntimeError::InvalidConfig(
                "request_timeout_ms must be > 0".to_string(),
            ));
        }
        if self.shutdown_timeout_ms == 0 {
            return Err(RuntimeError::InvalidConfig(
                "shutdown_timeout_ms must be > 0".to_string(),
            ));
        }
        self.webtransport.validate()?;
        self.socket_addr()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebTransportConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub cert_path: Option<String>,
    #[serde(default)]
    pub key_path: Option<String>,
    #[serde(default = "default_webtransport_keepalive_ms")]
    pub keepalive_interval_ms: u64,
    #[serde(default = "default_webtransport_stream_buffer_capacity")]
    pub stream_buffer_capacity: usize,
}

impl Default for WebTransportConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cert_path: None,
            key_path: None,
            keepalive_interval_ms: default_webtransport_keepalive_ms(),
            stream_buffer_capacity: default_webtransport_stream_buffer_capacity(),
        }
    }
}

impl WebTransportConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.keepalive_interval_ms == 0 {
            return Err(RuntimeError::InvalidConfig(
                "webtransport.keepalive_interval_ms must be > 0".to_string(),
            ));
        }
        if self.stream_buffer_capacity == 0 {
            return Err(RuntimeError::InvalidConfig(
                "webtransport.stream_buffer_capacity must be > 0".to_string(),
            ));
        }

        if self.enabled {
            let cert_path = self
                .cert_path
                .as_ref()
                .map(|value| value.trim())
                .unwrap_or_default();
            let key_path = self
                .key_path
                .as_ref()
                .map(|value| value.trim())
                .unwrap_or_default();

            if cert_path.is_empty() || key_path.is_empty() {
                return Err(RuntimeError::InvalidConfig(
                    "webtransport.enabled=true requires both webtransport.cert_path and webtransport.key_path"
                        .to_string(),
                ));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteSpec {
    pub name: String,
    pub method: HttpMethod,
    pub path: String,
    pub handler: String,
    #[serde(default)]
    pub entry_module: Option<String>,
    #[serde(default)]
    pub props_loader: Option<String>,
    #[serde(default)]
    pub middleware: Vec<String>,
    #[serde(default)]
    pub auth: Option<AuthPolicy>,
}

impl RouteSpec {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.name.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig(
                "route name must not be empty".to_string(),
            ));
        }
        if self.handler.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig(format!(
                "route '{}' has empty handler id",
                self.name
            )));
        }
        if let Some(entry_module) = &self.entry_module {
            if entry_module.trim().is_empty() {
                return Err(RuntimeError::InvalidConfig(format!(
                    "route '{}' has empty entry_module",
                    self.name
                )));
            }
        }
        if let Some(props_loader) = &self.props_loader {
            if props_loader.trim().is_empty() {
                return Err(RuntimeError::InvalidConfig(format!(
                    "route '{}' has empty props_loader",
                    self.name
                )));
            }
            if self.entry_module.is_none() {
                return Err(RuntimeError::InvalidConfig(format!(
                    "route '{}' sets props_loader without entry_module",
                    self.name
                )));
            }
        }
        if !self.path.starts_with('/') {
            return Err(RuntimeError::InvalidConfig(format!(
                "route '{}' has invalid path '{}' (must start with '/')",
                self.name, self.path
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RendererConfig {
    pub artifacts_dir: String,
    #[serde(default)]
    pub cache_snapshot_path: Option<String>,
    #[serde(default = "default_renderer_cache_ttl_ms")]
    pub cache_ttl_ms: u64,
    #[serde(default = "default_renderer_cache_swr_ms")]
    pub cache_swr_ms: u64,
    #[serde(default = "default_renderer_cache_max_entries")]
    pub cache_max_entries: usize,
}

impl RendererConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.artifacts_dir.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig(
                "renderer.artifacts_dir must not be empty".to_string(),
            ));
        }
        if self.cache_ttl_ms == 0 {
            return Err(RuntimeError::InvalidConfig(
                "renderer.cache_ttl_ms must be > 0".to_string(),
            ));
        }
        if self.cache_max_entries == 0 {
            return Err(RuntimeError::InvalidConfig(
                "renderer.cache_max_entries must be > 0".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LayoutSpec {
    pub name: String,
    pub path: String,
    pub handler: String,
}

impl LayoutSpec {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.name.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig(
                "layout name must not be empty".to_string(),
            ));
        }
        if self.handler.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig(format!(
                "layout '{}' has empty handler id",
                self.name
            )));
        }
        if !self.path.starts_with('/') {
            return Err(RuntimeError::InvalidConfig(format!(
                "layout '{}' has invalid path '{}' (must start with '/')",
                self.name, self.path
            )));
        }
        if self.path.len() > 1 && self.path.ends_with('/') {
            return Err(RuntimeError::InvalidConfig(format!(
                "layout '{}' has invalid path '{}' (must not end with '/')",
                self.name, self.path
            )));
        }
        if self.path.contains("//") {
            return Err(RuntimeError::InvalidConfig(format!(
                "layout '{}' has invalid path '{}' (must not contain '//')",
                self.name, self.path
            )));
        }
        Ok(())
    }
}

const fn default_port() -> u16 {
    3000
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

const fn default_request_timeout_ms() -> u64 {
    15_000
}

const fn default_shutdown_timeout_ms() -> u64 {
    5_000
}

const fn default_webtransport_keepalive_ms() -> u64 {
    15_000
}

const fn default_webtransport_stream_buffer_capacity() -> usize {
    128
}

const fn default_renderer_cache_ttl_ms() -> u64 {
    30_000
}

const fn default_renderer_cache_swr_ms() -> u64 {
    120_000
}

const fn default_renderer_cache_max_entries() -> usize {
    512
}

fn parse_bool_env(name: &str, value: &str) -> Result<bool, RuntimeError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(RuntimeError::InvalidConfig(format!(
            "failed to parse {name} value '{value}' as boolean"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_server_config_validates() {
        let cfg = ServerConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_webtransport_requires_cert_and_key_when_enabled() {
        let mut cfg = ServerConfig::default();
        cfg.webtransport.enabled = true;

        let err = cfg.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("requires both webtransport.cert_path and webtransport.key_path"));

        cfg.webtransport.cert_path = Some("./cert.pem".to_string());
        cfg.webtransport.key_path = Some("./key.pem".to_string());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_app_config_detects_duplicate_route_pairs() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![
                RouteSpec {
                    name: "users.show".to_string(),
                    method: HttpMethod::Get,
                    path: "/users/{id}".to_string(),
                    handler: "users.show".to_string(),
                    entry_module: None,
                    props_loader: None,
                    middleware: Vec::new(),
                    auth: None,
                },
                RouteSpec {
                    name: "users.show_duplicate".to_string(),
                    method: HttpMethod::Get,
                    path: "/users/{id}".to_string(),
                    handler: "users.show".to_string(),
                    entry_module: None,
                    props_loader: None,
                    middleware: Vec::new(),
                    auth: None,
                },
            ],
        };

        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("duplicate route definition"));
    }

    #[test]
    fn test_app_config_detects_duplicate_layout_paths() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: vec![
                LayoutSpec {
                    name: "root".to_string(),
                    path: "/".to_string(),
                    handler: "layout.root".to_string(),
                },
                LayoutSpec {
                    name: "root_duplicate".to_string(),
                    path: "/".to_string(),
                    handler: "layout.alt".to_string(),
                },
            ],
            routes: Vec::new(),
        };

        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("duplicate layout definition path"));
    }
}
