/** 与 Rust settings_contracts 保持一致，不静默截取或更改用户的草稿值。 */
export function capabilitySettingsErrors(value: unknown, path = "capabilities"): string[] {
  if (!value || typeof value !== "object" || Array.isArray(value)) return [`${path}: 必须是设置对象`];
  return Object.entries(value).flatMap(([key, item]) => {
    const field = `${path}.${key}`;
    if (item && typeof item === "object") return capabilitySettingsErrors(item, field);
    if (typeof item !== "number" && !["temperature", "max_rounds", "max_iterations", "max_tokens", "exploring", "responding", "max_replans", "tool_timeout", "tool_max_retries", "paper_search_years_limit"].includes(key)) return [];
    const [min, max] = key === "temperature" ? [0, 2]
      : ["max_rounds", "max_iterations", "paper_search_years_limit"].includes(key) ? [1, 50]
      : ["max_replans", "tool_max_retries"].includes(key) ? [0, 10]
      : key === "tool_timeout" ? [1, 600] : [1, 200000];
    return typeof item === "number" && Number.isFinite(item) && item >= min && item <= max && (key === "temperature" || Number.isInteger(item))
      ? [] : [`${field}: 必须为 ${min}–${max} 之间的${key === "temperature" ? "数值" : "整数"}，当前值为 ${String(item)}`];
  });
}
