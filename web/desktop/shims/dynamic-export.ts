import type { ComponentType } from "react";

export type DynamicComponent = ComponentType<Record<string, unknown>>;

const DYNAMIC_EXPORT_ERROR = "动态模块没有可渲染的 React 组件";

/**
 * 归一化 next/dynamic 的两种合法返回值：
 * 1. Promise<ComponentType>，常见于 named export 的 .then(module => module.X)。
 * 2. Promise<Module>，使用 default 或其他可渲染导出。
 */
export function resolveDynamicExport(value: unknown): DynamicComponent {
  if (isDynamicComponent(value)) {
    return value;
  }
  if (!value || typeof value !== "object") {
    throw new Error(DYNAMIC_EXPORT_ERROR);
  }

  const moduleValue = value as Record<string, unknown>;
  const component = [moduleValue.default, ...Object.values(moduleValue)].find(
    isDynamicComponent,
  );
  if (!component) {
    throw new Error(DYNAMIC_EXPORT_ERROR);
  }
  return component;
}

function isDynamicComponent(value: unknown): value is DynamicComponent {
  if (typeof value === "function") {
    return true;
  }
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as { $$typeof?: unknown; render?: unknown };
  return (
    typeof candidate.render === "function" ||
    typeof candidate.$$typeof === "symbol"
  );
}
