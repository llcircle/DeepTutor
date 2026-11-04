import { apiFetch, apiUrl } from "@/lib/api";

const BASE = "/api/reading";

async function request<T>(path: string, body?: unknown, method = "GET"): Promise<T> {
  const response = await apiFetch(apiUrl(`${BASE}${path}`), {
    method, cache: "no-store",
    ...(body === undefined ? {} : { headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }),
  });
  const result = await response.json();
  if (!response.ok) throw new Error(result.detail || result.message || `请求失败：${response.status}`);
  return result as T;
}

export interface RetrievalSettings {
  enabled: boolean;
  consented: boolean;
  model_available: boolean;
  model?: string | null;
  model_error?: string | null;
}

export interface ReadingIndexStatus {
  status: "not_indexed" | "queued" | "indexing" | "ready" | "failed" | "stale" | "cancelled" | "interrupted";
  completed_pages?: number;
  total_pages?: number;
  model?: string;
  error?: string;
}

export interface OcrCorrection {
  id: string;
  material_id: string;
  locator: number;
  base_revision: number;
  original_ocr: string;
  previous_text: string;
  suggested_text: string;
  final_text: string | null;
  confidence: number;
  summary: string;
  model: string;
  profile_id: string | null;
  prompt_version: string;
  created_at: number;
  applied_revision: number | null;
  status: "pending" | "accepted" | "rejected";
}

export interface ReferenceResolution {
  material_id: string;
  revision: number;
  current_revision: number;
  locator: number;
  resolution: "resolved" | "unresolved" | "ambiguous" | "stale";
  old_text: string;
  current_text: string;
  offset: number | null;
}

export const getRetrievalSettings = () => request<RetrievalSettings>("/retrieval/settings");
export const saveRetrievalSettings = (enabled: boolean, consented: boolean, workspaceId?: string | null) =>
  request<RetrievalSettings>("/retrieval/settings", { enabled, consented, workspace_id: workspaceId }, "PUT");
export const getReadingIndex = (workspace: string) =>
  request<ReadingIndexStatus>(`/workspaces/${encodeURIComponent(workspace)}/index`);
export const rebuildReadingIndex = (workspace: string) =>
  request(`/workspaces/${encodeURIComponent(workspace)}/index`, {}, "POST");
export const deleteReadingIndex = (workspace: string) =>
  request(`/workspaces/${encodeURIComponent(workspace)}/index`, undefined, "DELETE");
export const listOcrCorrections = (material: string) =>
  request<{ items: OcrCorrection[]; pending_count: number }>(`/materials/${encodeURIComponent(material)}/ocr-corrections`);
export const acceptOcrCorrection = (material: string, id: string, expectedRevision: number, text: string) =>
  request<OcrCorrection>(`/materials/${encodeURIComponent(material)}/ocr-corrections/${encodeURIComponent(id)}/accept`,
    { expected_revision: expectedRevision, text }, "POST");
export const rejectOcrCorrection = (material: string, id: string) =>
  request<OcrCorrection>(`/materials/${encodeURIComponent(material)}/ocr-corrections/${encodeURIComponent(id)}/reject`, {}, "POST");
export const getReadingPageImage = (material: string, locator: number) =>
  request<{ url: string; sha256: string }>(`/materials/${encodeURIComponent(material)}/pages/${locator}/image`);
export const resolveReadingReference = (material: string, revision: number, locator: number, quote?: string) =>
  request<ReferenceResolution>(`/materials/${encodeURIComponent(material)}/references/resolve`, { revision, locator, quote }, "POST");
