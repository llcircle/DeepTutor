import assert from "node:assert/strict";
import test from "node:test";

import { resolveDynamicExport } from "@/desktop/shims/dynamic-export";

function LearningNavigation(): null {
  return null;
}

function ResourcesNavigation(): null {
  return null;
}

test("桌面动态加载器接受 then 直接返回的命名组件", async () => {
  const loadedModule = { LearningNavigation };
  const loaded = await Promise.resolve().then(
    () => loadedModule.LearningNavigation,
  );

  assert.equal(resolveDynamicExport(loaded), LearningNavigation);
});

test("桌面动态加载器优先使用模块默认导出", () => {
  const loadedModule = {
    default: LearningNavigation,
    ResourcesNavigation,
  };

  assert.equal(resolveDynamicExport(loadedModule), LearningNavigation);
});

test("桌面动态加载器回退到命名组件", () => {
  const loadedModule = { ResourcesNavigation };

  assert.equal(resolveDynamicExport(loadedModule), ResourcesNavigation);
});

test("桌面动态加载器接受 memo 和 forwardRef 形态的对象组件", () => {
  const memoComponent = {
    $$typeof: Symbol.for("react.memo"),
    type: LearningNavigation,
  };

  assert.equal(resolveDynamicExport(memoComponent), memoComponent);
});

test("桌面动态加载器拒绝没有组件的模块", () => {
  assert.throws(
    () => resolveDynamicExport({ default: { kind: "data" } }),
    /动态模块没有可渲染的 React 组件/,
  );
});
