use std::path::Path;

/// exFAT/AppleDouble volumes create `._*` sidecar files that tauri-build tries to parse as capabilities.
fn clean_appledouble(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                clean_appledouble(&p);
            } else if e.file_name().to_string_lossy().starts_with("._") {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

fn main() {
    for d in ["capabilities", "icons", "frontend"] {
        clean_appledouble(Path::new(d));
    }
    tauri_build::build()
}
