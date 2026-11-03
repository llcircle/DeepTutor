"use client";

import { useCallback, useEffect, useState } from "react";
import {
  deleteReadingIndex, getReadingIndex, getRetrievalSettings, rebuildReadingIndex, saveRetrievalSettings,
  type ReadingIndexStatus, type RetrievalSettings,
} from "@/lib/reading-retrieval-api";

const STATUS: Record<ReadingIndexStatus["status"], string> = {
  not_indexed: "尚未建立", queued: "等待处理", indexing: "正在建立", ready: "已就绪",
  failed: "建立失败", stale: "模型已改变，需更新", cancelled: "已取消", interrupted: "上次任务中断",
};

export function ReadingRetrievalSettings({ workspaceId }: { workspaceId?: string | null }) {
  const [settings, setSettings] = useState<RetrievalSettings | null>(null);
  const [index, setIndex] = useState<ReadingIndexStatus | null>(null);
  const [consent, setConsent] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const refresh = useCallback(async () => {
    const value = await getRetrievalSettings();
    setSettings(value);
    setIndex(workspaceId ? await getReadingIndex(workspaceId) : null);
  }, [workspaceId]);
  useEffect(() => {
    let alive = true;
    const update = async () => {
      try {
        const value = await getRetrievalSettings();
        const status = workspaceId ? await getReadingIndex(workspaceId) : null;
        if (alive) { setSettings(value); setIndex(status); }
      } catch (caught) { if (alive) setError(caught instanceof Error ? caught.message : "无法读取检索设置。"); }
    };
    void update();
    const timer = window.setInterval(() => { void update(); }, 6000);
    return () => { alive = false; window.clearInterval(timer); };
  }, [workspaceId]);

  const run = async (action: () => Promise<unknown>) => {
    setBusy(true); setError("");
    try { await action(); await refresh(); } catch (caught) {
      setError(caught instanceof Error ? caught.message : "操作失败，请重试。");
    } finally { setBusy(false); }
  };
  return (
    <section aria-label="阅读混合检索" className="rounded-lg border border-[var(--border)] p-3 text-sm">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <strong>阅读混合检索</strong>
        <button type="button" disabled={!settings || busy}
          onClick={() => {
            if (settings?.enabled) void run(() => saveRetrievalSettings(false, settings.consented, workspaceId));
            else if (settings?.consented) void run(() => saveRetrievalSettings(true, true, workspaceId));
            else setConsent(true);
          }} className="rounded border border-[var(--border)] px-3 py-1 disabled:opacity-50">
          {settings?.enabled ? "关闭向量检索" : "开启向量检索"}
        </button>
      </div>
      <p className="mt-2 text-xs text-[var(--muted-foreground)]">
        页级定位、页与段落向量和关键词共同检索当前资料集。关闭或未配置向量模型时，继续使用本地关键词检索。
      </p>
      {consent && (
        <div role="dialog" aria-label="确认向量检索的数据发送" className="mt-3 rounded border border-[var(--border)] p-3">
          <p>开启后，资料文本和检索问题会发送到您设置的向量服务，可能产生费用。当前资料集立即建立索引，其他资料集首次打开时建立。</p>
          <div className="mt-2 flex gap-3">
            <button type="button" disabled={busy} onClick={() => {
              void run(async () => { await saveRetrievalSettings(true, true, workspaceId); setConsent(false); });
            }}>我已了解，确认开启</button>
            <button type="button" disabled={busy} onClick={() => setConsent(false)}>取消</button>
          </div>
        </div>
      )}
      {settings?.enabled && !settings.model_available && (
        <p className="mt-2 text-xs">尚未配置可用向量模型，当前使用关键词检索。请在设置的向量模型页面配置服务。</p>
      )}
      {index && <p role="status" className="mt-2 text-xs">
        当前资料集索引：{STATUS[index.status] || index.status}
        {typeof index.total_pages === "number" && ` · ${index.completed_pages ?? 0}/${index.total_pages} 页`}
        {index.model && ` · ${index.model}`}
      </p>}
      {index?.error && <p className="mt-1 text-xs text-[var(--destructive)]">{index.error}</p>}
      {workspaceId && (
        <div className="mt-2 flex gap-4 text-xs">
          <button type="button" disabled={busy || !settings?.enabled || !settings.model_available}
            onClick={() => { void run(() => rebuildReadingIndex(workspaceId)); }}>更新索引</button>
          <button type="button" disabled={busy} onClick={() => {
            void run(async () => {
              // 先关闭自动建立，确保删除操作不会被下一次阅读轮询立即撤销。
              await saveRetrievalSettings(false, settings?.consented ?? false, workspaceId);
              await deleteReadingIndex(workspaceId);
            });
          }}>关闭并删除当前索引</button>
        </div>
      )}
      {error && <p role="alert" className="mt-2 text-xs text-[var(--destructive)]">{error}</p>}
    </section>
  );
}
