fn main() {
    // `bundle.resources` names the FrankenPHP directory, which
    // `scripts/setup-php-runtime.mjs` fills and git ignores. A checkout without
    // it (CI, a fresh clone) still has to compile, so make the directory; the
    // app then reports the runtime as not installed.
    let resources = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/frankenphp");
    if !resources.exists() {
        std::fs::create_dir_all(&resources).expect("create the frankenphp resource directory");
    }
    tauri_build::build()
}
