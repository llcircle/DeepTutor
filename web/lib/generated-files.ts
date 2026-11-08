import type { MessageAttachment } from "@/features/chat/ChatStateAdapter";
import type { StreamEvent } from "@/features/chat/model/protocol";

interface StreamedFile {
  workspace_id?: string;
  workspace_item_id?: string;
  relative_path?: string;
  filename?: string;
  url?: string;
  mime_type?: string;
  size_bytes?: number;
  sha256?: string;
  title?: string;
  caption?: string;
  generated?: boolean;
}

/** 汇集实时工具产物，供聊天卡片与会话侧栏使用同一份数据。 */
export function extractStreamedArtifacts(
  events?: StreamEvent[],
): MessageAttachment[] {
  const out: MessageAttachment[] = [];
  for (const ev of events ?? []) {
    if (ev.type !== "tool_result" && ev.type !== "workspace_items") continue;
    const meta = (ev.metadata ?? {}) as {
      items?: StreamedFile[];
      tool_metadata?: {
        workspace_items?: StreamedFile[];
        artifacts?: StreamedFile[];
      };
    };
    const items = ev.type === "workspace_items"
      ? meta.items ?? []
      : meta.tool_metadata?.workspace_items?.length
        ? meta.tool_metadata.workspace_items
        : meta.tool_metadata?.artifacts ?? [];
    for (const item of items) {
      if (!item?.url) continue;
      out.push({
        type: item.mime_type?.startsWith("image/") ? "image" : "document",
        filename: item.filename,
        url: item.url,
        mime_type: item.mime_type,
        size_bytes: item.size_bytes,
        generated: item.generated ?? true,
        ...(item.workspace_item_id ? {
          origin: "workspace" as const,
          workspace_id: item.workspace_id,
          workspace_item_id: item.workspace_item_id,
          relative_path: item.relative_path,
          sha256: item.sha256,
        } : {}),
        title: item.title,
        caption: item.caption,
      });
    }
  }
  return out;
}

export function mergeGeneratedFiles(
  attachments: MessageAttachment[],
  events?: StreamEvent[],
): MessageAttachment[] {
  const persisted = attachments.filter(
    (a) => a.generated || a.origin === "workspace",
  );
  const seen = new Set(persisted.map((a) => a.url).filter(Boolean));
  const merged = [...persisted];
  for (const a of extractStreamedArtifacts(events)) {
    if (a.url && seen.has(a.url)) continue;
    if (a.url) seen.add(a.url);
    merged.push(a);
  }
  return merged;
}
