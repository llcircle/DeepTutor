type Kind = "starters" | "attachments" | "network" | "capabilities" | "readiness" | "usage";

const fields: Record<Kind, Record<string, string>> = {
  starters: { "settings.trace_count": "number", "bounds.trace_count": "array" },
  attachments: {
    "settings.max_file_mb": "number", "settings.max_total_mb": "number",
    "settings.max_chars_per_doc": "number", "settings.max_chars_total": "number",
    "effective.max_file_bytes": "number", "effective.max_total_bytes": "number",
    "effective.max_chars_per_doc": "number", "effective.max_chars_total": "number", "effective.ws_max_size": "number",
    "bounds.max_file_mb": "array", "bounds.max_total_mb": "array", "bounds.chars": "array",
  },
  network: {
    "settings.backend_port": "number", "settings.frontend_port": "number",
    "settings.public_api_base": "string", "settings.cors_origins": "array",
    "effective.backend_url": "string", "effective.frontend_url": "string",
    "effective.browser_api_base": "string", "effective.cors_mode": "string",
    "auth.enabled": "boolean", "auth.cookie_samesite": "string", "auth.cookie_secure": "boolean",
    "auth.cross_site_cookie_ready": "boolean", restart_required: "boolean",
  },
  capabilities: {
    "chat.temperature": "number", "chat.max_rounds": "number",
    "chat.stage_budgets.exploring": "number", "chat.stage_budgets.responding": "number",
    "solve.temperature": "number", "solve.max_tokens": "number", "solve.max_rounds": "number",
    "solve.max_replans": "number", "research.max_tokens": "number", "research.temperature": "number",
    "research.researching.note_agent_mode": "string", "research.researching.tool_timeout": "number",
    "research.researching.tool_max_retries": "number", "research.researching.paper_search_years_limit": "number",
    "question.max_tokens": "number", "question.temperature": "number", "question.exploring.max_iterations": "number",
    "question.exploring.tool_summarizer.enabled": "boolean", "question.exploring.tool_summarizer.max_tokens": "number",
    "co_writer.temperature": "number", "co_writer.max_tokens": "number",
    "vision_solver.temperature": "number", "vision_solver.max_tokens": "number",
    "math_animator.temperature": "number", "math_animator.max_tokens": "number",
    "visualize.temperature": "number", "visualize.max_tokens": "number",
  },
  readiness: {
    schema_version: "string", ok: "boolean", rows: "array", notices: "array",
    "summary.enabled_verified": "number", "summary.available_disabled": "number",
    "summary.unavailable": "number", "summary.misconfigured": "number", "summary.not_selected": "number",
  },
  usage: {
    year: "number", timezone: "string", days: "array", models: "array",
    "totals.total_tokens": "number", "totals.prompt_tokens": "number",
    "totals.completion_tokens": "number", "totals.total_calls": "number",
    active_days: "number", sessions: "number", turns: "number", tracked_turns: "number", updated_at: "number",
  },
};

// 在边界拒绝不完整响应，让错误进入页面的可重试提示，而非渲染时崩溃。
export function requireSettingsResponse(value: unknown, kind: Kind): void {
  for (const [path, type] of Object.entries(fields[kind])) {
    const result = path.split(".").reduce<unknown>((node, key) =>
      node !== null && typeof node === "object" ? (node as Record<string, unknown>)[key] : undefined, value);
    const valid = type === "array" ? Array.isArray(result)
      : typeof result === type && (type !== "number" || Number.isFinite(result));
    if (!valid) throw new Error(`设置接口返回的数据不完整：${kind}.${path}，请更新程序后重试。`);
  }
  const record = value as Record<string, unknown>;
  const validRow = (row: unknown, required: Record<string, string>) =>
    row !== null && typeof row === "object" && Object.entries(required).every(([key, type]) => {
      const value = (row as Record<string, unknown>)[key];
      return typeof value === type && (type !== "number" || Number.isFinite(value));
    });
  if (kind === "usage") {
    const daily = { date: "string", total_tokens: "number", total_calls: "number", turns: "number", tracked_turns: "number" };
    const model = { provider: "string", model: "string", total_tokens: "number", prompt_tokens: "number", completion_tokens: "number", total_calls: "number" };
    if (!(record.days as unknown[]).every(row => validRow(row, daily))
      || !(record.models as unknown[]).every(row => validRow(row, model))) {
      throw new Error("用量统计接口返回了无效的每日记录或模型明细。");
    }
  }
  if (kind === "readiness") {
    const states = ["enabled_verified", "available_disabled", "unavailable", "misconfigured", "not_selected"];
    const rowFields = {
      id: "string", section: "string", label: "string", state: "string", detail_code: "string",
      enabled: "boolean", available: "boolean", configured: "boolean", verified: "boolean", required: "boolean",
    };
    if (record.schema_version !== "deeptutor.settings-readiness/v2"
      || !(record.rows as unknown[]).every(row => validRow(row, rowFields)
        && states.includes((row as Record<string, string>).state))) {
      throw new Error("运行状态接口的版本或状态明细无效。");
    }
  }
}
