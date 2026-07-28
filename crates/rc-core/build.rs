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
        // Verdict v2 fields: rows written before evidence-backed attribution
        // must still load. Same rationale as env_hints — one field at a time.
        .field_attribute(".rc.v1.TaskResult.verdict", "#[serde(default)]")
        .field_attribute(".rc.v1.TaskResult.test_summary", "#[serde(default)]")
        .field_attribute(".rc.v1.TaskResult.units_seen_total", "#[serde(default)]")
        .field_attribute(".rc.v1.TaskResult.diag_delta", "#[serde(default)]")
        .field_attribute(".rc.v1.TaskResult.effective_plan", "#[serde(default)]")
        .field_attribute(".rc.v1.SubmitTaskReq.path_context", "#[serde(default)]")
        .field_attribute(".rc.v1.SubmitTaskReq.command_is_default", "#[serde(default)]")
        .field_attribute(".rc.v1.TaskAssignment.command_is_default", "#[serde(default)]")
        .field_attribute(".rc.v1.TaskAssignment.scope_hash", "#[serde(default)]")
        .field_attribute(".rc.v1.TaskAssignment.path_context", "#[serde(default)]")
        .field_attribute(".rc.v1.LogChunk.matched_lines", "#[serde(default)]")
        .field_attribute(".rc.v1.LogChunk.empty_reason", "#[serde(default)]")
        .field_attribute(".rc.v1.TaskStatus.queue_depth", "#[serde(default)]")
        .field_attribute(".rc.v1.TaskStatus.running", "#[serde(default)]")
        .field_attribute(".rc.v1.TaskStatus.capacity", "#[serde(default)]")
        .field_attribute(".rc.v1.TaskStatus.suggest_wait_secs", "#[serde(default)]")
        .field_attribute(".rc.v1.ProfileResp.pending_pre_commands", "#[serde(default)]")
        .compile_protos(&["proto/rc.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/rc.proto");
    Ok(())
}
