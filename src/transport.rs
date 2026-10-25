//! In-process transport: the desktop shell registers a Tauri custom URI scheme and hands every webview
//! request to [`dispatch`], which runs it through the axum [`Router`] with `tower::ServiceExt::oneshot`.
//! No socket is bound anywhere.
//!
//! Limits of a custom-scheme response (the whole body is handed to the webview at once):
//! * no streaming, so SSE endpoints do not work (the frontend polls instead, see `web/components/transport.js`);
//! * a response with a known `Content-Length` above [`MAX_BODY`] (64 MiB) to a plain GET (no `Range`) is cut
//!   to its first [`MAX_BODY`] bytes and answered as `206 Partial Content` with a matching `Content-Range`
//!   and `Accept-Ranges: bytes`, so media elements and `fetch` keep going with follow-up Range requests and
//!   memory stays bounded. Normal `Range` requests are already answered by `/audio/stream` (<= 16 MiB each).
use axum::body::Body;
use axum::http::{header, HeaderValue, Method, Request, Response, StatusCode};
use axum::Router;
use futures::StreamExt;
use tower::ServiceExt;

/// Largest body forwarded in one non-Range response.
pub const MAX_BODY: u64 = 64 * 1024 * 1024;

/// Convert a request received by a URI-scheme handler into an axum request: method, path + query only
/// (scheme and authority are dropped), all headers, body bytes.
pub fn to_axum_request(req: Request<Vec<u8>>) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    let pq = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    parts.uri = pq.parse().unwrap_or_else(|_| "/".parse().unwrap());
    Request::from_parts(parts, Body::from(body))
}

fn error_response(status: StatusCode, msg: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CONTENT_LENGTH, msg.len())
        .body(msg.as_bytes().to_vec())
        .unwrap()
}

/// Collect an axum response into bytes for the webview. HEAD keeps headers (incl. Content-Length) but sends
/// no body. See the module docs for the large-body rule.
pub async fn from_axum_response(resp: Response<Body>, method: &Method, had_range: bool) -> Response<Vec<u8>> {
    let (mut parts, body) = resp.into_parts();
    if method == Method::HEAD {
        return Response::from_parts(parts, Vec::new());
    }
    let declared: Option<u64> = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());

    let cap = match declared {
        Some(len) if len > MAX_BODY && parts.status == StatusCode::OK && !had_range => Some((len, MAX_BODY)),
        _ => None,
    };

    let mut out: Vec<u8> = Vec::with_capacity(declared.unwrap_or(0).min(MAX_BODY) as usize);
    let mut stream = body.into_data_stream();
    let limit = cap.map(|(_, c)| c).unwrap_or(u64::MAX);
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(b) => {
                let room = (limit - out.len() as u64).min(b.len() as u64) as usize;
                out.extend_from_slice(&b[..room]);
                if out.len() as u64 >= limit {
                    break;
                }
            }
            Err(e) => {
                tracing::warn!("transport: response body error: {e}");
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "body error");
            }
        }
    }
    if let Some((total, c)) = cap {
        parts.status = StatusCode::PARTIAL_CONTENT;
        parts.headers.insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes 0-{}/{total}", c - 1)).unwrap());
        parts.headers.insert(header::CONTENT_LENGTH, HeaderValue::from(c));
        parts.headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    } else if declared.is_none() {
        parts.headers.insert(header::CONTENT_LENGTH, HeaderValue::from(out.len()));
    }
    Response::from_parts(parts, out)
}

