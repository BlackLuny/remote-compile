# Worker limits and explicit placement

Workers report the effective per-container memory, CPU and PID limits on each heartbeat, after CPU clamping. Online console and agent views display these limits; older or disconnected workers report them as unknown. Parallel capacity is separate from the per-container limits.

`check` accepts an optional worker ID. A pinned submission uses a dedicated gRPC method so an older server cannot silently ignore the placement requirement. The server validates the enrolled identity, persists the pin with the task, and only dispatches to that worker subject to all existing safety and compatibility gates. Busy, offline, draining or otherwise ineligible workers leave the task queued. Infrastructure retries never fall back to another worker. Placement explanations include pin mismatch and offline identity. A pinned invocation bypasses both result caches and cross-task deduplication so a request to execute there actually executes there. It has a separate supersession scope from unpinned tasks and other worker pins.

Deployment is server first, then workers and agents. New fields are additive; old workers show unknown limits and old agents keep existing placement behavior. An agent speaking to an old server receives UNIMPLEMENTED for pinned requests rather than silently running elsewhere.

Rollback must first cancel or finish every queued pinned task before starting an older server: an old scheduler does not understand the persisted placement column. The additive SQLite column itself is safe for old binaries to ignore once no pinned tasks remain.
