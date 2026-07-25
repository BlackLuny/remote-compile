fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        // Wire types double as storage/REST types; serde keeps a single source of truth.
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        // Rows outlive the schema that wrote them: a `TaskResult` stored before
        // this field existed has no `env_hints`, and without a default it would
        // fail to deserialise — which `TaskRow::result()` turns into a silently
        // empty result rather than an error anyone would notice.
        //
        // Deliberately one field, not one message and not `.`. Defaulting a
        // whole message would let a *truncated* row deserialise into mostly
        // defaults, and `rc-server` relies on the opposite: `try_dispatch`
        // fails a task outright when its stored profile or manifest is
        // unreadable, because a silently-emptied profile goes to a worker and
        // builds the wrong thing. A new persisted field needs its own line
        // here, and needs the same argument made for it.
        .field_attribute(".rc.v1.TaskResult.env_hints", "#[serde(default)]")
        .compile_protos(&["proto/rc.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/rc.proto");
    Ok(())
}
