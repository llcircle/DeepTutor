import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ReadingOcrReview } from "@/components/reading/ReadingOcrReview";
import { ReadingRetrievalSettings } from "@/components/reading/ReadingRetrievalSettings";
import type { OcrCorrection } from "@/lib/reading-retrieval-api";

const api = vi.hoisted(() => ({
  list: vi.fn(), accept: vi.fn(), reject: vi.fn(), image: vi.fn(),
  settings: vi.fn(), save: vi.fn(), index: vi.fn(), rebuild: vi.fn(), remove: vi.fn(),
}));
vi.mock("@/lib/reading-retrieval-api", () => ({
  listOcrCorrections: api.list, acceptOcrCorrection: api.accept, rejectOcrCorrection: api.reject,
  getReadingPageImage: api.image, getRetrievalSettings: api.settings, saveRetrievalSettings: api.save,
  getReadingIndex: api.index, rebuildReadingIndex: api.rebuild, deleteReadingIndex: api.remove,
}));
vi.mock("@/lib/api", () => ({ apiUrl: (url: string) => url }));

const correction: OcrCorrection = {
  id: "correction-1", material_id: "material-1", locator: 1, base_revision: 1,
  original_ocr: "原始识别错字", previous_text: "原始识别错字", suggested_text: "模型建议文字",
  final_text: null, confidence: 0.9, summary: "修正错字", model: "vision-fixture",
  profile_id: "profile-1", prompt_version: "reading-ocr-v1", created_at: 1,
  applied_revision: null, status: "pending",
};

beforeEach(() => {
  vi.clearAllMocks();
  api.list.mockResolvedValue({ items: [correction], pending_count: 1 });
  api.image.mockResolvedValue({ url: "/api/original-page.jpg", sha256: "fixture" });
  api.settings.mockResolvedValue({ enabled: false, consented: false, model_available: true });
  api.index.mockResolvedValue({ status: "not_indexed" });
  api.save.mockResolvedValue({ enabled: true, consented: true, model_available: true });
  api.accept.mockResolvedValue({ ...correction, status: "accepted", applied_revision: 2 });
});

describe("阅读检索与 OCR 人工审核", () => {
  it("首次开启必须明确确认资料发送提示", async () => {
    render(<ReadingRetrievalSettings workspaceId="workspace-1" />);
    const enable = await screen.findByRole("button", { name: "开启向量检索" });
    await waitFor(() => expect(enable).not.toBeDisabled());
    fireEvent.click(enable);
    expect(api.save).not.toHaveBeenCalled();
    expect(screen.getByRole("dialog", { name: "确认向量检索的数据发送" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "我已了解，确认开启" }));
    await waitFor(() => expect(api.save).toHaveBeenCalledWith(true, true, "workspace-1"));
  });

  it("已配置但未设默认的唯一模型显示真实名称并允许更新索引", async () => {
    api.settings.mockResolvedValue({
      enabled: true, consented: true, model_available: true, model: "bge-m3", model_error: null,
    });
    render(<ReadingRetrievalSettings workspaceId="workspace-1" />);
    expect(await screen.findByText("当前检索模型：bge-m3")).toBeInTheDocument();
    expect(screen.queryByText(/尚未配置可用向量模型/)).not.toBeInTheDocument();
    const update = screen.getByRole("button", { name: "更新索引" });
    expect(update).not.toBeDisabled();
    fireEvent.click(update);
    await waitFor(() => expect(api.rebuild).toHaveBeenCalledWith("workspace-1"));
  });

  it("多个模型或提供方错误显示真实原因而不是统一提示未配置", async () => {
    api.settings.mockResolvedValue({
      enabled: true, consented: true, model_available: false, model: null,
      model_error: "已配置多个向量模型，请在设置 → 向量模型将其中一个设为默认。",
    });
    render(<ReadingRetrievalSettings workspaceId="workspace-1" />);
    expect(await screen.findByText(/已配置多个向量模型/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "更新索引" })).toBeDisabled();
    expect(screen.queryByText(/尚未配置可用向量模型/)).not.toBeInTheDocument();
  });

  it("仅在用户点击确认后保存可编辑文本和当前版本", async () => {
    const applied = vi.fn().mockResolvedValue(undefined);
    render(<ReadingOcrReview materialId="material-1" revision={1} locator={1} onApplied={applied} onJump={vi.fn()} />);
    fireEvent.click(await screen.findByRole("button", { name: /OCR 校正建议/ }));
    expect(await screen.findByText("原始 OCR 文本")).toBeInTheDocument();
    expect(api.accept).not.toHaveBeenCalled();
    fireEvent.change(screen.getByRole("textbox", { name: "校正后的完整页文本" }), { target: { value: "用户核对后的文字" } });
    fireEvent.click(screen.getByRole("button", { name: "确认并更新本页索引" }));
    await waitFor(() => expect(api.accept).toHaveBeenCalledWith("material-1", "correction-1", 1, "用户核对后的文字"));
    expect(applied).toHaveBeenCalledOnce();
  });

  it("保留并显示版本冲突，拒绝建议不会调用接受接口", async () => {
    api.accept.mockRejectedValueOnce(new Error("材料版本已改变，请刷新后重新确认。"));
    render(<ReadingOcrReview materialId="material-1" revision={1} locator={1} onApplied={vi.fn()} onJump={vi.fn()} />);
    fireEvent.click(await screen.findByRole("button", { name: /OCR 校正建议/ }));
    fireEvent.click(await screen.findByRole("button", { name: "确认并更新本页索引" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("材料版本已改变");
    api.accept.mockClear();
    fireEvent.click(screen.getByRole("button", { name: "拒绝建议" }));
    await waitFor(() => expect(api.reject).toHaveBeenCalledWith("material-1", "correction-1"));
    expect(api.accept).not.toHaveBeenCalled();
  });

  it("其他页的建议必须先定位到对应页面", async () => {
    const jump = vi.fn();
    render(<ReadingOcrReview materialId="material-1" revision={1} locator={100} onApplied={vi.fn()} onJump={jump} />);
    fireEvent.click(await screen.findByRole("button", { name: /OCR 校正建议/ }));
    expect(screen.queryByRole("button", { name: "确认并更新本页索引" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "第 1 页" }));
    expect(jump).toHaveBeenCalledWith(1);
  });
});
