import { act, render } from "@testing-library/react";
import { useLayoutEffect } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ChatStateAdapterProvider, useChatStateAdapter } from "@/features/chat/ChatStateAdapter";
import type { StreamEvent } from "@/features/chat/model/protocol";
import type { SessionDetail } from "@/lib/session-api";
import { initI18n } from "@/i18n/init";

initI18n("en");
const api = vi.hoisted(() => ({ getSession: vi.fn() }));
const transport = vi.hoisted(() => {
  class Client {
    static instances: Client[] = [];
    connected = false;
    sent: Record<string, unknown>[] = [];
    cursor = 0;
    turnId: string | null = null;
    constructor(private onEvent: (event: StreamEvent) => void, private onClose?: () => void) {
      Client.instances.push(this);
    }
    connect() { this.connected = true; }
    disconnect() { this.connected = false; this.onClose?.(); }
    setResumeState(turnId: string | null, seq: number) {
      if (turnId !== this.turnId) this.cursor = 0;
      this.turnId = turnId;
      this.cursor = Math.max(seq, this.cursor);
    }
    send(command: Record<string, unknown>) { this.sent.push(command); }
    sendAwaitingAck(command: Record<string, unknown>) {
      this.sent.push(command);
      return Promise.resolve(true);
    }
    emit(seq: number, content: string, type: StreamEvent["type"] = "content",
      metadata: Record<string, unknown> = {}) {
      if (seq <= this.cursor) return;
      this.cursor = seq;
      this.turnId = "turn-a";
      this.onEvent({ type, content, metadata, turn_id: "turn-a", session_id: "a",
        seq, source: "chat", stage: "responding", timestamp: Date.now() / 1000 });
    }
  }
  return { Client };
});
vi.mock("@/lib/session-api", async importOriginal => ({
  ...await importOriginal<typeof import("@/lib/session-api")>(),
  getSession: api.getSession,
}));
vi.mock("@/features/chat/transport/UnifiedTurnClient", () => ({
  UnifiedTurnClient: transport.Client,
}));

let chat: ReturnType<typeof useChatStateAdapter>;
function Harness() {
  const value = useChatStateAdapter();
  useLayoutEffect(() => { chat = value; }, [value]);
  return <div>{value.state.messages.map(message => message.content).join("\n")}</div>;
}
function snapshot(id = "a", status: SessionDetail["status"] = "running",
  activeStatus: "queued" | "running" | "waiting_input" | null = "running"): SessionDetail {
  return {
    id, session_id: id, title: id, created_at: 1, updated_at: 1, status,
    messages: [{ id: 10, session_id: id, role: "user", content: `问题${id}`,
      events: [], attachments: [], created_at: 1 }],
    active_turns: activeStatus ? [{
      id: `turn-${id}`, turn_id: `turn-${id}`, session_id: id, capability: "chat",
      status: activeStatus, error: "", created_at: 1, updated_at: 1, last_seq: 3,
    }] : [],
  };
}
const mount = () => render(<ChatStateAdapterProvider><Harness /></ChatStateAdapterProvider>);
const load = async (id = "a", revalidate = false) => {
  await act(async () => { await chat.loadSession(id, { revalidate }); });
};
const emit = (seq: number, content: string, type?: StreamEvent["type"],
  metadata?: Record<string, unknown>) => act(() => {
  transport.Client.instances[0].emit(seq, content, type, metadata);
});

beforeEach(() => {
  transport.Client.instances.length = 0;
  api.getSession.mockReset().mockImplementation(async (id: string) =>
    snapshot(id, id === "a" ? "running" : "completed", id === "a" ? "running" : null));
});

