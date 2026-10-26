import { Link } from "react-router-dom";
import { Mono } from "./ui";
import { shortId } from "../lib/format";

/**
 * A worker wherever the console names a machine.
 *
 * The operator's name is what a task list has to show — `worker-01ky…` next to
 * `worker-01kz…` is not something anyone can scan. The id is still the identity,
 * so it stays the link target and lives in the tooltip (and, where there is
 * room, next to the name) rather than disappearing behind the alias.
 */
export function WorkerRef({
  id,
  name,
  showId = false,
}: {
  id: string;
  name?: string;
  showId?: boolean;
}) {
  if (!id) return <span className="text-[var(--color-ink-dim)]">–</span>;
  const label = name?.trim() ?? "";
  return (
    <Link
      to={`/workers/${id}`}
      className="hover:text-[var(--color-accent)]"
      title={label ? `${label} · ${id}` : id}
    >
      {label ? <span className="font-medium">{label}</span> : <Mono className="text-[var(--color-ink-dim)]">{shortId(id, 14)}</Mono>}
      {label && showId && (
        <Mono className="ml-1.5 text-[var(--color-ink-faint)]">{shortId(id, 12)}</Mono>
      )}
    </Link>
  );
}
