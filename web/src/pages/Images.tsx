import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type ImageRow, type Role } from "../api";
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
import { agoSecs, percent, shortDigest, statusTone } from "../lib/format";

export function Images({ role }: { role: Role }) {
  const qc = useQueryClient();
  const [review, setReview] = useState<ImageRow | null>(null);

  const q = useQuery({
    queryKey: ["images"],
    queryFn: () => api.get<{ images: ImageRow[] }>("/api/images"),
    refetchInterval: 15_000,
  });

  const act = useMutation({
    mutationFn: ({ id, action }: { id: string; action: string }) =>
      api.post(`/api/images/${id}/${action}`),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["images"] });
      setReview(null);
    },
  });

  if (q.isLoading) return <Spinner />;
  if (q.isError) return <ErrorBox message={(q.error as Error).message} />;

  const all = q.data!.images;
  const pending = all.filter((r) => r.image.status === "pending_approval");
  const rest = all.filter((r) => r.image.status !== "pending_approval");

  return (
    <div className="space-y-4">
      {/* §8.3: the approval queue is the gate between "an agent wrote a
          Dockerfile" and "that Dockerfile runs on our fleet". It goes first. */}
      <Card
        title={
          <span className="flex items-center gap-2">
            审批队列
            {pending.length > 0 && <Badge tone="warn">{pending.length}</Badge>}
          </span>
        }
        bodyClassName="p-0"
      >
        {pending.length === 0 ? (
          <Empty>没有待审批的镜像。</Empty>
        ) : (
          <Table>
            <thead>
              <tr>
                <Th>镜像</Th>
                <Th>来源</Th>
                <Th>提交者</Th>
                <Th>说明</Th>
                <Th>提交时间</Th>
                <Th />
              </tr>
            </thead>
            <tbody>
              {pending.map((r) => (
                <tr key={r.image.id} className="hover:bg-[var(--color-panel-2)]/50">
                  <Td>
                    <Mono>{r.image.image_ref || r.image.pull_ref}</Mono>
                  </Td>
                  <Td className="text-[var(--color-ink-dim)]">
                    {r.image.dockerfile ? "Dockerfile" : "上游镜像"}
                  </Td>
                  <Td>
                    <Mono className="text-[var(--color-ink-dim)]">{r.image.created_by || "–"}</Mono>
                  </Td>
                  <Td className="text-[var(--color-ink-dim)]">{r.image.description || "–"}</Td>
                  <Td className="text-[var(--color-ink-faint)]">{agoSecs(r.image.created_at)}</Td>
                  <Td>
                    <div className="flex justify-end gap-1.5">
                      <Button onClick={() => setReview(r)}>查看</Button>
                      {role === "admin" && (
                        <>
                          <Button
                            variant="primary"
                            onClick={() => act.mutate({ id: r.image.id, action: "approve" })}
                          >
                            批准
                          </Button>
                          <Button
                            variant="danger"
                            onClick={() => act.mutate({ id: r.image.id, action: "reject" })}
                          >
                            拒绝
                          </Button>
                        </>
                      )}
                    </div>
                  </Td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}
      </Card>

      <Card title="环境镜像" bodyClassName="p-0">
        {rest.length === 0 ? (
          <Empty>还没有环境镜像。agent 用 prepare_env 提交后会出现在这里。</Empty>
        ) : (
          <Table>
            <thead>
              <tr>
                <Th>镜像</Th>
                <Th>状态</Th>
                <Th>成功率</Th>
                <Th>最近成功</Th>
                <Th>审批人</Th>
                <Th>说明</Th>
                <Th />
              </tr>
            </thead>
            <tbody>
              {rest.map((r) => (
                <tr key={r.image.id} className="hover:bg-[var(--color-panel-2)]/50">
                  <Td>
                    <Mono>{shortDigest(r.full_ref)}</Mono>
                  </Td>
                  <Td>
                    <Badge tone={statusTone[r.image.status] ?? "muted"}>{r.image.status}</Badge>
                    {r.image.consecutive_env_errors > 0 && (
                      <Badge tone="warn" className="ml-1">
                        连续 {r.image.consecutive_env_errors} 次 env_error
                      </Badge>
                    )}
                  </Td>
                  <Td className="tnum">
                    {r.health.total_runs === 0 ? "–" : percent(r.health.success_rate_7d)}
                    <span className="ml-1 text-[11px] text-[var(--color-ink-faint)]">
                      / {r.health.total_runs} 次
                    </span>
                  </Td>
                  <Td className="text-[var(--color-ink-dim)]">{agoSecs(r.health.last_success_at)}</Td>
                  <Td className="text-[var(--color-ink-dim)]">{r.image.approved_by || "–"}</Td>
                  <Td className="text-[var(--color-ink-dim)]">{r.image.description || "–"}</Td>
                  <Td>
                    <div className="flex justify-end gap-1.5">
                      <Button onClick={() => setReview(r)}>查看</Button>
                      {role === "admin" && r.image.dockerfile && (
                        <Button onClick={() => act.mutate({ id: r.image.id, action: "rebuild" })}>
                          重建
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

      {review && (
        <Modal title={review.image.image_ref || review.image.pull_ref} onClose={() => setReview(null)} wide>
          <div className="space-y-3 text-[12px]">
            <div className="grid grid-cols-2 gap-x-6 gap-y-1.5">
              <Kv label="env_id" value={review.image.id} mono />
              <Kv label="状态" value={review.image.status} />
              <Kv label="digest" value={review.image.digest || "（尚未构建）"} mono />
              <Kv label="提交者" value={review.image.created_by || "–"} mono />
              <Kv
                label="审批"
                value={
                  review.image.approved_by
                    ? `${review.image.approved_by} · ${agoSecs(review.image.approved_at)}`
                    : "未审批"
                }
              />
              <Kv label="构建于" value={review.image.built_at ? agoSecs(review.image.built_at) : "–"} />
            </div>

            {review.image.message && (
              <div className="rounded border border-[var(--color-line-soft)] bg-[var(--color-surface)] px-3 py-2 text-[var(--color-ink-dim)]">
                {review.image.message}
              </div>
            )}

            {review.image.dockerfile ? (
              <div>
                <div className="mb-1 text-[11px] tracking-wide text-[var(--color-ink-dim)] uppercase">
                  Dockerfile — 构建期就能执行任意命令，运行时沙箱兜不住，请逐行看
                </div>
                <pre className="max-h-96 overflow-auto rounded border border-[var(--color-line)] bg-[var(--color-surface)] p-3 font-[var(--font-mono)] text-[11.5px] leading-[17px] whitespace-pre">
                  {review.image.dockerfile}
                </pre>
              </div>
            ) : (
              <div className="text-[var(--color-ink-dim)]">
                直接引用上游镜像：<Mono>{review.image.pull_ref}</Mono>
              </div>
            )}

            {role === "admin" && review.image.status === "pending_approval" && (
              <div className="flex justify-end gap-2 border-t border-[var(--color-line-soft)] pt-3">
                <Button
                  variant="danger"
                  onClick={() => act.mutate({ id: review.image.id, action: "reject" })}
                >
                  拒绝
                </Button>
                <Button
                  variant="primary"
                  onClick={() => act.mutate({ id: review.image.id, action: "approve" })}
                >
                  批准并构建
                </Button>
              </div>
            )}
          </div>
        </Modal>
      )}
    </div>
  );
}

function Kv({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div>
      <div className="text-[11px] tracking-wide text-[var(--color-ink-dim)] uppercase">{label}</div>
      <div className="mt-0.5 break-all">{mono ? <Mono>{value}</Mono> : value}</div>
    </div>
  );
}
