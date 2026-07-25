import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type AuditEntry, type Policy, type Role } from "../api";
import {
  Badge,
  Button,
  Card,
  Empty,
  ErrorBox,
  Input,
  Modal,
  Mono,
  Spinner,
  Table,
  Td,
  Th,
} from "../components/ui";
import { agoSecs, duration } from "../lib/format";

export function Settings({ role }: { role: Role }) {
  const qc = useQueryClient();
  const [draft, setDraft] = useState<Policy | null>(null);
  const [message, setMessage] = useState("");
  const [error, setError] = useState("");
  const [newToken, setNewToken] = useState<string | null>(null);

  const settings = useQuery({
    queryKey: ["settings"],
    queryFn: () => api.get<{ policy: Policy }>("/api/settings"),
  });
  useEffect(() => {
    if (settings.data) setDraft(settings.data.policy);
  }, [settings.data]);

  const admins = useQuery({
    queryKey: ["admins"],
    queryFn: () =>
      api.get<{ admins: { username: string; role: string; created_at: number }[] }>("/api/admins"),
  });

  const tokens = useQuery({
    queryKey: ["agent-tokens"],
    queryFn: () =>
      api.get<{ tokens: { hash: string; label: string; created_at: number; last_used: number }[] }>(
        "/api/agent-tokens",
      ),
  });

  const audit = useQuery({
    queryKey: ["audit"],
    queryFn: () => api.get<{ entries: AuditEntry[] }>("/api/audit?limit=50"),
  });

  const save = useMutation({
    mutationFn: (p: Policy) => api.put<{ policy: Policy }>("/api/settings", p),
    onSuccess: () => {
      setMessage("已保存");
      setError("");
      qc.invalidateQueries({ queryKey: ["settings"] });
      setTimeout(() => setMessage(""), 2500);
    },
    onError: (e: Error) => setError(e.message),
  });

  const mintToken = useMutation({
    mutationFn: (label: string) =>
      api.post<{ token: string }>("/api/agent-tokens", { label }),
    onSuccess: (d) => {
      setNewToken(d.token);
      qc.invalidateQueries({ queryKey: ["agent-tokens"] });
    },
  });

  const revokeToken = useMutation({
    mutationFn: (hash: string) => api.del(`/api/agent-tokens/${hash}`),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["agent-tokens"] }),
  });

  const addAdmin = useMutation({
    mutationFn: (body: { username: string; password: string; role: string }) =>
      api.post("/api/admins", body),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["admins"] });
      setError("");
    },
    onError: (e: Error) => setError(e.message),
  });

  const removeAdmin = useMutation({
    mutationFn: (username: string) => api.del(`/api/admins/${username}`),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["admins"] }),
    onError: (e: Error) => setError(e.message),
  });

  if (settings.isLoading || !draft) return <Spinner />;
  if (settings.isError) return <ErrorBox message={(settings.error as Error).message} />;

  const readOnly = role !== "admin";
  const set = <K extends keyof Policy>(key: K, value: Policy[K]) =>
    setDraft({ ...draft, [key]: value });

  return (
    <div className="space-y-4">
      <Card
        title="运行策略"
        action={
          !readOnly && (
            <div className="flex items-center gap-2">
              {message && <span className="text-[11.5px] text-[var(--color-ok)]">{message}</span>}
              <Button variant="primary" onClick={() => save.mutate(draft)} disabled={save.isPending}>
                保存
              </Button>
            </div>
          )
        }
      >
        {error && (
          <div className="mb-3">
            <ErrorBox message={error} />
          </div>
        )}
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-3">
          <Group title="缓存与队列">
            <NumberField
              label="任务缓存 TTL"
              hint={`当前 ${duration(draft.task_cache_ttl_secs)} — 构建并非完全确定性，无限缓存会返回过期结果（§5.1）`}
              value={draft.task_cache_ttl_secs}
              onChange={(v) => set("task_cache_ttl_secs", v)}
              readOnly={readOnly}
            />
            <NumberField
              label="排队任务 TTL"
              hint={`当前 ${duration(draft.pending_ttl_secs)} — 断连不取消任务，只有这个兜底（§5.3）`}
              value={draft.pending_ttl_secs}
              onChange={(v) => set("pending_ttl_secs", v)}
              readOnly={readOnly}
            />
            <NumberField
              label="infra 重试次数"
              hint="失败后必须换机重试，耗尽才上报 agent（§6.2）"
              value={draft.max_infra_retries}
              onChange={(v) => set("max_infra_retries", v)}
              readOnly={readOnly}
            />
            <NumberField
              label="返回给 agent 的诊断条数"
              value={draft.max_diagnostics}
              onChange={(v) => set("max_diagnostics", v)}
              readOnly={readOnly}
            />
          </Group>

          <Group title="GC 与保留">
            <NumberField
              label="CAS blob TTL"
              hint={duration(draft.blob_gc_ttl_secs)}
              value={draft.blob_gc_ttl_secs}
              onChange={(v) => set("blob_gc_ttl_secs", v)}
              readOnly={readOnly}
            />
            <NumberField
              label="日志保留"
              hint={duration(draft.log_retention_secs)}
              value={draft.log_retention_secs}
              onChange={(v) => set("log_retention_secs", v)}
              readOnly={readOnly}
            />
            <NumberField
              label="worker 离线判定"
              hint={duration(draft.worker_offline_secs)}
              value={draft.worker_offline_secs}
              onChange={(v) => set("worker_offline_secs", v)}
              readOnly={readOnly}
            />
          </Group>

          <Group title="调度打分权重（§6.1）">
            <NumberField
              label="磁盘余量 w1"
              value={draft.w_disk}
              step={0.1}
              onChange={(v) => set("w_disk", v)}
              readOnly={readOnly}
            />
            <NumberField
              label="CPU 余量 w2"
              value={draft.w_cpu}
              step={0.1}
              onChange={(v) => set("w_cpu", v)}
              readOnly={readOnly}
            />
            <NumberField
              label="缓存亲和 w3"
              hint="通常最大：命中 target volume 能省掉整轮重编"
              value={draft.w_cache_affinity}
              step={0.1}
              onChange={(v) => set("w_cache_affinity", v)}
              readOnly={readOnly}
            />
            <NumberField
              label="镜像亲和 w4"
              value={draft.w_image_affinity}
              step={0.1}
              onChange={(v) => set("w_image_affinity", v)}
              readOnly={readOnly}
            />
            <NumberField
              label="最低可用磁盘 GB"
              value={draft.min_disk_free_gb}
              onChange={(v) => set("min_disk_free_gb", v)}
              readOnly={readOnly}
            />
          </Group>

          <Group title="安全">
            <label className="flex items-start gap-2">
              <input
                type="checkbox"
                checked={draft.require_image_approval}
                disabled={readOnly}
                onChange={(e) => set("require_image_approval", e.target.checked)}
                className="mt-0.5"
              />
              <span>
                <span className="block text-[12px]">新镜像需管理员审批</span>
                <span className="block text-[11px] text-[var(--color-ink-faint)]">
                  关掉意味着 agent 提交的 Dockerfile 可以直接在编译机上构建并运行 —— 构建期是运行时沙箱管不到的攻击面（§8.3）
                </span>
              </span>
            </label>
            {!draft.require_image_approval && (
              <div className="mt-2">
                <ErrorBox message="审批已关闭：任何能访问 gRPC 端口的 agent 都能让编译机执行任意构建指令。" />
              </div>
            )}
          </Group>

          <Group title="告警与默认值">
            <TextField
              label="告警 webhook"
              hint="钉钉 / 飞书 / Slack 通用格式；留空则只在控制台显示"
              value={draft.alert_webhook}
              onChange={(v) => set("alert_webhook", v)}
              readOnly={readOnly}
            />
            <TextField
              label="默认镜像"
              value={draft.default_image}
              onChange={(v) => set("default_image", v)}
              readOnly={readOnly}
            />
          </Group>
        </div>
      </Card>

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
        <Card
          title="Agent token"
          action={
            !readOnly && (
              <Button
                onClick={() => {
                  const label = prompt("给这个 token 起个名字（如 dev-laptop-01）");
                  if (label) mintToken.mutate(label);
                }}
              >
                生成
              </Button>
            )
          }
          bodyClassName="p-0"
        >
          {tokens.data?.tokens.length === 0 ? (
            <Empty>还没有 agent token。没有 token 的 agent 无法提交任务。</Empty>
          ) : (
            <Table>
              <thead>
                <tr>
                  <Th>名称</Th>
                  <Th>指纹</Th>
                  <Th>创建</Th>
                  {!readOnly && <Th />}
                </tr>
              </thead>
              <tbody>
                {tokens.data?.tokens.map((t) => (
                  <tr key={t.hash}>
                    <Td>{t.label}</Td>
                    <Td>
                      <Mono className="text-[var(--color-ink-faint)]">{t.hash.slice(0, 12)}</Mono>
                    </Td>
                    <Td className="text-[var(--color-ink-dim)]">{agoSecs(t.created_at)}</Td>
                    {!readOnly && (
                      <Td>
                        <div className="flex justify-end">
                          <Button variant="danger" onClick={() => revokeToken.mutate(t.hash)}>
                            吊销
                          </Button>
                        </div>
                      </Td>
                    )}
                  </tr>
                ))}
              </tbody>
            </Table>
          )}
        </Card>

        <Card title="控制台账号" bodyClassName="p-0">
          <Table>
            <thead>
              <tr>
                <Th>用户名</Th>
                <Th>角色</Th>
                <Th>创建</Th>
                {!readOnly && <Th />}
              </tr>
            </thead>
            <tbody>
              {admins.data?.admins.map((a) => (
                <tr key={a.username}>
                  <Td>{a.username}</Td>
                  <Td>
                    <Badge tone={a.role === "admin" ? "accent" : "muted"}>{a.role}</Badge>
                  </Td>
                  <Td className="text-[var(--color-ink-dim)]">{agoSecs(a.created_at)}</Td>
                  {!readOnly && (
                    <Td>
                      <div className="flex justify-end">
                        <Button variant="danger" onClick={() => removeAdmin.mutate(a.username)}>
                          删除
                        </Button>
                      </div>
                    </Td>
                  )}
                </tr>
              ))}
            </tbody>
          </Table>
          {!readOnly && (
            <NewAdminForm
              onSubmit={(username, password, role) => addAdmin.mutate({ username, password, role })}
            />
          )}
        </Card>
      </div>

      <Card title="审计日志" bodyClassName="p-0">
        {audit.data?.entries.length === 0 ? (
          <Empty>还没有操作记录。</Empty>
        ) : (
          <Table>
            <thead>
              <tr>
                <Th>时间</Th>
                <Th>操作者</Th>
                <Th>动作</Th>
                <Th>对象</Th>
                <Th>详情</Th>
              </tr>
            </thead>
            <tbody>
              {audit.data?.entries.map((e) => (
                <tr key={e.id}>
                  <Td className="text-[var(--color-ink-faint)]">{agoSecs(e.at)}</Td>
                  <Td>
                    <Mono>{e.actor}</Mono>
                  </Td>
                  <Td>{e.action}</Td>
                  <Td>
                    <Mono className="text-[var(--color-ink-dim)]">{e.target}</Mono>
                  </Td>
                  <Td className="text-[var(--color-ink-dim)]">{e.detail}</Td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}
      </Card>

      {newToken && (
        <Modal title="Agent token" onClose={() => setNewToken(null)}>
          <p className="mb-3 text-[12px] text-[var(--color-ink-dim)]">
            只显示这一次。放进开发机的 rc-agent 配置：
          </p>
          <div className="rounded border border-[var(--color-line)] bg-[var(--color-surface)] p-3 font-[var(--font-mono)] text-[11.5px] break-all">
            rc-agent configure --server {location.protocol}//{location.hostname}:7701 --token{" "}
            {newToken}
          </div>
          <div className="mt-4 flex justify-end gap-2">
            <Button onClick={() => navigator.clipboard?.writeText(newToken)}>复制</Button>
            <Button variant="primary" onClick={() => setNewToken(null)}>
              我已保存
            </Button>
          </div>
        </Modal>
      )}
    </div>
  );
}

function Group({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="rounded border border-[var(--color-line-soft)] bg-[var(--color-surface)] p-3">
      <h3 className="mb-2 text-[11px] font-semibold tracking-wide text-[var(--color-ink-dim)] uppercase">
        {title}
      </h3>
      <div className="space-y-2.5">{children}</div>
    </div>
  );
}

function NumberField({
  label,
  hint,
  value,
  onChange,
  readOnly,
  step,
}: {
  label: string;
  hint?: string;
  value: number;
  onChange: (v: number) => void;
  readOnly?: boolean;
  step?: number;
}) {
  return (
    <div>
      <div className="flex items-center justify-between gap-3">
        <label className="text-[12px]">{label}</label>
        <Input
          type="number"
          step={step ?? 1}
          value={value}
          readOnly={readOnly}
          onChange={(e) => onChange(Number(e.target.value))}
          className="tnum w-32 text-right"
        />
      </div>
      {hint && <p className="mt-0.5 text-[11px] text-[var(--color-ink-faint)]">{hint}</p>}
    </div>
  );
}

function TextField({
  label,
  hint,
  value,
  onChange,
  readOnly,
}: {
  label: string;
  hint?: string;
  value: string;
  onChange: (v: string) => void;
  readOnly?: boolean;
}) {
  return (
    <div>
      <label className="text-[12px]">{label}</label>
      <Input
        value={value}
        readOnly={readOnly}
        onChange={(e) => onChange(e.target.value)}
        className="mt-1 w-full"
      />
      {hint && <p className="mt-0.5 text-[11px] text-[var(--color-ink-faint)]">{hint}</p>}
    </div>
  );
}

function NewAdminForm({
  onSubmit,
}: {
  onSubmit: (username: string, password: string, role: string) => void;
}) {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [role, setRole] = useState("viewer");
  return (
    <form
      className="flex flex-wrap items-center gap-2 border-t border-[var(--color-line-soft)] px-4 py-3"
      onSubmit={(e) => {
        e.preventDefault();
        onSubmit(username, password, role);
        setUsername("");
        setPassword("");
      }}
    >
      <Input
        placeholder="用户名"
        value={username}
        onChange={(e) => setUsername(e.target.value)}
        className="w-32"
      />
      <Input
        placeholder="密码（≥8 位）"
        type="password"
        value={password}
        onChange={(e) => setPassword(e.target.value)}
        className="w-40"
      />
      <select
        value={role}
        onChange={(e) => setRole(e.target.value)}
        className="h-7 rounded border border-[var(--color-line)] bg-[var(--color-surface)] px-2 text-[12px]"
      >
        <option value="viewer">viewer（只读）</option>
        <option value="admin">admin</option>
      </select>
      <Button type="submit" variant="primary">
        添加
      </Button>
    </form>
  );
}