describe("生成中会话的恢复", () => {
  it.each(["queued", "running", "waiting_input"] as const)(
    "活动回合为 %s 时，不因会话时间过旧丢弃订阅", async status => {
      api.getSession.mockResolvedValue(snapshot("a", "running", status));
      mount();
      await load();
      expect(chat.state.isStreaming).toBe(true);
      expect(transport.Client.instances[0].sent).toContainEqual({
        type: "subscribe_turn", turn_id: "turn-a", after_seq: 0,
      });
    });

  it("前台重新打开正在生成的会话，保留正文和原连接的游标", async () => {
    mount();
    await load();
    emit(1, "切换前");
    await load("b");
    expect(chat.state.sessionId).toBe("b");
    await load();
    expect(chat.state.sessionId).toBe("a");
    expect(chat.state.messages.at(-1)?.content).toBe("切换前");
    expect(transport.Client.instances).toHaveLength(1);
    expect(transport.Client.instances[0].cursor).toBe(1);
    expect(transport.Client.instances[0].sent).toHaveLength(1);
    emit(2, "切换后");
    emit(3, "", "done", { status: "completed", user_message_id: 10, assistant_message_id: 11 });
    expect(chat.state.messages.at(-1)?.content).toBe("切换前切换后");
    expect(chat.state.isStreaming).toBe(false);
  });

  it("后台重新校验不清除流消息", async () => {
    mount();
    await load();
    emit(1, "已经收到的正文");
    await load("a", true);
    expect(chat.state.messages.at(-1)?.content).toBe("已经收到的正文");
    expect(chat.state.isStreaming).toBe(true);
  });

  it("快照在完成事件之后才返回，不把已完成的对话改回等待", async () => {
    mount();
    await load();
    emit(1, "完整正文");
    let resolve!: (session: SessionDetail) => void;
    api.getSession.mockImplementationOnce(() => new Promise<SessionDetail>(done => { resolve = done; }));
    let loading!: Promise<unknown>;
    act(() => { loading = chat.loadSession("a"); });
    emit(2, "", "done", { status: "completed", user_message_id: 10, assistant_message_id: 11 });
    await act(async () => { resolve(snapshot()); await loading; });
    expect(chat.state.messages.at(-1)?.content).toBe("完整正文");
    expect(chat.state.isStreaming).toBe(false);
  });

  it("快照与新到达的事件在同一批更新中也不会相互覆盖", async () => {
    api.getSession.mockResolvedValueOnce(snapshot("a", "completed", null));
    mount();
    await load();
    await act(async () => { chat.sendMessage("继续提问"); });
    await act(async () => {
      await chat.loadSession("a");
      transport.Client.instances[0].emit(1, "本轮正文");
    });
    expect(chat.state.messages.at(-1)?.content).toBe("本轮正文");
    expect(chat.state.isStreaming).toBe(true);
  });

  it("明确没有活动回合的残留 running 状态不会锁住输入框", async () => {
    api.getSession.mockResolvedValue(snapshot("a", "running", null));
    mount();
    await load();
    expect(chat.state.isStreaming).toBe(false);
    expect(transport.Client.instances).toHaveLength(0);
  });

  it("已取消的历史加载不会选择会话或创建订阅", async () => {
    mount();
    await load("b");
    const controller = new AbortController();
    controller.abort();
    await act(async () => { await chat.loadSession("a", { signal: controller.signal }); });
    expect(chat.state.sessionId).toBe("b");
    expect(transport.Client.instances).toHaveLength(0);
  });

  it("取消确认后等待终态回写 ID，下一条消息仍接在原会话链上", async () => {
    mount();
    await load();
    emit(1, "取消前的正文");
    await act(async () => { await chat.cancelStreamingTurn(); });
    expect(chat.state.isStreaming).toBe(true);
    expect(transport.Client.instances[0].connected).toBe(true);
    emit(2, "", "done", { status: "cancelled", user_message_id: 10, assistant_message_id: 11 });
    expect(chat.state.isStreaming).toBe(false);
    await act(async () => { await chat.sendMessage("停止后继续"); });
    expect(transport.Client.instances[1].sent.at(-1)).toMatchObject({
      type: "start_turn", parent_message_id: 11,
    });
  });

  it("上一轮的迟到事件和延迟关闭不能结束下一轮或污染正文", async () => {
    mount();
    await load();
    emit(1, "上一轮正文");
    emit(2, "", "done", { status: "completed", user_message_id: 10, assistant_message_id: 11 });
    await act(async () => { await chat.sendMessage("下一轮"); });
    expect(chat.state.isStreaming).toBe(true);
    const old = transport.Client.instances[0];
    act(() => {
      old.emit(3, "迟到的阶段事件", "stage_end");
      old.disconnect();
    });
    expect(chat.state.isStreaming).toBe(true);
    expect(chat.state.messages.at(-1)?.content).toBe("");
    expect(chat.state.messages.at(-1)?.events).toEqual([]);
  });
});
