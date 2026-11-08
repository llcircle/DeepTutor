import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { once } from "node:events";
import { cp, mkdir, readFile, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

// 仅在用户明确授权调用已配置的真实模型后运行；不打印或导出模型凭据。
const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const require = createRequire(path.join(project, "web/package.json"));
const { chromium } = require("playwright");
const { expect } = require("@playwright/test");
const { unzipSync, strFromU8 } = require("fflate");
const snapshot = JSON.parse(await readFile(path.join(project, "artifacts/live-ai-followup-snapshot.json"), "utf8"));
const output = path.join(project, "artifacts/live-ai-documents-verification");
const root = path.join(output, `exe-${Date.now()}`);
const checks = [];
let child, browser, database, serviceUrl;
const sha = async file => createHash("sha256").update(await readFile(file)).digest("hex");
const originalDatabase = path.join(snapshot.source, "data/deeptutor.sqlite3");
const originalHash = await sha(originalDatabase);

async function start() {
  child = spawn(path.join(root, "DeepTutor.exe"), [], { cwd: root, windowsHide: true, stdio: "ignore",
    env: { ...process.env, DEEPTUTOR_PORTABLE_ROOT: root, RUST_LOG: "deeptutor_desktop_lib=info,tauri=warn" } });
  let launchError;
  child.on("error", error => { launchError = error; });
  for (let i = 0; i < 160; i++) {
    if (launchError) throw launchError;
    assert.equal(child.exitCode, null, "隔离 EXE 提前退出");
    const log = await readFile(path.join(root, "logs/desktop.log"), "utf8").catch(() => "");
    serviceUrl = [...log.matchAll(/(?:^|\s)url=(http:\/\/127\.0\.0\.1:\d+)/g)].at(-1)?.[1];
    if (serviceUrl) {
      try { if ((await fetch(`${serviceUrl}/health`, { signal: AbortSignal.timeout(1000) })).ok) return; }
      catch { /* 等待本地服务。 */ }
    }
    await delay(250);
  }
  throw new Error("隔离 EXE 启动超时");
}
async function stop() {
  if (!child || child.exitCode !== null) return;
  const exited = once(child, "exit");
  child.kill();
  await Promise.race([exited, delay(10000)]);
}
async function api(endpoint, body, method = body === undefined ? "GET" : "PUT") {
  const response = await fetch(`${serviceUrl}/api${endpoint}`, { method, headers: { "Content-Type": "application/json" },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(30000) });
  assert(response.ok, `${endpoint}: HTTP ${response.status}`);
  return response.json();
}
async function realTurn(command) {
  const socket = new WebSocket(serviceUrl.replace("http:", "ws:") + "/ws");
  const frames = [];
  let timer;
  try {
    await new Promise((resolve, reject) => {
      timer = setTimeout(() => reject(new Error("真实模型文档验收超过 30 分钟")), 1800000);
      socket.addEventListener("open", () => socket.send(JSON.stringify({ protocol_version: "2.0", ...command })));
      socket.addEventListener("error", () => reject(new Error("WebSocket 连接失败")));
      socket.addEventListener("message", event => {
        try {
          const frame = JSON.parse(event.data);
          frames.push(frame);
          if (frame.type === "tool_result") console.log(JSON.stringify({ tool: frame.metadata.tool_name, state: frame.metadata.call_state }));
          if (frame.type === "protocol_error") throw new Error("真实模型回合协议错误");
          if (frame.type === "done") {
            assert.equal(frame.metadata.status, "completed", JSON.stringify(frame.metadata));
            resolve();
          }
        } catch (error) { reject(error); }
      });
    });
    return frames;
  } finally {
    clearTimeout(timer); socket.close();
    await writeFile(path.join(output, "real-turn-events.json"), JSON.stringify(frames, null, 2), "utf8");
  }
}

await mkdir(output, { recursive: true });
await cp(path.join(project, "artifacts/portable-package/DeepTutor"), root, { recursive: true });
await mkdir(path.join(root, "data"), { recursive: true });
await cp(path.join(snapshot.directory, "deeptutor.sqlite3"), path.join(root, "data/deeptutor.sqlite3"));
database = new DatabaseSync(path.join(root, "data/deeptutor.sqlite3"));
database.exec("PRAGMA busy_timeout=10000");
const originalRequest = database.prepare("SELECT content FROM messages WHERE id=31 AND role='user'").get()?.content;
assert(originalRequest?.includes("严格按照这个提纲"), "快照未包含原始完整复习提纲");
const workspaceId = "rws_9dd1aa9e-97ab-4ec6-908c-93d8d47de3e1";
const workspace = JSON.parse(database.prepare("SELECT payload_json FROM resources WHERE domain='reading_workspace' AND id=?").get(workspaceId).payload_json);
for (const tab of workspace.tabs) {
  const relative = path.join("data/reading/materials", tab.material_id);
  await cp(path.join(snapshot.source, relative), path.join(root, relative), { recursive: true });
}
try {
  await start();
  await api("/settings/tour/complete", {}, "POST");
  browser = await chromium.launch({ channel: "msedge", headless: true });
  const page = await browser.newPage({ viewport: { width: 1600, height: 1000 }, acceptDownloads: true });
  const errors = [];
  page.on("pageerror", error => errors.push(error.message));
  const liveSettings = await api("/settings");
  const liveCaps = await api("/capabilities/settings");
  const draft = (await api("/settings/draft")).draft;
  assert.equal(draft.extensions.capabilities.chat.max_rounds, 330, "应复现真实越界草稿");
  const catalog = structuredClone(liveSettings.catalog);
  const model = catalog.services.llm.profiles[0].models[0];
  const savedModelName = `${model.name || model.model} 保存验收`;
  model.name = savedModelName;
  await api("/settings/draft", { ...draft, catalog, extensions: { ...draft.extensions, "chat-starters": { trace_count: 27 } } });
  await page.goto(`${serviceUrl}/settings/capabilities`);
  const rounds = page.locator('[data-tour="tour-capabilities"] input[type=number][min="1"][max="50"]').first();
  await expect(rounds).toHaveValue("330", { timeout: 60000 });
  await expect(rounds).toHaveAttribute("aria-invalid", "true");
  await expect(page.getByRole("alert").filter({ hasText: "max_rounds" })).toContainText("1–50");
  const apply = () => page.locator('[data-tour="tour-actions"]').getByRole("button", { name: /应用|Apply changes/ });
  await apply().click();
  await expect.poll(async () => (await api("/settings/chat-starters")).settings.trace_count).toBe(27);
  await expect.poll(async () => (await api("/settings")).catalog.services.llm.profiles[0].models[0].name).toBe(savedModelName);
  assert.equal((await api("/capabilities/settings")).chat.max_rounds, liveCaps.chat.max_rounds);
  assert.equal((await api("/settings/draft")).draft.extensions.capabilities.chat.max_rounds, 330);
  await page.screenshot({ path: path.join(output, "invalid-setting-visible.png"), fullPage: true });
  checks.push("真实 330 越界草稿有字段范围提示；界面应用后其他设置及模型名称生效，非法值保留为草稿且不进入运行设置");
  await rounds.fill("34");
  await apply().click();
  await expect.poll(async () => (await api("/capabilities/settings")).chat.max_rounds).toBe(34);
  await expect.poll(async () => (await api("/settings/draft")).draft).toBeNull();
  await page.screenshot({ path: path.join(output, "settings-applied.png"), fullPage: true });
  checks.push("把 330 显式修正为 34 后，通过实际界面应用并清除草稿");
  await browser.close(); browser = null;
  await stop(); await start();
  assert.equal((await api("/capabilities/settings")).chat.max_rounds, 34);
  assert.equal((await api("/settings/chat-starters")).settings.trace_count, 27);
  assert.equal((await api("/settings")).catalog.services.llm.profiles[0].models[0].name, savedModelName);
  checks.push("重启 EXE 后能力、聊天和模型设置全部持久化，配置服务地址和凭据未改动");
  console.log("开始使用用户原模型和原复习提纲生成完整文档，未使用模拟响应。");
  const frames = await realTurn({ type: "start_turn", client_submission_id: randomUUID(), content: originalRequest,
    capability: "immersive_reading", workspace_mode: "immersive_reading", reading_workspace_id: workspaceId,
    reading_material_id: workspace.active_material_id, reading_material_revision: 1, enabled_tools: [] });
  const turnId = frames.find(frame => frame.turn_id)?.turn_id;
  const sessionId = frames.find(frame => frame.session_id)?.session_id;
  // 列表可能不含来源元数据，直接回读 SQLite 核对真实落盘。
  const storedDocuments = database.prepare("SELECT payload_json FROM resources WHERE domain='document' AND deleted_at IS NULL").all()
    .map(row => JSON.parse(row.payload_json)).filter(doc => doc.source_turn_id === turnId);
  assert.equal(storedDocuments.length, 1, "本回合应且仅应交付一份完整文档");
  const document = storedDocuments[0];
  const text = document.content;
  for (const pattern of [/一[、.．\s].*绪论/, /二[、.．\s].*测量系统/, /四[、.．\s].*误差理论/, /六[、.．\s].*数据处理/]) assert.match(text, pattern);
  for (const keyword of ["检定", "校准", "灵敏度", "动态", "贝塞尔", "别捷尔斯", "格罗布斯", "狄克松", "最小二乘"]) assert(text.includes(keyword), `文档缺少提纲主题：${keyword}`);
  assert(text.length > 6000, "文档过短，可能只是摘要或占位内容");
  assert(!text.includes('"_raw"') && !text.includes("测试内容"), "不能把损坏 JSON 或测试占位当成正文");
  assert(frames.some(frame => frame.type === "tool_result" && frame.metadata.tool_name === "read_material"), "应实际阅读原课件");
  await writeFile(path.join(output, "真实模型复习文档.md"), text, "utf8");
  checks.push("真实配置模型完成原始完整提纲：一、二、四、六及全部关键主题，实际读取原课件并且只保存一篇非占位文档");
  browser = await chromium.launch({ channel: "msedge", headless: true });
  const reading = await browser.newPage({ viewport: { width: 1600, height: 1000 }, acceptDownloads: true });
  reading.on("pageerror", error => errors.push(error.message));
  await reading.goto(`${serviceUrl}/learning/reading/${workspaceId}/sessions/${sessionId}`);
  const editor = reading.locator(".dt-note-editor [contenteditable=true]").first();
  await expect(editor).toBeVisible({ timeout: 60000 });
  await expect(editor).toContainText("最小二乘");
  await editor.click(); await reading.keyboard.press("Control+End"); await reading.keyboard.press("Enter");
  await reading.keyboard.type("LIVE_AI_DOCUMENT_EDIT_CHECK");
  await expect.poll(async () => (await api(`/documents/${document.id}`)).content, { timeout: 20000 }).toContain("LIVE_AI_DOCUMENT_EDIT_CHECK");
  await reading.screenshot({ path: path.join(output, "real-document-editable.png"), fullPage: true });
  await reading.locator(".dt-note-topbar button[aria-haspopup=menu]").last().click();
  const downloading = reading.waitForEvent("download");
  await reading.getByRole("menuitem", { name: /导出 Word|Export Word/ }).click();
  const download = await downloading;
  const docxPath = path.join(output, "真实模型文档验收.docx"); await download.saveAs(docxPath);
  const docx = await readFile(docxPath);
  const xml = strFromU8(unzipSync(docx)["word/document.xml"]);
  assert(xml.includes("最小二乘") && xml.includes("LIVE_AI_DOCUMENT_EDIT_CHECK"));
  assert.deepEqual(errors, []);
  checks.push("真实生成文档在沉浸式阅读中打开、修改自动保存，Word 导出保留正文及最新编辑，无前端运行错误");
  await browser.close(); browser = null;
  await stop(); await start();
  assert((await api(`/documents/${document.id}`)).content.includes("LIVE_AI_DOCUMENT_EDIT_CHECK"));
  assert.equal(database.prepare("PRAGMA integrity_check").get().integrity_check, "ok");
  assert.equal(await sha(originalDatabase), originalHash, "验收不能修改原用户数据库");
  checks.push("EXE 重启后生成文档与编辑保持，SQLite 完整性通过，原用户数据库哈希不变");
  const report = { passed: true, root, binarySha256: await sha(path.join(root, "DeepTutor.exe")), checks, turnId, sessionId,
    documentId: document.id, characters: text.length, docxBytes: docx.length, model: model.model,
    usage: frames.findLast(frame => frame.type === "done")?.metadata?.usage_summary ?? null };
  await writeFile(path.join(output, "report.json"), JSON.stringify(report, null, 2) + "\n", "utf8");
  console.log(JSON.stringify(report, null, 2));
} catch (error) {
  const active = browser?.contexts()[0]?.pages()[0];
  if (active) await active.screenshot({ path: path.join(output, "failure.png"), fullPage: true }).catch(() => {});
  await writeFile(path.join(output, "failure.json"), JSON.stringify({ passed: false, root, checks, error: String(error.stack ?? error) }, null, 2));
  throw error;
} finally {
  await browser?.close(); database?.close(); await stop();
}
