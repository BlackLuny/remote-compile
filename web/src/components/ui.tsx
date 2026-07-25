// Small hand-rolled primitives in the shadcn/ui spirit: composable, styled
// with Tailwind, no runtime theming layer to fight.

import clsx from "clsx";
import type { ButtonHTMLAttributes, InputHTMLAttributes, ReactNode } from "react";

export type Tone = "ok" | "bad" | "warn" | "info" | "muted" | "accent";

const toneText: Record<Tone, string> = {
  ok: "text-[var(--color-ok)]",
  bad: "text-[var(--color-bad)]",
  warn: "text-[var(--color-warn)]",
  info: "text-[var(--color-info)]",
  accent: "text-[var(--color-accent)]",
  muted: "text-[var(--color-ink-dim)]",
};

const toneChip: Record<Tone, string> = {
  ok: "bg-[color-mix(in_srgb,var(--color-ok)_16%,transparent)] text-[var(--color-ok)] ring-[color-mix(in_srgb,var(--color-ok)_35%,transparent)]",
  bad: "bg-[color-mix(in_srgb,var(--color-bad)_16%,transparent)] text-[var(--color-bad)] ring-[color-mix(in_srgb,var(--color-bad)_35%,transparent)]",
  warn: "bg-[color-mix(in_srgb,var(--color-warn)_16%,transparent)] text-[var(--color-warn)] ring-[color-mix(in_srgb,var(--color-warn)_35%,transparent)]",
  info: "bg-[color-mix(in_srgb,var(--color-info)_16%,transparent)] text-[var(--color-info)] ring-[color-mix(in_srgb,var(--color-info)_35%,transparent)]",
  accent:
    "bg-[color-mix(in_srgb,var(--color-accent)_16%,transparent)] text-[var(--color-accent)] ring-[color-mix(in_srgb,var(--color-accent)_35%,transparent)]",
  muted: "bg-[var(--color-panel-2)] text-[var(--color-ink-dim)] ring-[var(--color-line)]",
};

export function Badge({
  children,
  tone = "muted",
  className,
}: {
  children: ReactNode;
  tone?: Tone;
  className?: string;
}) {
  return (
    <span
      className={clsx(
        "inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] font-medium ring-1 ring-inset whitespace-nowrap",
        toneChip[tone],
        className,
      )}
    >
      {children}
    </span>
  );
}

export function Card({
  title,
  action,
  children,
  className,
  bodyClassName,
}: {
  title?: ReactNode;
  action?: ReactNode;
  children: ReactNode;
  className?: string;
  bodyClassName?: string;
}) {
  return (
    <section
      className={clsx(
        "rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)]",
        className,
      )}
    >
      {(title || action) && (
        <header className="flex items-center justify-between gap-3 border-b border-[var(--color-line-soft)] px-4 py-2.5">
          <h2 className="text-[12px] font-semibold tracking-wide text-[var(--color-ink-dim)] uppercase">
            {title}
          </h2>
          {action}
        </header>
      )}
      <div className={clsx("p-4", bodyClassName)}>{children}</div>
    </section>
  );
}

/** A single headline number. The unit and delta stay subordinate to the value. */
export function Stat({
  label,
  value,
  hint,
  tone = "muted",
}: {
  label: string;
  value: ReactNode;
  hint?: ReactNode;
  tone?: Tone;
}) {
  return (
    <div className="rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-4 py-3">
      <div className="text-[11px] font-medium tracking-wide text-[var(--color-ink-dim)] uppercase">
        {label}
      </div>
      <div className={clsx("tnum mt-1 text-2xl leading-none font-semibold", toneText[tone])}>
        {value}
      </div>
      {hint !== undefined && (
        <div className="mt-1.5 text-[11px] text-[var(--color-ink-faint)]">{hint}</div>
      )}
    </div>
  );
}

type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: "primary" | "ghost" | "danger" | "subtle";
  size?: "sm" | "md";
};

export function Button({
  variant = "subtle",
  size = "sm",
  className,
  ...props
}: ButtonProps) {
  const variants = {
    primary:
      "bg-[var(--color-accent)] text-[#0b0f14] hover:brightness-110 font-medium",
    danger:
      "bg-[color-mix(in_srgb,var(--color-bad)_15%,transparent)] text-[var(--color-bad)] ring-1 ring-inset ring-[color-mix(in_srgb,var(--color-bad)_35%,transparent)] hover:bg-[color-mix(in_srgb,var(--color-bad)_25%,transparent)]",
    ghost: "text-[var(--color-ink-dim)] hover:text-[var(--color-ink)] hover:bg-[var(--color-panel-2)]",
    subtle:
      "bg-[var(--color-panel-2)] text-[var(--color-ink)] ring-1 ring-inset ring-[var(--color-line)] hover:bg-[var(--color-line-soft)]",
  };
  return (
    <button
      className={clsx(
        "inline-flex items-center justify-center gap-1.5 rounded transition-colors disabled:cursor-not-allowed disabled:opacity-45",
        size === "sm" ? "h-7 px-2.5 text-[12px]" : "h-9 px-4 text-[13px]",
        variants[variant],
        className,
      )}
      {...props}
    />
  );
}

