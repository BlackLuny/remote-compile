import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api";
import { Button, ErrorBox, Input } from "../components/ui";

export function Login() {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  // "No account exists yet" and "wrong password" are very different problems;
  // a fresh deployment should not look like a credentials failure.
  const bootstrap = useQuery({
    queryKey: ["bootstrap"],
    queryFn: () => api.get<{ needs_setup: boolean; version: string }>("/api/bootstrap"),
    retry: false,
  });

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError("");
    try {
      await api.post("/api/login", { username, password });
      await qc.invalidateQueries({ queryKey: ["me"] });
      navigate("/", { replace: true });
    } catch (err) {
      setError(err instanceof Error ? err.message : "登录失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex h-full items-center justify-center bg-[var(--color-surface)]">
      <form
        onSubmit={submit}
        className="fade-in w-80 rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] p-6"
      >
        <div className="mb-5 flex items-center gap-2">
          <div className="h-7 w-7 rounded bg-[var(--color-accent)]/20 text-center text-[14px] leading-7 font-bold text-[var(--color-accent)]">
            rc
          </div>
          <div>
            <div className="text-[14px] leading-none font-semibold">remote-compile</div>
            <div className="mt-1 text-[10px] text-[var(--color-ink-faint)]">
              控制台 {bootstrap.data?.version ? `v${bootstrap.data.version}` : ""}
            </div>
          </div>
        </div>

        {bootstrap.data?.needs_setup && (
          <div className="mb-4 rounded border border-[color-mix(in_srgb,var(--color-warn)_35%,transparent)] bg-[color-mix(in_srgb,var(--color-warn)_10%,transparent)] px-3 py-2 text-[11.5px] leading-relaxed text-[var(--color-warn)]">
            还没有任何管理员账号。先在服务器上执行：
            <div className="mt-1 font-[var(--font-mono)] text-[11px]">
              rc-server admin --username admin --password &lt;密码&gt;
            </div>
          </div>
        )}

        <label className="mb-1 block text-[11px] text-[var(--color-ink-dim)]">用户名</label>
        <Input
          className="mb-3 h-8 w-full"
          value={username}
          autoFocus
          autoComplete="username"
          onChange={(e) => setUsername(e.target.value)}
        />

        <label className="mb-1 block text-[11px] text-[var(--color-ink-dim)]">密码</label>
        <Input
          className="mb-4 h-8 w-full"
          type="password"
          value={password}
          autoComplete="current-password"
          onChange={(e) => setPassword(e.target.value)}
        />

        {error && (
          <div className="mb-3">
            <ErrorBox message={error} />
          </div>
        )}

        <Button type="submit" variant="primary" size="md" className="w-full" disabled={busy}>
          {busy ? "登录中…" : "登录"}
        </Button>
      </form>
    </div>
  );
}
