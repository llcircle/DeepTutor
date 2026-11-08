import type { MessageAttachment } from "@/features/chat/ChatStateAdapter";

export function generatedDocumentId(attachment: MessageAttachment): string | null {
  if (!attachment.generated || attachment.type !== "document") return null;
  return attachment.url?.match(
    /^\/api\/documents\/([a-f0-9]{32}|[a-f0-9-]{36})\/markdown$/,
  )?.[1] ?? null;
}
