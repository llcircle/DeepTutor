import { render, screen, waitFor } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
import ChatMarkdownNoteTab from "@/components/chat/home/ChatMarkdownNoteTab";
import { createNote } from "@/lib/notes-api";
import { loadChatMarkdownNoteDraft } from "@/lib/chat-markdown-note";

vi.mock("next/dynamic", () => ({
  default: () => function Editor({ docId }: { docId: string }) {
    return <div data-testid="document-editor">{docId}</div>;
  },
}));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (text: string) => text }),
}));
vi.mock("@/hooks/useAuthStatus", () => ({
  useAuthStatus: () => ({ loading: false, statusAvailable: true, enabled: false }),
}));
vi.mock("@/lib/notes-api", () => ({ createNote: vi.fn() }));
vi.mock("@/lib/chat-markdown-note", () => ({
  loadChatMarkdownNoteDraft: vi.fn(),
  saveChatMarkdownNoteDraft: vi.fn(),
}));

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(loadChatMarkdownNoteDraft).mockReturnValue({
    title: "手工草稿", content: "未保存的编辑",
    saved: { id: "manual-document", title: "旧标题", content: "旧正文" },
  });
});

it("直接打开模型已保存的文档，不被会话的手工草稿替换，也不重复创建", async () => {
  render(<ChatMarkdownNoteTab sessionId="reading-session" documentId="generated-document" />);
  await waitFor(() => expect(screen.getByTestId("document-editor")).toHaveTextContent("generated-document"));
  expect(loadChatMarkdownNoteDraft).not.toHaveBeenCalled();
  expect(createNote).not.toHaveBeenCalled();
});

it("切换生成文档时始终打开对应文档，不沿用上一个正文", async () => {
  const view = render(<ChatMarkdownNoteTab sessionId="reading-session" documentId="first-document" />);
  view.rerender(<ChatMarkdownNoteTab sessionId="reading-session" documentId="second-document" />);
  await waitFor(() => expect(screen.getByTestId("document-editor")).toHaveTextContent("second-document"));
  expect(createNote).not.toHaveBeenCalled();
});

it("手工新建笔记入口仍保留未保存草稿恢复提示", async () => {
  render(<ChatMarkdownNoteTab sessionId="reading-session" />);
  await screen.findByText("An earlier conversation draft has unsaved changes.");
  expect(createNote).not.toHaveBeenCalled();
});
