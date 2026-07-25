import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type Profile, type Role } from "../api";
import {
  Badge,
  Button,
  Card,
  Empty,
  ErrorBox,
  Modal,
  Mono,
  Spinner,
  Table,
  Td,
  Th,
} from "../components/ui";
import { agoSecs, percent, shortDigest } from "../lib/format";

export function Profiles({ role }: { role: Role }) {
  const qc = useQueryClient();
  const [editing, setEditing] = useState<Profile | null>(null);
  const [draft, setDraft] = useState("");
  const [error, setError] = useState("");

  const q = useQuery({
    queryKey: ["profiles"],
    queryFn: () => api.get<{ profiles: Profile[] }>("/api/profiles"),
  });

  const save = useMutation({
    mutationFn: (p: Profile) =>
      api.put<{ ok: boolean; unknown_keys: string[] }>(`/api/profiles/${p.id}`, {
        project_id: p.project_id,
        path: p.path,
        config_toml: draft,
      }),
    onSuccess: (data) => {
      qc.invalidateQueries({ queryKey: ["profiles"] });
      if (data.unknown_keys.length > 0) {
        setError(`已保存，但忽略了未知字段: ${data.unknown_keys.join(", ")}`);
      } else {
        setEditing(null);
        setError("");
      }
    },
    onError: (e: Error) => setError(e.message),
  });

  const remove = useMutation({
    mutationFn: (id: string) => api.del(`/api/profiles/${id}`),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["profiles"] }),
  });

  if (q.isLoading) return <Spinner />;
  if (q.isError) return <ErrorBox message={(q.error as Error).message} />;

  return (
    <div className="space-y-4">
      <Card title="构建档案" bodyClassName="p-0">
        <div className="border-b border-[var(--color-line-soft)] px-4 py-2.5 text-[11.5px] text-[var(--color-ink-dim)]">
          一个 agent 摸索出的构建方式，整个 fleet 共享。仓库内的{" "}
          <Mono>.remote-compile.toml</Mono> 优先级更高，这里存的是兜底。
        </div>
        {q.data!.profiles.length === 0 ? (
          <Empty>还没有沉淀出构建档案。第一次成功编译后会自动写入。</Empty>
        ) : (
          <Table>
            <thead>
              <tr>
                <Th>Project</Th>
                <Th>子路径</Th>
                <Th>适配器</Th>
                <Th>镜像</Th>
                <Th>成功率</Th>
                <Th>最近成功</Th>
                <Th>创建者</Th>
                <Th />
              </tr>
            </thead>
            <tbody>
              {q.data!.profiles.map((p) => (
                <tr key={p.id} className="hover:bg-[var(--color-panel-2)]/50">
                  <Td>
                    <Mono>{p.project_id}</Mono>
                  </Td>
                  <Td className="text-[var(--color-ink-dim)]">{p.path || "（根）"}</Td>
                  <Td>
                    <Badge tone="accent">{p.adapter || "auto"}</Badge>
                  </Td>
                  <Td>
                    <Mono className="text-[var(--color-ink-dim)]">
                      {p.image ? shortDigest(p.image) : "–"}
                    </Mono>
                  </Td>
                  <Td className="tnum">
                    {p.total_count === 0 ? "–" : percent(p.success_count / p.total_count)}
                    <span className="ml-1 text-[11px] text-[var(--color-ink-faint)]">
                      / {p.total_count}
                    </span>
                  </Td>
                  <Td className="text-[var(--color-ink-dim)]">{agoSecs(p.last_success_at)}</Td>
                  <Td>
                    <Mono className="text-[var(--color-ink-faint)]">{p.created_by || "–"}</Mono>
                  </Td>
                  <Td>
                    <div className="flex justify-end gap-1.5">
                      <Button
                        onClick={() => {
                          setEditing(p);
                          setDraft(p.config_toml);
                          setError("");
                        }}
                      >
                        {role === "admin" ? "编辑" : "查看"}
                      </Button>
                      {role === "admin" && (
                        <Button
                          variant="danger"
                          onClick={() => {
                            if (confirm("删除这个档案？下次 check 会重新自动探测。")) {
                              remove.mutate(p.id);
                            }
                          }}
                        >
                          删除
                        </Button>
                      )}
                    </div>
                  </Td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}
      </Card>

      {editing && (
        <Modal
          title={
            <span className="flex items-center gap-2">
              构建档案 <Mono className="normal-case">{editing.project_id}</Mono>
            </span>
          }
          onClose={() => setEditing(null)}
          wide
        >
          <textarea
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            readOnly={role !== "admin"}
            spellCheck={false}
            rows={18}
            className="w-full rounded border border-[var(--color-line)] bg-[var(--color-surface)] p-3 font-[var(--font-mono)] text-[11.5px] leading-[17px] text-[var(--color-ink)] outline-none focus:border-[var(--color-accent)]"
          />
          {error && (
            <div className="mt-2">
              <ErrorBox message={error} />
            </div>
          )}
          <p className="mt-2 text-[11px] text-[var(--color-ink-faint)]">
            指向未审批镜像的改动会被拒绝（§8.3）。
          </p>
          {role === "admin" && (
            <div className="mt-4 flex justify-end gap-2">
              <Button onClick={() => setEditing(null)}>取消</Button>
              <Button
                variant="primary"
                onClick={() => save.mutate(editing)}
                disabled={save.isPending}
              >
                保存
              </Button>
            </div>
          )}
        </Modal>
      )}
    </div>
  );
}
