import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";

const read = (relative: string) =>
  fs.readFileSync(path.resolve(process.cwd(), relative), "utf8");

test("桌面设置分页不会在渲染期间反复创建 lazy 组件", () => {
  const source = read("desktop/main.tsx");
  const settingsRoute = source.slice(
    source.indexOf("function SettingsSectionRoute()"),
    source.indexOf("const ReadingFolderContent"),
  );

  assert.match(source, /import SettingsPageContent from/);
  assert.doesNotMatch(settingsRoute, /\blazy\(/);
  assert.match(settingsRoute, /<SettingsPageContent section=/);
});

test("桌面特殊路由保持模块级惰性组件并组合页面布局", () => {
  const source = read("desktop/main.tsx");

  assert.match(source, /const ReadingFolderContent = lazy\(/);
  assert.match(source, /const WatchingSessionContent = lazy\(/);
  assert.match(
    source,
    /Component: specialPages\[key\]\s*\? composeSpecialRoute\(key, specialPages\[key\]\)/,
  );

  const composer = source.slice(
    source.indexOf("function composeSpecialRoute("),
    source.indexOf("function SettingsSectionRoute()"),
  );
  assert.match(composer, /layoutKeysFor\(pageKey\)/);
  assert.match(composer, /<Layout>\{content\}<\/Layout>/);
});
