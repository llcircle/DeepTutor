import assert from "node:assert/strict";
import test from "node:test";

import {
  readingConversationMaterialId,
  readingMaterialSelectionId,
} from "../lib/reading-session-material";
import type {
  ReadingConversation,
  ReadingWorkspace,
} from "../lib/reading-workspace-api";

const workspace: ReadingWorkspace = {
  workspace_id: "collection",
  title: "数字信号处理",
  description: "",
  color: "",
  active_material_id: "material-a",
  created_at: 1,
  updated_at: 1,
  tabs: [
    {
      material: {
        material_id: "material-a",
        content_id: "material-a",
        filename: "a.pdf",
        title: "A",
        source_kind: "file",
        source_url: "",
        mime: "application/pdf",
        render_mode: "pdf",
        cover_url: "",
        duration_seconds: 0,
        status: "ready",
        progress: 100,
        error_code: "",
        error_detail: "",
        created_at: 1,
        updated_at: 1,
        last_opened_at: 1,
      },
      tab_order: 0,
      pinned: false,
      opened: true,
      added_at: 1,
    },
    {
      material: {
        material_id: "material-b",
        content_id: "material-b",
        filename: "b.pdf",
        title: "B",
        source_kind: "file",
        source_url: "",
        mime: "application/pdf",
        render_mode: "pdf",
        cover_url: "",
        duration_seconds: 0,
        status: "ready",
        progress: 100,
        error_code: "",
        error_detail: "",
        created_at: 1,
        updated_at: 1,
        last_opened_at: 1,
      },
      tab_order: 1,
      pinned: false,
      opened: true,
      added_at: 1,
    },
  ],
};

function conversation(
  sessionId: string,
  materialId: string | null,
): ReadingConversation {
  return {
    workspace_id: "collection",
    session_id: sessionId,
    title: "历史阅读",
    active_material_id: materialId,
    created_at: 1,
    updated_at: 1,
  };
}

test("历史阅读会话恢复其绑定的材料而不是资料集当前材料", () => {
  assert.equal(
    readingConversationMaterialId(
      workspace,
      conversation("session-b", "material-b"),
      "session-b",
    ),
    "material-b",
  );
});

test("不从其他会话、无绑定会话或不属于资料集的材料猜测", () => {
  assert.equal(
    readingConversationMaterialId(
      workspace,
      conversation("another-session", "material-b"),
      "session-b",
    ),
    null,
  );
  assert.equal(
    readingConversationMaterialId(
      workspace,
      conversation("session-b", null),
      "session-b",
    ),
    null,
  );
  assert.equal(
    readingConversationMaterialId(
      workspace,
      conversation("session-b", "outside"),
      "session-b",
    ),
    null,
  );
  assert.equal(
    readingConversationMaterialId(
      workspace,
      conversation("session-b", "material-b"),
      null,
    ),
    null,
  );
});

test("历史会话材料优先于资料集全局材料，且旧轮询状态不能覆盖它", () => {
  const saved = conversation("session-b", "material-b");
  assert.equal(
    readingMaterialSelectionId(workspace, saved, "session-b", null),
    "material-b",
  );
  assert.equal(
    readingMaterialSelectionId(
      { ...workspace, active_material_id: "material-b" },
      saved,
      "session-b",
      null,
    ),
    "material-b",
  );
});

test("当前会话手动选择覆盖会话绑定和轮询返回的全局材料", () => {
  const saved = conversation("session-b", "material-b");
  const manual = {
    workspaceId: "collection",
    sessionId: "session-b",
    materialId: "material-a",
  };
  assert.equal(
    readingMaterialSelectionId(workspace, saved, "session-b", manual),
    "material-a",
  );
  assert.equal(
    readingMaterialSelectionId(
      { ...workspace, active_material_id: "material-b" },
      saved,
      "session-b",
      manual,
    ),
    "material-a",
  );
});

test("会话切换时不串用上一会话的手动选择", () => {
  assert.equal(
    readingMaterialSelectionId(
      workspace,
      conversation("session-c", "material-b"),
      "session-c",
      {
        workspaceId: "collection",
        sessionId: "session-b",
        materialId: "material-a",
      },
    ),
    "material-b",
  );
});

test("不同资料集中的同名会话不继承上一资料集的手动选择", () => {
  assert.equal(
    readingMaterialSelectionId(
      { ...workspace, workspace_id: "another-collection" },
      conversation("session-b", "material-b"),
      "session-b",
      {
        workspaceId: "collection",
        sessionId: "session-b",
        materialId: "material-a",
      },
    ),
    "material-b",
  );
});

test("历史会话缺少有效绑定时不退回资料集全局材料", () => {
  assert.equal(
    readingMaterialSelectionId(
      workspace,
      conversation("session-b", null),
      "session-b",
      null,
    ),
    null,
  );
  assert.equal(
    readingMaterialSelectionId(workspace, null, "session-b", null),
    null,
  );
});
