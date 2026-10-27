//! `kiln cache serve`: a small HTTP server hosting a store's objects.
//!
//! One protocol, four endpoints:
//!
//! - `GET  /objects/<aa>/<key>` — the raw object bytes
//! - `PUT  /objects/<aa>/<key>` — store an object (decoded and re-encoded
//!   through the integrity-checked format; garbage never lands)
//! - `POST /has`  — which of the posted keys exist
//! - `GET  /list` — every key
//!
//! A token, when set, gates writes; a read token, when set, gates reads.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use kiln_core::{Error, Result};

use crate::Store;

/// A server's shared state.
#[derive(Clone)]
pub struct ServerState {
    store: std::sync::Arc<Store>,
    write_token: Option<String>,
    read_token: Option<String>,
}

/// Run the cache server until the process is stopped.
pub async fn serve(
    store: Store,
    bind: &str,
    write_token: Option<String>,
    read_token: Option<String>,
) -> Result<()> {
    let state = ServerState {
        store: std::sync::Arc::new(store),
        write_token,
        read_token,
    };
    let app = Router::new()
        .route("/objects/{shard}/{key}", get(get_object).put(put_object))
        .route("/has", post(has_objects))
        .route("/list", get(list_objects))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| Error::io("bind", bind, e))?;
    eprintln!("kiln cache server listening on http://{bind}");
    axum::serve(listener, app)
        .await
        .map_err(|e| Error::Network { context: "serving".into(), source: Box::new(e) })
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

async fn get_object(
    State(state): State<ServerState>,
    Path((_shard, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = &state.read_token {
        if bearer(&headers) != Some(token.as_str()) {
            return (StatusCode::UNAUTHORIZED, Vec::new());
        }
    }
    let key = key.trim_end_matches(".zst");
    // The wire bytes are the object file itself: clients verify the embedded
    // integrity hash on read, so a proxy that changes a byte is caught.
    match state.store.object_bytes(key) {
        Some(bytes) => (StatusCode::OK, bytes),
        None => (StatusCode::NOT_FOUND, Vec::new()),
    }
}

async fn put_object(
    State(state): State<ServerState>,
    Path((_shard, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    if let Some(token) = &state.write_token {
        if bearer(&headers) != Some(token.as_str()) {
            return StatusCode::UNAUTHORIZED;
        }
    }
    if body.is_empty() {
        return StatusCode::BAD_REQUEST;
    }
    // Decode-and-re-encode: the PUT body must be a valid, integrity-checked
    // object, and the store writes its own canonical bytes.
    let entry = match crate::format::decode(&body) {
        Ok(entry) => entry,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    let key = key.trim_end_matches(".zst").to_owned();
    match state.store.put(&key, &entry) {
        Ok(()) => StatusCode::CREATED,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn has_objects(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(keys): Json<Vec<String>>,
) -> (StatusCode, Json<Vec<String>>) {
    if let Some(token) = &state.read_token {
        if bearer(&headers) != Some(token.as_str()) {
            return (StatusCode::UNAUTHORIZED, Json(Vec::new()));
        }
    }
    let present = keys.into_iter().filter(|k| state.store.has(k)).collect();
    (StatusCode::OK, Json(present))
}

async fn list_objects(State(state): State<ServerState>, headers: HeaderMap) -> (StatusCode, Json<Vec<String>>) {
    if let Some(token) = &state.read_token {
        if bearer(&headers) != Some(token.as_str()) {
            return (StatusCode::UNAUTHORIZED, Json(Vec::new()));
        }
    }
    match state.store.list_keys() {
        Ok(keys) => (StatusCode::OK, Json(keys)),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(Vec::new())),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;

    fn bearer_of(headers: &HeaderMap) -> Option<&str> {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
    }

    #[test]
    fn bearer_tokens_are_parsed_exactly() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        assert_eq!(bearer_of(&headers), Some("secret"));
        assert_ne!(bearer_of(&headers), Some("other"));
        assert_eq!(bearer_of(&HeaderMap::new()), None);
    }
}
