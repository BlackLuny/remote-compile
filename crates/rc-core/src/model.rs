//! Domain enums shared by every process. These are stored as strings so that
//! a SQLite row remains readable and an unknown value from a newer peer
//! degrades instead of panicking.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

macro_rules! str_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }, default = $default:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = ();
            fn from_str(s: &str) -> Result<Self, ()> {
                match s {
                    $($text => Ok($name::$variant),)+
                    _ => Err(()),
                }
            }
        }

        impl $name {
            /// Lenient parse used on wire/storage boundaries.
            pub fn parse_or_default(s: &str) -> Self {
                s.parse().unwrap_or($name::$default)
            }
        }
    };
}

str_enum!(TaskType {
    Check => "check",
    Build => "build",
    Test => "test",
    Clippy => "clippy",
    Custom => "custom",
}, default = Check);

str_enum!(TaskState {
    Pending => "pending",
    Syncing => "syncing",
    Queued => "queued",
    Running => "running",
    Uploading => "uploading",
    Done => "done",
    Failed => "failed",
    Canceled => "canceled",
    Superseded => "superseded",
}, default = Pending);

str_enum!(ResultKind {
    Success => "success",
    CompileError => "compile_error",
    EnvError => "env_error",
    InfraError => "infra_error",
    Timeout => "timeout",
}, default = InfraError);

str_enum!(ImageStatus {
    PendingApproval => "pending_approval",
    Building => "building",
    Healthy => "healthy",
    Failing => "failing",
    Rejected => "rejected",
}, default = PendingApproval);

str_enum!(WorkerStatus {
    Online => "online",
    Draining => "draining",
    Offline => "offline",
}, default = Offline);

impl TaskState {
    /// Terminal states release CAS pins and never transition again.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskState::Done | TaskState::Failed | TaskState::Canceled | TaskState::Superseded
        )
    }

    /// Only tasks that have not begun executing may be superseded (§5.2).
    pub fn is_cancelable(&self) -> bool {
        matches!(self, TaskState::Pending | TaskState::Syncing | TaskState::Queued)
    }
}

impl ResultKind {
    /// `infra_error` is retried on a different worker without telling the
    /// agent (§3.5); everything else is the caller's problem.
    pub fn is_retryable(&self) -> bool {
        matches!(self, ResultKind::InfraError)
    }

    /// Whether a result of this kind may be served from the task cache.
    /// Infrastructure failures must never be cached.
    pub fn is_cacheable(&self) -> bool {
        matches!(self, ResultKind::Success | ResultKind::CompileError)
    }

    /// What the agent should do next — surfaced verbatim through MCP so the
    /// coding agent does not have to guess.
    pub fn agent_hint(&self) -> &'static str {
        match self {
            ResultKind::Success => "编译通过，继续。",
            ResultKind::CompileError => "代码问题：按结构化诊断修改源码。",
            ResultKind::EnvError => "环境缺依赖：用 list_envs 找可用镜像，或 prepare_env 提交 Dockerfile。",
            ResultKind::InfraError => "基础设施故障，系统已自动换机重试并耗尽次数；无需修改代码，稍后重试。",
            ResultKind::Timeout => "任务超时：拆分任务或调大 profile 中的 timeout_secs。",
        }
    }
}

// Role of an admin console user (§14.2).
str_enum!(Role {
    Admin => "admin",
    Viewer => "viewer",
}, default = Viewer);

impl Role {
    pub fn can_write(&self) -> bool {
        matches!(self, Role::Admin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_strings() {
        for k in ResultKind::ALL {
            assert_eq!(ResultKind::parse_or_default(k.as_str()), *k);
        }
        for s in TaskState::ALL {
            assert_eq!(TaskState::parse_or_default(s.as_str()), *s);
        }
    }

    #[test]
    fn unknown_state_does_not_panic() {
        assert_eq!(TaskState::parse_or_default("from_the_future"), TaskState::Pending);
    }

    #[test]
    fn only_unstarted_tasks_are_cancelable() {
        assert!(TaskState::Queued.is_cancelable());
        assert!(!TaskState::Running.is_cancelable());
        assert!(!TaskState::Done.is_cancelable());
    }

    #[test]
    fn infra_errors_are_never_cached() {
        assert!(!ResultKind::InfraError.is_cacheable());
        assert!(!ResultKind::Timeout.is_cacheable());
        assert!(ResultKind::CompileError.is_cacheable());
    }
}
