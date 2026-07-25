//! gRPC client for the control plane.
//!
//! Every failure mode here has one rule (§12): never pretend success, never
//! retry forever. When the control plane is unreachable the agent is told to
//! run `cargo check` locally instead.

use anyhow::{anyhow, Result};
use rc_core::cas;
use rc_core::pb::agent_api_client::AgentApiClient;
use rc_core::pb::*;
use tonic::transport::Channel;
use tonic::{Request, Status};

#[derive(Clone)]
pub struct AgentClient {
    inner: AgentApiClient<Channel>,
    token: String,
}

impl AgentClient {
    pub async fn connect(server: &str, token: &str) -> Result<Self> {
        let channel = rc_core::transport::endpoint(server)?
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect()
            .await
            .map_err(|e| unreachable_error(server, e))?;
        Ok(AgentClient {
            inner: AgentApiClient::new(channel).max_decoding_message_size(64 * 1024 * 1024),
            token: token.to_string(),
        })
    }

    fn authed<T>(&self, message: T) -> Request<T> {
        let mut req = Request::new(message);
        if !self.token.is_empty() {
            if let Ok(v) = format!("Bearer {}", self.token).parse() {
                req.metadata_mut().insert("authorization", v);
            }
        }
        req
    }

    /// Ask which blobs are missing. Answering also renews the lease on the
    /// ones the server already holds (§4.7).
    pub async fn check_blobs(&mut self, hashes: Vec<String>, session: &str) -> Result<Vec<String>> {
        if hashes.is_empty() {
            return Ok(vec![]);
        }
        let resp = self
            .inner
            .check_blobs(self.authed(CheckBlobsReq {
                hashes,
                agent_session: session.to_string(),
            }))
            .await
            .map_err(status_error)?;
        Ok(resp.into_inner().missing)
    }

    pub async fn upload_blobs(&mut self, blobs: Vec<(String, Vec<u8>)>) -> Result<u32> {
        if blobs.is_empty() {
            return Ok(0);
        }
        let mut chunks = Vec::new();
        for (hash, data) in blobs {
            if data.is_empty() {
                chunks.push(BlobChunk {
                    hash,
                    data: vec![],
                    last: true,
                    total_size: 0,
                });
                continue;
            }
            let total = data.len() as u64;
            let n = data.len();
            for (i, part) in data.chunks(cas::CHUNK_SIZE).enumerate() {
                chunks.push(BlobChunk {
                    hash: hash.clone(),
                    data: part.to_vec(),
                    last: (i + 1) * cas::CHUNK_SIZE >= n,
                    total_size: total,
                });
            }
        }
        let resp = self
            .inner
            .upload_blobs(self.authed(tokio_stream::iter(chunks)))
            .await
            .map_err(status_error)?
            .into_inner();
        if !resp.rejected.is_empty() {
            return Err(anyhow!(
                "control plane rejected {} blob(s) as corrupt",
                resp.rejected.len()
            ));
        }
        Ok(resp.accepted)
    }

    pub async fn get_baseline(&mut self, project_id: &str, commit: &str) -> Result<BaselineResp> {
        Ok(self
            .inner
            .get_baseline(self.authed(BaselineReq {
                project_id: project_id.to_string(),
                base_commit: commit.to_string(),
            }))
            .await
            .map_err(status_error)?
            .into_inner())
    }

    pub async fn register_bundle(&mut self, upload: BundleUpload) -> Result<()> {
        self.inner
            .register_bundle(self.authed(upload))
            .await
            .map_err(status_error)?;
        Ok(())
    }

    pub async fn submit(&mut self, req: SubmitTaskReq) -> Result<TaskHandle> {
        Ok(self
            .inner
            .submit_task(self.authed(req))
            .await
            .map_err(status_error)?
            .into_inner())
    }

    pub async fn get_task(&mut self, task_id: &str, wait_secs: u32) -> Result<TaskStatus> {
        Ok(self
            .inner
            .get_task(self.authed(TaskQuery {
                task_id: task_id.to_string(),
                wait_secs,
            }))
            .await
            .map_err(status_error)?
            .into_inner())
    }

