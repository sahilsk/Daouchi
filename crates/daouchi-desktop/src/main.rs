//! Daouchi desktop shell. The web UI is served to the webview through a Tauri custom URI scheme
//! (`daouchi://localhost/` on macOS/Linux, `http://daouchi.localhost/` on Windows). Each request is
//! dispatched in-process into the axum `Router` (`daouchi::transport::dispatch`): the app never opens a
//! TCP/localhost listening port, so no other process or web page can reach the API.
//! Native drag-out of exported clips uses tauri-plugin-drag.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// Custom URI scheme name served by the in-process router.
const SCHEME: &str = "daouchi";

#[cfg(not(windows))]
const START_URL: &str = "daouchi://localhost/";
#[cfg(windows)]
const START_URL: &str = "http://daouchi.localhost/";

fn main() {
    // Libraries (symphonia, ort, hyper, ...) only log warnings and errors; our own crates log info.
    // RUST_LOG overrides this, e.g. RUST_LOG=debug ./dev.sh
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,daouchi=info,daouchi_ai=info,daouchi_desktop=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Storage + DB + engines + Router. Binds nothing. The tauri async runtime is tokio.
    let core = tauri::async_runtime::block_on(async { daouchi::build_app() }).expect("failed to initialise Daouchi");
    let router = core.router.clone();
    // Keep the shared state (DB pool, engines) alive for the whole run.
    let keep_alive = core.state.clone();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_drag::init())
        .register_asynchronous_uri_scheme_protocol(SCHEME, move |_ctx, request, responder| {
            let router = router.clone();
            tauri::async_runtime::spawn(async move {
                responder.respond(daouchi::transport::dispatch(router, request).await);
            });
        })
        .setup(move |app| {
            app.manage(keep_alive.clone());
            tracing::info!("Daouchi UI at {START_URL} (in-process, no listening port)");
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(START_URL.parse()?))
                .title("Daouchi")
                .inner_size(1440.0, 900.0)
                .min_inner_size(1100.0, 680.0)
                .center()
                .build()?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build Daouchi desktop app");

    // On exit tauri leaves through process::exit(0), so libc atexit handlers (the ONNX Runtime environment
    // release registered in daouchi-ai) run exactly once. There is no server to shut down.
    app.run(|_handle, _event| {});
}