export function Input({ className, ...props }: InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      className={clsx(
        "h-7 rounded border border-[var(--color-line)] bg-[var(--color-surface)] px-2 text-[12px] text-[var(--color-ink)] outline-none placeholder:text-[var(--color-ink-faint)] focus:border-[var(--color-accent)]",
        className,
      )}
      {...props}
    />
  );
}

export function Select({
  value,
  onChange,
  options,
  className,
}: {
  value: string;
  onChange: (v: string) => void;
  options: { value: string; label: string }[];
  className?: string;
}) {
  return (
    <select
      value={value}
      onChange={(e) => onChange(e.target.value)}
      className={clsx(
        "h-7 rounded border border-[var(--color-line)] bg-[var(--color-surface)] px-2 text-[12px] text-[var(--color-ink)] outline-none focus:border-[var(--color-accent)]",
        className,
      )}
    >
      {options.map((o) => (
        <option key={o.value} value={o.value}>
          {o.label}
        </option>
      ))}
    </select>
  );
}

export function Table({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div className="overflow-x-auto">
      <table className={clsx("w-full border-collapse text-[12px]", className)}>{children}</table>
    </div>
  );
}

export function Th({ children, className }: { children?: ReactNode; className?: string }) {
  return (
    <th
      className={clsx(
        "border-b border-[var(--color-line)] px-3 py-2 text-left text-[11px] font-medium tracking-wide text-[var(--color-ink-dim)] uppercase",
        className,
      )}
    >
      {children}
    </th>
  );
}

export function Td({ children, className }: { children?: ReactNode; className?: string }) {
  return (
    <td className={clsx("border-b border-[var(--color-line-soft)] px-3 py-1.5 align-middle", className)}>
      {children}
    </td>
  );
}

export function Mono({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <span className={clsx("font-[var(--font-mono)] text-[11.5px]", className)}>{children}</span>
  );
}

export function Empty({ children }: { children: ReactNode }) {
  return (
    <div className="px-3 py-10 text-center text-[12px] text-[var(--color-ink-faint)]">
      {children}
    </div>
  );
}

export function Spinner({ label }: { label?: string }) {
  return (
    <div className="flex items-center gap-2 px-3 py-8 text-[12px] text-[var(--color-ink-dim)]">
      <span className="live-dot inline-block h-1.5 w-1.5 rounded-full bg-[var(--color-accent)]" />
      {label ?? "加载中…"}
    </div>
  );
}

export function ErrorBox({ message }: { message: string }) {
  return (
    <div className="rounded border border-[color-mix(in_srgb,var(--color-bad)_35%,transparent)] bg-[color-mix(in_srgb,var(--color-bad)_10%,transparent)] px-3 py-2 text-[12px] text-[var(--color-bad)]">
      {message}
    </div>
  );
}

/** Horizontal meter, used for load and disk. */
export function Meter({ value, tone = "info" }: { value: number; tone?: Tone }) {
  const pct = Math.max(0, Math.min(1, value)) * 100;
  const bar: Record<Tone, string> = {
    ok: "bg-[var(--color-ok)]",
    bad: "bg-[var(--color-bad)]",
    warn: "bg-[var(--color-warn)]",
    info: "bg-[var(--color-info)]",
    accent: "bg-[var(--color-accent)]",
    muted: "bg-[var(--color-ink-faint)]",
  };
  return (
    <div className="h-1.5 w-20 overflow-hidden rounded-full bg-[var(--color-panel-2)]">
      <div className={clsx("h-full rounded-full", bar[tone])} style={{ width: `${pct}%` }} />
    </div>
  );
}

export function Modal({
  title,
  onClose,
  children,
  wide,
}: {
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
  wide?: boolean;
}) {
  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/60 p-8"
      onClick={onClose}
    >
      <div
        className={clsx(
          "fade-in w-full rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] shadow-2xl",
          wide ? "max-w-4xl" : "max-w-xl",
        )}
        onClick={(e) => e.stopPropagation()}
      >
        <header className="flex items-center justify-between border-b border-[var(--color-line-soft)] px-4 py-3">
          <h3 className="text-[13px] font-semibold">{title}</h3>
          <Button variant="ghost" onClick={onClose} aria-label="关闭">
            ✕
          </Button>
        </header>
        <div className="p-4">{children}</div>
      </div>
    </div>
  );
}