    pub async fn get_log(&mut self, query: LogQuery) -> Result<LogChunk> {
        Ok(self
            .inner
            .get_log(self.authed(query))
            .await
            .map_err(status_error)?
            .into_inner())
    }

    pub async fn get_profile(&mut self, project_id: &str, path: &str) -> Result<ProfileResp> {
        Ok(self
            .inner
            .get_profile(self.authed(GetProfileReq {
                project_id: project_id.to_string(),
                path: path.to_string(),
            }))
            .await
            .map_err(status_error)?
            .into_inner())
    }

    pub async fn upsert_profile(&mut self, req: UpsertProfileReq) -> Result<ProfileResp> {
        Ok(self
            .inner
            .upsert_profile(self.authed(req))
            .await
            .map_err(status_error)?
            .into_inner())
    }

    pub async fn list_envs(&mut self, req: ListEnvsReq) -> Result<Vec<EnvImage>> {
        Ok(self
            .inner
            .list_envs(self.authed(req))
            .await
            .map_err(status_error)?
            .into_inner()
            .envs)
    }

    pub async fn prepare_env(&mut self, req: PrepareEnvReq) -> Result<EnvStatus> {
        Ok(self
            .inner
            .prepare_env(self.authed(req))
            .await
            .map_err(status_error)?
            .into_inner())
    }

    pub async fn get_env_status(&mut self, env_id: &str) -> Result<EnvStatus> {
        Ok(self
            .inner
            .get_env_status(self.authed(EnvQuery {
                env_id: env_id.to_string(),
            }))
            .await
            .map_err(status_error)?
            .into_inner())
    }

    pub async fn list_workers(&mut self) -> Result<ListWorkersResp> {
        Ok(self
            .inner
            .list_workers(self.authed(Empty {}))
            .await
            .map_err(status_error)?
            .into_inner())
    }
}

/// §12: when the control plane is down, say so plainly and point at the local
/// fallback. Silently degrading would be worse than not existing.
fn unreachable_error(server: &str, e: impl std::fmt::Display) -> anyhow::Error {
    anyhow!(
        "控制面不可达 ({server}: {e})。远程编译不可用，请改为本地执行 `cargo check`。"
    )
}

fn status_error(status: Status) -> anyhow::Error {
    match status.code() {
        tonic::Code::Unavailable => anyhow!(
            "控制面不可达 ({})。远程编译不可用，请改为本地执行 `cargo check`。",
            status.message()
        ),
        tonic::Code::Unauthenticated => anyhow!(
            "认证失败：{}。检查 rc-agent 配置中的 token（用 `rc-server agent-token` 生成）。",
            status.message()
        ),
        _ => anyhow!("{}", status.message()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreachable_control_plane_tells_the_agent_what_to_do_instead() {
        let e = unreachable_error("http://127.0.0.1:1", "connection refused");
        let text = e.to_string();
        assert!(text.contains("cargo check"), "{text}");
        assert!(text.contains("控制面不可达"));
    }

    #[test]
    fn unavailable_is_translated_into_the_local_fallback_advice() {
        let e = status_error(Status::unavailable("connect error"));
        assert!(e.to_string().contains("cargo check"));
    }

    #[test]
    fn an_auth_failure_names_the_fix() {
        let e = status_error(Status::unauthenticated("unknown agent token"));
        assert!(e.to_string().contains("agent-token"));
    }

    #[test]
    fn other_errors_pass_through_verbatim() {
        let e = status_error(Status::failed_precondition("image digest is not approved yet"));
        assert_eq!(e.to_string(), "image digest is not approved yet");
    }

    #[tokio::test]
    async fn connecting_to_a_dead_port_fails_fast_with_advice() {
        let err = match AgentClient::connect("http://127.0.0.1:1", "").await {
            Ok(_) => panic!("nothing should be listening on port 1"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("cargo check"), "{err}");
    }
}
