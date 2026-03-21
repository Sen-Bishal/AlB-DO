use crate::error::RuntimeError;
use crate::routing::{parse_query_string, HttpMethod};
use axum::body::Body;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use bytes::Bytes;
use futures_util::stream;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::BTreeMap;
use std::convert::Infallible;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub request_id: String,
    pub method: HttpMethod,
    pub path: String,
    pub query: BTreeMap<String, Vec<String>>,
    pub params: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub body: Bytes,
    pub metadata: BTreeMap<String, Value>,
}

impl RequestContext {
    pub fn new(
        method: HttpMethod,
        path: String,
        query_string: Option<&str>,
        params: BTreeMap<String, String>,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Self {
        Self {
            request_id: Uuid::new_v4().to_string(),
            method,
            path,
            query: parse_query_string(query_string),
            params,
            headers: normalize_headers(headers),
            body,
            metadata: BTreeMap::new(),
        }
    }

    pub fn query_value(&self, key: &str) -> Option<&str> {
        self.query
            .get(key)
            .and_then(|values| values.first().map(String::as_str))
    }

    pub fn parse_json_body<T: DeserializeOwned>(&self) -> Result<T, RuntimeError> {
        serde_json::from_slice::<T>(&self.body).map_err(|err| {
            RuntimeError::RequestHandling(format!(
                "failed to parse JSON request body for '{}': {err}",
                self.path
            ))
        })
    }
}

#[derive(Debug, Clone)]
pub struct ResponsePayload {
    pub status: StatusCode,
    pub headers: BTreeMap<String, String>,
    pub body: ResponseBody,
}

#[derive(Debug, Clone)]
pub enum ResponseBody {
    Full(Bytes),
    Stream(Vec<Bytes>),
}

impl ResponseBody {
    pub fn as_full_bytes(&self) -> Option<&Bytes> {
        match self {
            Self::Full(bytes) => Some(bytes),
            Self::Stream(_) => None,
        }
    }

    fn into_axum_body(self) -> Body {
        match self {
            Self::Full(bytes) => Body::from(bytes),
            Self::Stream(chunks) => {
                let stream = stream::iter(
                    chunks
                        .into_iter()
                        .map(|chunk| Ok::<Bytes, Infallible>(chunk)),
                );
                Body::from_stream(stream)
            }
        }
    }
}

impl ResponsePayload {
    pub fn new(status: StatusCode, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            headers: BTreeMap::new(),
            body: ResponseBody::Full(body.into()),
        }
    }

    pub fn ok_text(body: impl Into<String>) -> Self {
        let mut response = Self::new(StatusCode::OK, Bytes::from(body.into()));
        response.headers.insert(
            "content-type".to_string(),
            "text/plain; charset=utf-8".to_string(),
        );
        response
    }

    pub fn ok_html(body: impl Into<String>) -> Self {
        let mut response = Self::new(StatusCode::OK, Bytes::from(body.into()));
        response.headers.insert(
            "content-type".to_string(),
            "text/html; charset=utf-8".to_string(),
        );
        response
    }

    pub fn ok_html_stream<I, T>(chunks: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<Bytes>,
    {
        let mut response = Self {
            status: StatusCode::OK,
            headers: BTreeMap::new(),
            body: ResponseBody::Stream(chunks.into_iter().map(Into::into).collect()),
        };
        response.headers.insert(
            "content-type".to_string(),
            "text/html; charset=utf-8".to_string(),
        );
        response
    }

    pub fn json(value: &Value) -> Result<Self, RuntimeError> {
        let encoded = serde_json::to_vec(value).map_err(|err| {
            RuntimeError::RequestHandling(format!("failed to encode JSON response: {err}"))
        })?;
        let mut response = Self::new(StatusCode::OK, Bytes::from(encoded));
        response
            .headers
            .insert("content-type".to_string(), "application/json".to_string());
        Ok(response)
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers
            .insert(name.into().to_lowercase(), value.into());
        self
    }
}

impl IntoResponse for ResponsePayload {
    fn into_response(self) -> axum::response::Response {
        let mut response = Response::new(self.body.into_axum_body());
        *response.status_mut() = self.status;
        for (name, value) in self.headers {
            if let (Ok(name), Ok(value)) = (
                axum::http::HeaderName::from_bytes(name.as_bytes()),
                axum::http::HeaderValue::from_str(value.as_str()),
            ) {
                response.headers_mut().insert(name, value);
            }
        }
        response
    }
}

fn normalize_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut normalized = BTreeMap::new();
    for (name, value) in headers {
        if let Ok(value) = value.to_str() {
            normalized.insert(name.as_str().to_lowercase(), value.to_string());
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_value_reads_first_value() {
        let headers = HeaderMap::new();
        let ctx = RequestContext::new(
            HttpMethod::Get,
            "/search".to_string(),
            Some("q=albedo&q=rust"),
            BTreeMap::new(),
            &headers,
            Bytes::new(),
        );

        assert_eq!(ctx.query_value("q"), Some("albedo"));
    }

    #[test]
    fn test_response_payload_sets_content_type_for_text() {
        let payload = ResponsePayload::ok_text("hello");
        assert_eq!(
            payload.headers.get("content-type").map(String::as_str),
            Some("text/plain; charset=utf-8")
        );
        assert!(matches!(payload.body, ResponseBody::Full(_)));
    }

    #[test]
    fn test_response_payload_stream_html_sets_stream_body() {
        let payload = ResponsePayload::ok_html_stream([
            Bytes::from_static(b"<main>"),
            Bytes::from_static(b"ALBEDO"),
            Bytes::from_static(b"</main>"),
        ]);
        assert_eq!(
            payload.headers.get("content-type").map(String::as_str),
            Some("text/html; charset=utf-8")
        );
        assert!(matches!(payload.body, ResponseBody::Stream(_)));
    }
}
