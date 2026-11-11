import { buildResumeTurn } from "@/contracts/parse/turn-command";

interface IdleTurnRecoveryInput {
  isStreaming: boolean;
  hasPendingUserInput: boolean;
  activeTurnId: string | null;
  lastSeq: number;
  updatedAt: number;
  now: number;
  idleTimeoutMs: number;
}

export type IdleTurnRecoveryDecision =
  | { kind: "none" }
  | { kind: "resubscribe"; message: ReturnType<typeof buildResumeTurn> }
  | { kind: "reconcile" };

/**
 * Decide what the client-side idle watchdog should do.
 *
 * A quiet WebSocket is not proof that a server turn failed. Long research
 * tool calls can legitimately emit nothing for several minutes, and the
 * backend keeps the turn alive when a browser briefly disconnects. When a
 * server turn id is known, re-subscribe from the last received sequence so
 * buffered events (including a missed terminal event) are replayed.
 */
export function decideIdleTurnRecovery(
  input: IdleTurnRecoveryInput,
): IdleTurnRecoveryDecision {
  if (!input.isStreaming || input.hasPendingUserInput) return { kind: "none" };
  if (input.now - input.updatedAt <= input.idleTimeoutMs) {
    return { kind: "none" };
  }
  if (!input.activeTurnId) return { kind: "reconcile" };
  return {
    kind: "resubscribe",
    message: buildResumeTurn({
      turnId: input.activeTurnId,
      afterSeq: input.lastSeq,
    }),
  };
}

/** 优先采用后端活动回合；会话时间只在旧服务未提供活动列表时兜底。 */
export function resolveLoadedRunStatus<T extends string>(
  status: T,
  lastActivityAt: number,
  now: number,
  idleTimeoutMs: number,
  activeTurnStatus?: string | null,
): T | "idle" | "running" {
  if (activeTurnStatus === "queued" ||
      activeTurnStatus === "running" ||
      activeTurnStatus === "waiting_input") return "running";
  if (status !== "running") return status;
  // 明确返回空活动列表时，不能只凭残留的会话标记让输入框永远等待。
  if (activeTurnStatus === null) return "idle";
  // No usable timestamp: trust the server rather than guessing.
  if (!Number.isFinite(lastActivityAt) || lastActivityAt <= 0) return status;
  if (now - lastActivityAt <= idleTimeoutMs) return status;
  return "idle";
}