/// Run one webview request through the router (the same path for the desktop scheme handler and tests).
pub async fn dispatch(router: Router, req: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let method = req.method().clone();
    let had_range = req.headers().contains_key(header::RANGE);
    tracing::debug!("scheme request: {method} {}", req.uri());
    let resp = match router.oneshot(to_axum_request(req)).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    let out = from_axum_response(resp, &method, had_range).await;
    tracing::debug!("scheme response: {} ({} bytes)", out.status(), out.body().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_app, App};
    use std::path::PathBuf;
    use std::sync::OnceLock;

    static APP: OnceLock<(App, PathBuf)> = OnceLock::new();

    /// One app per test process (lifecycle home is a process-wide OnceLock). Needs a tokio runtime on first use.
    fn app() -> &'static (App, PathBuf) {
        APP.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("daouchi-transport-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let dir = dir.canonicalize().unwrap();
            std::env::set_var("DAOUCHI_HOME", dir.join("home"));
            let app = build_app().unwrap();
            // small WAV: 1000 bytes of payload after a 44-byte header
            let wav_dir = dir.join("lib");
            std::fs::create_dir_all(&wav_dir).unwrap();
            let mut wav = b"RIFF".to_vec();
            wav.extend((1036u32).to_le_bytes());
            wav.extend(b"WAVEfmt ");
            wav.extend(16u32.to_le_bytes());
            wav.extend([1, 0, 1, 0]);
            wav.extend(8000u32.to_le_bytes());
            wav.extend(16000u32.to_le_bytes());
            wav.extend([2, 0, 16, 0]);
            wav.extend(b"data");
            wav.extend(1000u32.to_le_bytes());
            wav.extend((0..1000u32).map(|i| (i % 251) as u8));
            std::fs::write(wav_dir.join("t.wav"), &wav).unwrap();
            app.state.indexed_roots.lock().unwrap().push(wav_dir.to_string_lossy().into_owned());
            (app, wav_dir)
        })
    }

    fn req(method: &str, uri: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Request<Vec<u8>> {
        let mut b = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(body).unwrap()
    }

    async fn call(r: Request<Vec<u8>>) -> Response<Vec<u8>> {
        dispatch(app().0.router.clone(), r).await
    }

    // --- conversion unit tests (no app needed) ---

    #[test]
    fn request_conversion_keeps_path_query_headers() {
        let r = req(
            "POST",
            "daouchi://localhost/api/x?a=1&b=%20c",
            &[("content-type", "application/json"), ("range", "bytes=0-9")],
            b"{}".to_vec(),
        );
        let a = to_axum_request(r);
        assert_eq!(a.method(), Method::POST);
        assert_eq!(a.uri().to_string(), "/api/x?a=1&b=%20c");
        assert_eq!(a.headers()["content-type"], "application/json");
        assert_eq!(a.headers()["range"], "bytes=0-9");
        // Windows-style origin too
        assert_eq!(to_axum_request(req("GET", "http://daouchi.localhost/", &[], vec![])).uri().to_string(), "/");
    }

    #[tokio::test]
    async fn binary_body_roundtrip_and_headers() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let resp = Response::builder()
            .status(200)
            .header("content-type", "application/octet-stream")
            .header("x-multi", "a")
            .header("x-multi", "b")
            .body(Body::from(bytes.clone()))
            .unwrap();
        let out = from_axum_response(resp, &Method::GET, false).await;
        assert_eq!(out.body(), &bytes);
        assert_eq!(out.headers().get_all("x-multi").iter().count(), 2);
        assert_eq!(out.headers()[header::CONTENT_LENGTH], "256");
    }

    #[tokio::test]
    async fn head_has_headers_no_body() {
        let resp = Response::builder().header(header::CONTENT_LENGTH, 5).body(Body::from("hello")).unwrap();
        let out = from_axum_response(resp, &Method::HEAD, false).await;
        assert!(out.body().is_empty());
        assert_eq!(out.headers()[header::CONTENT_LENGTH], "5");
    }

    #[tokio::test]
    async fn oversized_plain_get_is_cut_to_206() {
        let total = MAX_BODY + 10;
        let chunk = vec![7u8; 1 << 20];
        let n = (total / chunk.len() as u64) + 1;
        let stream = futures::stream::iter((0..n).map(move |_| Ok::<_, std::io::Error>(axum::body::Bytes::from(chunk.clone()))));
        let resp = Response::builder()
            .header(header::CONTENT_LENGTH, total)
            .body(Body::from_stream(stream))
            .unwrap();
        let out = from_axum_response(resp, &Method::GET, false).await;
        assert_eq!(out.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(out.body().len() as u64, MAX_BODY);
        assert_eq!(out.headers()[header::CONTENT_RANGE], format!("bytes 0-{}/{total}", MAX_BODY - 1));
        assert_eq!(out.headers()[header::CONTENT_LENGTH], MAX_BODY.to_string());
        assert_eq!(out.headers()[header::ACCEPT_RANGES], "bytes");
    }

    // --- through the real router, same code path as the scheme handler ---

    #[tokio::test(flavor = "multi_thread")]
    async fn serves_index_html() {
        let r = call(req("GET", "daouchi://localhost/", &[], vec![])).await;
        assert_eq!(r.status(), 200);
        assert!(r.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/html"));
        assert!(String::from_utf8_lossy(r.body()).contains("<html"));
        let r = call(req("HEAD", "daouchi://localhost/", &[], vec![])).await;
        assert_eq!(r.status(), 200);
        assert!(r.body().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn api_json_on_temp_db() {
        let r = call(req("GET", "daouchi://localhost/api/sounds/stats", &[], vec![])).await;
        assert_eq!(r.status(), 200, "{}", String::from_utf8_lossy(r.body()));
        let v: serde_json::Value = serde_json::from_slice(r.body()).unwrap();
        assert!(v.is_object());
        let r = call(req("GET", "daouchi://localhost/api/models", &[], vec![])).await;
        assert_eq!(r.status(), 200);
        assert!(serde_json::from_slice::<serde_json::Value>(r.body()).unwrap().is_array());
        let r = call(req("GET", "daouchi://localhost/api/index/status", &[], vec![])).await;
        assert_eq!(r.status(), 200);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn audio_range_206() {
        let path = app().1.join("t.wav");
        let enc: String = path.to_string_lossy().bytes().map(|b| format!("%{b:02X}")).collect();
        let uri = format!("daouchi://localhost/audio/stream?path={enc}");
        let r = call(req("GET", &uri, &[("range", "bytes=10-109")], vec![])).await;
        assert_eq!(r.status(), 206);
        assert_eq!(r.headers()[header::CONTENT_RANGE], "bytes 10-109/1044");
        assert_eq!(r.headers()[header::CONTENT_LENGTH], "100");
        assert_eq!(r.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(r.headers()[header::CONTENT_TYPE], "audio/wav");
        assert_eq!(r.body().len(), 100);
        let full = std::fs::read(&path).unwrap();
        assert_eq!(r.body().as_slice(), &full[10..110]);
        // open-ended
        let r = call(req("GET", &uri, &[("range", "bytes=1000-")], vec![])).await;
        assert_eq!(r.status(), 206);
        assert_eq!(r.headers()[header::CONTENT_RANGE], "bytes 1000-1043/1044");
        // no Range, small file => 200 full body
        let r = call(req("GET", &uri, &[], vec![])).await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.body(), &full);
        // unsatisfiable
        assert_eq!(call(req("GET", &uri, &[("range", "bytes=5000-6000")], vec![])).await.status(), 416);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn foreign_origin_gets_no_cors_headers() {
        for (m, u) in [("GET", "/api/models"), ("OPTIONS", "/api/models"), ("GET", "/")] {
            let r = call(req(m, &format!("daouchi://localhost{u}"), &[("origin", "https://evil.example"),
                ("access-control-request-method", "DELETE")], vec![])).await;
            assert!(
                !r.headers().keys().any(|k| k.as_str().starts_with("access-control-")),
                "{m} {u} leaked CORS headers: {:?}", r.headers()
            );
        }
    }
}
