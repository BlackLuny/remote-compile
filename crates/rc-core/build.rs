fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        // Wire types double as storage/REST types; serde keeps a single source of truth.
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .compile_protos(&["proto/rc.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/rc.proto");
    Ok(())
}
