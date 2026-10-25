//! Static web UI: embedded into the binary with rust-embed (no CWD dependence).
//! `DAOUCHI_WEB_DIR=/path/to/web` serves from disk instead (dev override, read per request).
use axum::{
    body::Body,
    http::{header, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;
use std::path::{Component, Path, PathBuf};

#[derive(RustEmbed)]
#[folder = "web/"]
#[exclude = "._*"]
#[exclude = "*/._*"]
#[exclude = ".DS_Store"]
#[exclude = "*/.DS_Store"]
struct WebAssets;

fn content_type(path: &str) -> String {
    let m = mime_guess::from_path(path).first_or_octet_stream();
    let essence = m.essence_str();
    if essence.starts_with("text/") || essence == "application/javascript" || essence == "application/json" {
        format!("{essence}; charset=utf-8")
    } else {
        essence.to_string()
    }
}

fn disk_root() -> Option<PathBuf> {
    std::env::var_os("DAOUCHI_WEB_DIR").filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn load(rel: &str) -> Option<Vec<u8>> {
    if let Some(root) = disk_root() {
        let p = Path::new(rel);
        if p.components().any(|c| !matches!(c, Component::Normal(_))) {
            return None;
        }
        return std::fs::read(root.join(p)).ok();
    }
    WebAssets::get(rel).map(|f| f.data.into_owned())
}

fn ok(rel: &str, bytes: Vec<u8>, head: bool) -> Response {
    let len = bytes.len();
    let body = if head { Body::empty() } else { Body::from(bytes) };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(rel))
        .header(header::CONTENT_LENGTH, len)
        .body(body)
        .unwrap()
}

/// Router fallback. `/` -> index.html; existing file -> file; unknown path with a file extension -> 404;
/// unknown extension-less path -> index.html (SPA-friendly).
pub async fn serve(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let head = method == Method::HEAD;
    let rel = uri.path().trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    // never serve API-looking paths as SPA pages
    if rel.starts_with("api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(b) = load(rel) {
        return ok(rel, b, head);
    }
    let has_ext = Path::new(rel).extension().is_some();
    if !has_ext {
        if let Some(b) = load("index.html") {
            return ok("index.html", b, head);
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::Request, Router};
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new().fallback(serve)
    }
    async fn get(uri: &str) -> (StatusCode, String, Vec<u8>) {
        let r = app().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let st = r.status();
        let ct = r.headers().get(header::CONTENT_TYPE).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
        let b = axum::body::to_bytes(r.into_body(), 64 << 20).await.unwrap().to_vec();
        (st, ct, b)
    }

    // Env var tests share process state; run them in one test.
    #[tokio::test]
    async fn embedded_and_override() {
        std::env::remove_var("DAOUCHI_WEB_DIR");
        let (st, ct, b) = get("/").await;
        assert_eq!(st, 200);
        assert_eq!(ct, "text/html; charset=utf-8");
        assert!(String::from_utf8_lossy(&b).contains("<html"));
        let (st, ct, _) = get("/app.js").await;
        assert_eq!(st, 200);
        assert!(ct.starts_with("text/javascript") || ct.starts_with("application/javascript"), "{ct}");
        let (st, ct, _) = get("/style.css").await;
        assert_eq!((st.as_u16(), ct.as_str()), (200, "text/css; charset=utf-8"));
        assert_eq!(get("/components/player.js").await.0, 200);
        // unknown path with extension => 404; extension-less => SPA index
        assert_eq!(get("/nope.js").await.0, 404);
        assert_eq!(get("/api/nope").await.0, 404);
        let (st, ct, b) = get("/some/route").await;
        assert_eq!(st, 200);
        assert!(ct.starts_with("text/html"));
        assert!(String::from_utf8_lossy(&b).contains("<html"));
        // the UI must be self-contained: no external script hosts, vendored libs are served
        let (_, _, idx) = get("/").await;
        let idx = String::from_utf8_lossy(&idx);
        assert!(!idx.contains("src=\"http"), "index.html loads a script from an external host");
        // Alpine only evaluates directives inside #app; anything after it would render unbound
        // (always visible, empty text), so no directive may follow the closing marker.
        let tail = &idx[idx.find("<!-- /#app -->").expect("missing /#app marker")..];
        for d in [" x-show=", " x-text=", " x-for=", " x-if=", " x-bind", " @click", " :class=", " :style="] {
            assert!(!tail.contains(d), "Alpine directive `{d}` outside #app");
        }
        for lib in ["alpine-3.17.5.min.js", "wavesurfer-7.12.12.min.js", "tailwind-play-3.4.17.js"] {
            assert!(idx.contains(&format!("/vendor/{lib}")), "{lib} not referenced");
            let (st, ct, body) = get(&format!("/vendor/{lib}")).await;
            assert_eq!(st, 200, "{lib}");
            assert!(ct.contains("javascript"), "{lib}: {ct}");
            assert!(body.len() > 10_000, "{lib} looks truncated");
        }
        // traversal never escapes
        assert_eq!(get("/..%2f..%2fCargo.toml").await.0, 404);

        // disk override
        let d = std::env::temp_dir().join(format!("daouchi-web-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("index.html"), "<html>override</html>").unwrap();
        std::fs::write(d.join("x.js"), "1").unwrap();
        std::env::set_var("DAOUCHI_WEB_DIR", &d);
        let (st, _, b) = get("/").await;
        assert_eq!(st, 200);
        assert_eq!(b, b"<html>override</html>");
        assert_eq!(get("/x.js").await.0, 200);
        assert_eq!(get("/app.js").await.0, 404);
        assert_eq!(get("/%2e%2e/secret.txt").await.0, 404);
        std::env::remove_var("DAOUCHI_WEB_DIR");
        let _ = std::fs::remove_dir_all(&d);
    }
}
