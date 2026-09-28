# Code review: admin image registry mirror (push/pull)

## Context
Feature: admin can configure external docker registry in Settings and manually push/pull env images from Images UI so fleet workers share environment images (e.g. hub.covm.net/rc-env:{short_id}).

This is NOT automatic push on build — admin-driven only. Credentials stay on workers via docker login.

## Files to review (read these thoroughly)
- crates/rc-core/proto/rc.proto — ImageMirrorOrder, ImageMirrorDone, ServerCmd/WorkerEvent
- crates/rc-server/src/config.rs — Policy image_registry_* + image_remote_ref()
- crates/rc-server/src/images.rs — dispatch_push/pull, on_mirror_done, MirrorStatus
- crates/rc-server/src/admin.rs — list_images enrichment, POST push/pull
- crates/rc-server/src/grpc_worker.rs — MirrorDone handling
- crates/rc-worker/src/docker.rs — push_image, pull_and_tag, registry credentials, split_repo_tag
- crates/rc-worker/src/runner.rs — mirror_image
- crates/rc-worker/src/main.rs — MirrorImage command
- web/src/pages/Settings.tsx — registry settings UI
- web/src/pages/Images.tsx — push/pull buttons + mirror status
- web/src/api.ts — Policy/ImageRow types

## Acceptance / review goals
Write findings to `.acp-agent/reviews/image-registry-mirror.md` with:
1. Correctness bugs (push/pull/auth/tag/digest pin compatibility)
2. Security issues (credentials, arbitrary image refs, SSRF, privilege)
3. Race conditions / multi-worker pull status
4. Missing tests
5. API/UI gaps vs product intent
6. Severity: blocker / major / minor / nit

Do not implement fixes. Read-only review. Be specific with file paths and issue descriptions.
