# Desktop shell and packaging

Crate: `crates/daouchi-desktop` (Tauri v2). It calls `daouchi::build_app()` and registers an asynchronous Tauri URI scheme `daouchi`; every webview request is run through the axum `Router` in-process. No TCP port is opened. How the transport behaves (origins, Range handling, the 64 MiB cap, polling instead of SSE) is described in [ARCHITECTURE.md](ARCHITECTURE.md#transport).

Versions: tauri 2.12, tauri-build 2.7, tauri-plugin-single-instance 2.5, tauri-plugin-drag 2.1, rustc 1.91.

The window loads `daouchi://localhost/` (macOS/Linux) or `http://daouchi.localhost/` (Windows). The web UI is embedded in the binary (rust-embed of `web/`); `DAOUCHI_WEB_DIR=/path/to/web` serves it from disk instead. `capabilities/default.json` lists the two scheme origins.

## Run

```bash
export CARGO_TARGET_DIR=$HOME/.cache/daouchi-target ORT_DYLIB_PATH=$HOME/.daouchi/lib/libonnxruntime.dylib
cargo run -p daouchi-desktop                              # desktop window
DAOUCHI_WEB_DIR=$PWD/web cargo run -p daouchi-desktop     # edit web/ without rebuilding
```

`./start.sh` and `./dev.sh` wrap these.

## Bundle (macOS, unsigned)

```bash
scripts/bundle-macos.sh [debug|release]      # default release -> $CARGO_TARGET_DIR/bundle/macos/Daouchi.app
```

The script builds `daouchi-desktop` (release builds enable the `custom-protocol` feature), assembles `Daouchi.app`, writes `Info.plist` (bundle id `com.daouchi.app`, minimum macOS 12.0) and places the ONNX Runtime library in `Contents/Frameworks/` as `libonnxruntime.<version>.dylib` plus a `libonnxruntime.dylib` symlink. The runtime probes `<exe>/../Frameworks`. The source library is `$ORT_DYLIB_PATH` (default `~/.daouchi/lib/libonnxruntime.dylib`).

With the Tauri CLI (`cargo install tauri-cli --version '^2' --locked`), from `crates/daouchi-desktop`:

```bash
cp "$ORT_DYLIB_PATH" "$STAGE/libonnxruntime.dylib"     # a real file, not a symlink
echo '{"bundle":{"macOS":{"files":{"Frameworks/libonnxruntime.dylib":"'$STAGE'/libonnxruntime.dylib"}}}}' > "$STAGE/ort.json"
cargo tauri build --bundles app,dmg --config "$STAGE/ort.json"
```

## Signing and notarization

Release builds are not yet signed. The steps for when a Developer ID certificate is available:

1. Sign inside-out: the ONNX Runtime dylib first, then the app, with the hardened runtime.
   `codesign --force --options runtime --timestamp -s "Developer ID Application: NAME (TEAM)" Daouchi.app/Contents/Frameworks/*.dylib`
   `codesign --force --options runtime --timestamp --entitlements entitlements.plist -s "..." Daouchi.app`
2. Entitlements under the hardened runtime: `com.apple.security.cs.disable-library-validation` (the ONNX Runtime dylib is loaded with `dlopen` and is signed by a different identity unless re-signed with yours) and, if CoreML compiles models in-process, `com.apple.security.cs.allow-jit` and `allow-unsigned-executable-memory`.
3. Zip or build a dmg, then `xcrun notarytool submit X.dmg --apple-id ... --team-id ... --password <app-specific> --wait` and `xcrun stapler staple X.dmg`. With the Tauri CLI set `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` and `APPLE_TEAM_ID`.

## Windows and Linux (not yet built)

- Windows: ship `onnxruntime.dll` next to the executable (the runtime checks `<exe dir>`), configure the WebView2 bootstrapper (`bundle.windows.webviewInstallMode`), build NSIS/MSI with `cargo tauri build`, sign with `signtool`.
- Linux: needs `webkit2gtk-4.1` and libxdo development packages; bundle `libonnxruntime.so` into the AppImage or deb `lib/` and extend the runtime's probe directories (`<exe>/../lib`); drag-out needs GTK.
