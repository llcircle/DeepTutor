"use client";

import { useCallback, useEffect, useState } from "react";
import { apiUrl } from "@/lib/api";
import {
  acceptOcrCorrection, getReadingPageImage, listOcrCorrections, rejectOcrCorrection, type OcrCorrection,
} from "@/lib/reading-retrieval-api";
import { READER_TURN_END_EVENT } from "@/lib/reading-reader-action";

export function ReadingOcrReview({ materialId, revision, locator, onApplied, onJump }: {
  materialId: string; revision: number; locator: number;
  onApplied: () => Promise<void>; onJump: (locator: number) => void;
}) {
  const [rows, setRows] = useState<OcrCorrection[]>([]);
  const [open, setOpen] = useState(false);
  const [chosen, setChosen] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [image, setImage] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const refresh = useCallback(async () => {
    const response = await listOcrCorrections(materialId);
    setRows(response.items);
  }, [materialId]);
  useEffect(() => {
    let alive = true;
    const update = () => {
      void listOcrCorrections(materialId).then(result => { if (alive) setRows(result.items); })
        .catch(caught => { if (alive) setError(caught instanceof Error ? caught.message : "无法读取校正建议。"); });
    };
    update();
    const timer = window.setInterval(update, 8000);
    window.addEventListener(READER_TURN_END_EVENT, update);
    return () => { alive = false; window.clearInterval(timer); window.removeEventListener(READER_TURN_END_EVENT, update); };
  }, [materialId]);
  const pending = rows.filter(row => row.status === "pending");
  const current = pending.find(row => row.id === chosen && row.locator === locator)
    ?? pending.find(row => row.locator === locator);
  const currentId = current?.id;
  const currentLocator = current?.locator;
  useEffect(() => { setText(current?.suggested_text ?? ""); setError(""); }, [current?.id, current?.suggested_text]);
  useEffect(() => {
    let alive = true;
    setImage("");
    if (open && currentId && currentLocator) void getReadingPageImage(materialId, currentLocator)
      .then(result => { if (alive) setImage(apiUrl(result.url)); })
      .catch(caught => { if (alive) setError(caught instanceof Error ? caught.message : "无法读取原始页图。"); });
    return () => { alive = false; };
  }, [materialId, currentId, currentLocator, open]);
  const process = async (accept: boolean) => {
    if (!current) return;
    setBusy(true); setError("");
    try {
      if (accept) {
        await acceptOcrCorrection(materialId, current.id, revision, text);
        await onApplied();
      } else await rejectOcrCorrection(materialId, current.id);
      await refresh();
    } catch (caught) { setError(caught instanceof Error ? caught.message : "无法处理校正建议。"); }
    finally { setBusy(false); }
  };
  if (!rows.length && !error) return null;
  return (
    <aside aria-label="OCR 校正审核" className="shrink-0 border-b border-[var(--border)] text-xs">
      <button type="button" className="flex w-full items-center justify-between px-3 py-2"
        aria-expanded={open} onClick={() => setOpen(value => !value)}>
        <span>OCR 校正建议</span><span aria-label={`${pending.length} 条待审核`}>{pending.length} 条待审核 · {open ? "收起" : "展开"}</span>
      </button>
      {open && <div className="max-h-[48vh] space-y-3 overflow-auto px-3 pb-3">
        {pending.length > 0 && <nav aria-label="待审核页面" className="flex flex-wrap gap-2">
          {pending.map(row => <button key={row.id} type="button" disabled={busy}
            className="rounded border border-[var(--border)] px-2 py-1"
            onClick={() => { setChosen(row.id); onJump(row.locator); }}>第 {row.locator} 页</button>)}
        </nav>}
        {current ? <div className="space-y-2">
          <p>第 {current.locator} 页 · 模型 {current.model} · 自报置信度 {Math.round(current.confidence * 100)}%
            · {new Date(current.created_at * 1000).toLocaleString()} · 提示词 {current.prompt_version}</p>
          <p>{current.summary}</p>
          <p className="text-[var(--muted-foreground)]">置信度由模型给出，请以原始页图为准。确认前不会改变检索文本。</p>
          {image && <a href={image} target="_blank" rel="noreferrer" aria-label="查看原始页面图片">
            {/* 原始页图用于人工核验，不替换 PDF 原件。 */}
            {/* eslint-disable-next-line @next/next/no-img-element */}
            <img src={image} alt={`第 ${current.locator} 页原始图片`} className="max-h-56 max-w-full object-contain" />
          </a>}
          <details><summary>原始 OCR 文本</summary><pre className="whitespace-pre-wrap">{current.original_ocr}</pre></details>
          <details><summary>当前检索文本</summary><pre className="whitespace-pre-wrap">{current.previous_text}</pre></details>
          <label className="block">确认后的完整页文本（可编辑）
            <textarea aria-label="校正后的完整页文本" value={text} disabled={busy} onChange={event => setText(event.target.value)}
              rows={7} className="mt-1 w-full rounded border border-[var(--border)] bg-[var(--background)] p-2 text-sm" />
          </label>
          <div className="flex gap-4">
            <button type="button" disabled={busy || !text.trim()} onClick={() => { void process(true); }}>确认并更新本页索引</button>
            <button type="button" disabled={busy} onClick={() => { void process(false); }}>拒绝建议</button>
          </div>
        </div> : <p>{pending.length ? "请选择待审核页面，或跳转到该页后查看。" : "没有待审核建议。"}</p>}
        {rows.some(row => row.status !== "pending") && <details><summary>审核历史</summary>
          {rows.filter(row => row.status !== "pending").map(row => <details key={row.id} className="mt-2">
            <summary>第 {row.locator} 页 · {row.status === "accepted" ? `已应用至版本 ${row.applied_revision}` : "已拒绝"}</summary>
            <pre className="whitespace-pre-wrap">{row.final_text ?? row.suggested_text}</pre>
          </details>)}
        </details>}
      </div>}
      {error && <p role="alert" className="px-3 pb-2 text-[var(--destructive)]">{error}</p>}
    </aside>
  );
}
