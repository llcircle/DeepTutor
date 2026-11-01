import type {
  ReadingConversation,
  ReadingWorkspace,
} from "@/lib/reading-workspace-api";

/** 仅当会话属于当前路由和资料集时，返回它保存的材料。 */
export function readingConversationMaterialId(
  workspace: ReadingWorkspace,
  conversation: ReadingConversation | null,
  routeSessionId: string | null,
): string | null {
  if (!routeSessionId || conversation?.session_id !== routeSessionId) return null;
  return materialInWorkspace(workspace, conversation.active_material_id);
}

/** 按会话和手动选择派生当前文档，避免轮询覆盖阅读视图。 */
export function readingMaterialSelectionId(
  workspace: ReadingWorkspace,
  conversation: ReadingConversation | null,
  routeSessionId: string | null,
  manualSelection: {
    workspaceId: string;
    sessionId: string;
    materialId: string;
  } | null,
): string | null {
  if (routeSessionId) {
    if (conversation?.session_id !== routeSessionId) return null;
    if (
      manualSelection?.workspaceId === workspace.workspace_id &&
      manualSelection.sessionId === routeSessionId
    ) {
      return (
        materialInWorkspace(workspace, manualSelection.materialId) ??
        readingConversationMaterialId(workspace, conversation, routeSessionId)
      );
    }
    return readingConversationMaterialId(workspace, conversation, routeSessionId);
  }

  return (
    materialInWorkspace(workspace, workspace.active_material_id) ??
    workspace.tabs[0]?.material.material_id ??
    null
  );
}

function materialInWorkspace(
  workspace: ReadingWorkspace,
  materialId: string | null | undefined,
): string | null {
  const id = materialId?.trim();
  return id && workspace.tabs.some((tab) => tab.material.material_id === id)
    ? id
    : null;
}
