import { describe, expect, it } from "vitest";
import { requireSettingsResponse } from "../lib/settings-response";

describe("设置接口响应边界", () => {
  it("拒绝导致页面崩溃的旧版空对象和裸设置", () => {
    for (const kind of ["starters", "attachments", "network", "capabilities", "readiness", "usage"] as const) {
      expect(() => requireSettingsResponse({}, kind)).toThrow("设置接口返回的数据不完整");
    }
    expect(() => requireSettingsResponse({ trace_count: 20 }, "starters")).toThrow();
    expect(() => requireSettingsResponse({ settings: null }, "network")).toThrow();
  });

  it("接受完整默认值，拒绝数值字符串与非有限数值", () => {
    const payload = { settings: { trace_count: 20 }, bounds: { trace_count: [3, 100] } };
    expect(() => requireSettingsResponse(payload, "starters")).not.toThrow();
    expect(() => requireSettingsResponse({ ...payload, settings: { trace_count: "20" } }, "starters")).toThrow();
    expect(() => requireSettingsResponse({ ...payload, settings: { trace_count: Number.NaN } }, "starters")).toThrow();
  });

  it("就绪接口必须包含全部状态计数", () => {
    const payload = {
      schema_version: "deeptutor.settings-readiness/v2", ok: false, rows: [], notices: [],
      summary: { enabled_verified: 3, available_disabled: 0, unavailable: 0, misconfigured: 0, not_selected: 8 },
    };
    expect(() => requireSettingsResponse(payload, "readiness")).not.toThrow();
    expect(() => requireSettingsResponse({ ...payload, summary: {} }, "readiness")).toThrow();
    expect(() => requireSettingsResponse({ ...payload, rows: [null] }, "readiness")).toThrow();
    expect(() => requireSettingsResponse({ ...payload, schema_version: "unknown" }, "readiness")).toThrow();
  });
});
