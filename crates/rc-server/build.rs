//! The admin console is embedded from `web/dist` (§14.1), which is a build
//! artefact and therefore not in version control. `rust-embed` fails at macro
//! expansion if that directory is missing, which turns "you forgot to build the
//! frontend" into an opaque proc-macro error on a fresh clone.
//!
//! Drop in a placeholder instead, so `cargo build` always works and the server
//! explains what to do when someone opens the console.

use std::path::Path;

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist");
    let index = dist.join("index.html");
    println!("cargo:rerun-if-changed={}", index.display());

    if index.exists() {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&dist) {
        println!("cargo:warning=could not create {}: {e}", dist.display());
        return;
    }
    let placeholder = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>remote-compile</title></head>
<body style="font-family:system-ui;background:#0b0f14;color:#e6edf3;padding:3rem">
<h1>remote-compile</h1>
<p>The admin console has not been built into this binary.</p>
<pre style="background:#111820;padding:1rem;border-radius:6px">cd web &amp;&amp; npm install &amp;&amp; npm run build
cargo build --release</pre>
<p>The REST API and <code>/metrics</code> work regardless.</p>
</body></html>
"#;
    if let Err(e) = std::fs::write(&index, placeholder) {
        println!("cargo:warning=could not write {}: {e}", index.display());
    }
}
