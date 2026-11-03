import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { once } from "node:events";
import { cp, mkdir, readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

// 开发验收使用隔离副本及本地模型，不读取或更改用户资料和真实凭据。
const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const require = createRequire(path.join(project, "web/package.json"));
const { chromium } = require("playwright");
const { expect } = require("@playwright/test");
const baseline = process.argv.includes("--ui-only");
const output = path.join(project, "artifacts/settings-runtime-verification");
const root = path.join(output, `exe-${Date.now()}`);
const checks = [];
const failures = [];
const pages = [];
const requests = [];
let child, browser, database, serviceUrl;
const mock = createServer(async (request, response) => {
  let body = "";
  for await (const chunk of request) body += chunk;
  requests.push(JSON.parse(body));
  response.writeHead(200, { "Content-Type": "text/event-stream" });
  response.end(`data: ${JSON.stringify({
    choices: [{ delta: { content: "设置和用量验收成功" } }],
    usage: { prompt_tokens: 7, completion_tokens: 3, total_tokens: 10, prompt_tokens_details: { cached_tokens: 2 } },
  })}\n\ndata: [DONE]\n\n`);
});
await new Promise(resolve => mock.listen(0, "127.0.0.1", resolve));
await mkdir(output, { recursive: true });
await cp(path.join(project, "artifacts/portable-package/DeepTutor"), root, { recursive: true });
const executable = process.argv.find(arg => arg.startsWith("--exe="))?.slice(6);
if (executable) {
  await cp(path.resolve(executable), path.join(root, "DeepTutor.exe"));
  await cp(path.join(project, "src-tauri/resources/web"), path.join(root, "resources/web"), { recursive: true });
}

async function start() {
  child = spawn(path.join(root, "DeepTutor.exe"), [], {
    cwd: root, windowsHide: true, stdio: "ignore",
    env: { ...process.env, DEEPTUTOR_PORTABLE_ROOT: root, RUST_LOG: "deeptutor_desktop_lib=info,tauri=warn" },
  });
  let launchError;
  child.on("error", error => { launchError = error; });
  for (let attempt = 0; attempt < 120; attempt++) {
    if (launchError) throw launchError;
    assert.equal(child.exitCode, null, "EXE 提前退出");
    const log = await readFile(path.join(root, "logs/desktop.log"), "utf8").catch(() => "");
    const matches = [...log.matchAll(/(?:^|\s)url=(http:\/\/127\.0\.0\.1:\d+)/g)];
    if (matches.length) {
      serviceUrl = matches.at(-1)[1];
      try {
        if ((await fetch(`${serviceUrl}/health`, { signal: AbortSignal.timeout(1000) })).ok) return;
      } catch { /* 等待本地服务就绪。 */ }
    }
    await delay(250);
  }
  throw new Error("最终 EXE 未在 30 秒内启动");
}

async function stop() {
  if (!child || child.exitCode !== null) return;
  const exited = once(child, "exit");
  child.kill();
  await Promise.race([exited, delay(10000)]);
}

async function api(endpoint, body, method = body === undefined ? "GET" : "PUT") {
  const response = await fetch(`${serviceUrl}/api${endpoint}`, {
    method, headers: { "Content-Type": "application/json" },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(30000),
  });
  assert(response.ok, `${endpoint}：HTTP ${response.status} ${await response.clone().text()}`);
  return response.json();
}

async function check(label, action) {
  try { await action(); checks.push(label); }
  catch (error) { failures.push({ label, error: String(error.stack ?? error) }); }
}

async function chat() {
  const socket = new WebSocket(serviceUrl.replace("http:", "ws:") + "/ws");
  let timer;
  try {
    await new Promise((resolve, reject) => {
      timer = setTimeout(() => reject(new Error("验收对话超时")), 30000);
      socket.addEventListener("open", () => socket.send(JSON.stringify({
        type: "start_turn", protocol_version: "2.0", client_submission_id: randomUUID(),
        capability: "chat", content: "设置用量验收", enabled_tools: [],
      })));
      socket.addEventListener("error", () => reject(new Error("WebSocket 连接失败")));
      socket.addEventListener("message", event => {
        const value = JSON.parse(event.data);
        if (value.type === "protocol_error") reject(new Error(JSON.stringify(value)));
        if (value.type === "done") {
          if (value.metadata.status !== "completed") reject(new Error(JSON.stringify(value)));
          else resolve();
        }
      });
    });
  } finally { clearTimeout(timer); socket.close(); }
}

try {
  await start();
  database = new DatabaseSync(path.join(root, "data/deeptutor.sqlite3"));
  database.exec("PRAGMA busy_timeout=10000");
  await api("/settings/ui", { language: "zh", response_language: "zh" });
  await api("/settings/tour/complete", {}, "POST");
  const catalog = JSON.parse(await readFile(path.join(project, "src-tauri/tests/fixtures/referenced-catalog.json"), "utf8"));
  catalog.connections[0].base_url = `http://127.0.0.1:${mock.address().port}/api/v3`;
  catalog.connections[0].api_key = "local-settings-verification";
  await api("/settings/catalog", catalog);

  if (!baseline) {
    await check("默认、旧版局部设置和非法值均有完整契约", async () => {
      const starters = await api("/settings/chat-starters");
      assert.equal(starters.settings.trace_count, 20);
      assert.deepEqual(starters.bounds.trace_count, [3, 100]);
      const saved = await api("/settings/chat-starters", { trace_count: 35 });
      assert.equal(saved.settings.trace_count, 35);
      const rejected = await fetch(`${serviceUrl}/api/settings/chat-starters`, {
        method: "PUT", headers: { "Content-Type": "application/json" }, body: '{"trace_count":null}',
      });
      assert.equal(rejected.status, 400);
      const attachment = await api("/settings/chat-attachments");
      assert.equal(attachment.effective.max_file_bytes, attachment.settings.max_file_mb * 1024 * 1024);
      const caps = await api("/capabilities/settings", { chat: { temperature: 0.37, max_rounds: 2, stage_budgets: { responding: 256 } } });
      assert.equal(caps.chat.max_rounds, 2);
      assert.equal(caps.question.exploring.tool_summarizer.max_tokens, 1024);
      await api("/settings/chat-response-timeout", { chat_response_timeout: 75 });
      await api("/settings/voice-autoplay", { voice_autoplay: true });
      const ui = (await api("/settings")).ui;
      assert.equal(ui.chat_response_timeout, 75);
      assert.equal(ui.voice_autoplay, true);
      const network = await api("/settings/network");
      assert.equal(network.effective.backend_url, serviceUrl);
      assert.equal(network.settings.backend_port, Number(new URL(serviceUrl).port));
      assert.equal(network.desktop_managed, true);
      const readiness = await api("/settings/readiness");
      assert.equal(readiness.schema_version, "deeptutor.settings-readiness/v2");
      assert.equal(Object.values(readiness.summary).reduce((a, b) => a + b, 0), readiness.rows.length);
    });
    await check("真实模型调用用量落库、模型归属、参数应用和缓存统计", async () => {
      await chat();
      assert.equal(requests.at(-1).temperature, 0.37);
      assert.equal(requests.at(-1).max_tokens, 256);
      const value = await api("/settings/usage?year=2026&timezone=Asia%2FShanghai");
      assert.equal(value.totals.total_tokens, 10);
      assert.equal(value.totals.total_calls, 1);
      assert.equal(value.tracked_turns, 1);
      assert.equal(value.turns, 1);
      assert.equal(value.models[0].model, catalog.services.llm.profiles[0].models[0].model);
      assert.equal(value.totals.cache_hit_rate, 2 / 7);
    });
    await check("历史用量、无用量活动、闰年及时区边界", async () => {
      const session = database.prepare("SELECT id FROM sessions LIMIT 1").get().id;
      const insert = database.prepare(`INSERT INTO messages(session_id,role,content,events_json,metadata_json,created_at,parent_message_id) VALUES(?,?,?,?,?,?,?)`);
      const stamp = Date.parse("2024-02-28T18:00:00Z") / 1000;
      const user = insert.run(session, "user", "历史问题", "[]", "{}", stamp, null).lastInsertRowid;
      const summary = { prompt_tokens: 60, completion_tokens: 40, total_tokens: 100, total_calls: 2 };
      insert.run(session, "assistant", "历史回答", JSON.stringify([
        { type: "result", metadata: { metadata: { usage_summary: summary } } },
        { type: "done", metadata: { usage_summary: summary } },
      ]), "{}", stamp + 1, user);
      insert.run(session, "user", "未记录用量的历史活动", "[]", "{}", stamp + 2, null);
      const usage = await api("/settings/usage?year=2024&timezone=Asia%2FShanghai");
      assert.equal(usage.days.length, 366);
      assert.equal(usage.totals.total_tokens, 100);
      assert.equal(usage.totals.total_calls, 2);
      assert.equal(usage.turns, 2);
      assert.equal(usage.tracked_turns, 1);
      assert.equal(usage.days.find(day => day.date === "2024-02-29").turns, 2);
      assert.equal(usage.models[0].model, "");
      const empty = await api("/settings/usage?year=2025&timezone=UTC");
      assert.equal(empty.days.length, 365);
      assert.equal(empty.totals.total_tokens, 0);
      assert.equal(empty.totals.cache_hit_rate, null);
      assert.equal((await fetch(`${serviceUrl}/api/settings/usage?year=2026&timezone=invalid`)).status, 400);
    });
    await check("重新启动后设置及真实使用量保留", async () => {
      await stop();
      await start();
      assert.equal((await api("/settings/chat-starters")).settings.trace_count, 35);
      assert.equal((await api("/capabilities/settings")).chat.temperature, 0.37);
      assert.equal((await api("/settings/usage?year=2026&timezone=UTC")).totals.total_tokens, 10);
    });
  }

  browser = await chromium.launch({ channel: "msedge", headless: true });
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 }, locale: "zh-CN" });
  const page = await context.newPage();
  let currentErrors = [], currentRequests = [];
  page.on("pageerror", error => currentErrors.push(error.message));
  page.on("response", response => {
    if (response.url().includes("/api/") && response.status() >= 400)
      currentRequests.push({ path: new URL(response.url()).pathname, status: response.status() });
  });
  const selectedPages = process.argv.find(arg => arg.startsWith("--pages="))?.slice(8).split(",");
  for (const key of selectedPages ?? [
    "general", "workspace", "data-migration", "appearance", "usage",
    "connections", "llm", "task-models", "embedding", "search", "voice", "multimodal",
    "starters", "attachments", "video-learning", "learner-profile", "progress", "guardian", "memory",
    "tools", "capabilities", "agent-claude-code", "agent-codex", "agent-grok", "agent-antigravity",
    "agent-kimi", "agent-opencode", "agent-mimo", "agent-hermes", "agent-hermes-remote",
    "agent-openclaw", "agent-deepseek-harness", "knowledge", "network", "status", "about", "archive",
  ]) {
    currentErrors = []; currentRequests = [];
    let text = "";
    try {
      await page.goto(`${serviceUrl}/settings/${key}`, { waitUntil: "load", timeout: 30000 });
      await page.waitForTimeout(1200);
      text = await page.locator("body").innerText();
      if (["starters", "attachments", "capabilities", "network", "status", "usage"].includes(key)) {
        await page.screenshot({ path: path.join(output, `${baseline ? "baseline-" : ""}${key}.png`), fullPage: true });
      }
      if (/此页面出现异常|Cannot read properties|unexpected payload|设置接口返回的数据不完整|Unable to load usage|无法加载用量/.test(text)) {
        currentErrors.push("页面显示异常或接口结构错误");
      }
      assert(text.trim().length > 100, "页面内容为空");
      if (key === "usage" && !baseline) {
        assert.equal((await page.locator("dd").first().innerText()).trim(), "10", "用量页面未显示实际 token");
        assert(await page.getByRole("table").count(), "用量页面缺少真实模型明细");
      }
      if (key === "capabilities" && !baseline) {
        assert.equal(await page.locator('[data-tour="tour-capabilities"] input[type=number]').first().inputValue(), "0.37");
      }
      if (key === "network" && !baseline) {
        const port = page.locator('input[type=number][max="65535"]').first();
        assert.equal(await port.inputValue(), new URL(serviceUrl).port);
        assert(await port.isDisabled());
      }
    } catch (error) { currentErrors.push(String(error)); }
    const result = { key, errors: currentErrors, httpErrors: currentRequests,
      ...(currentErrors.length ? { textSnippet: text.slice(0, 1200) } : {}) };
    pages.push(result);
    if (currentErrors.length || currentRequests.length) failures.push({ label: `设置页面 ${key}`, ...result });
    console.log(JSON.stringify(result));
  }
  if (!baseline) {
    await check("真实设置界面应用、保存草稿及跨页面应用均生效", async () => {
      await page.goto(`${serviceUrl}/settings/starters`, { waitUntil: "load" });
      const input = page.locator('input[type=number][min="3"][max="100"]');
      await expect(input).toHaveValue("35");
      await input.fill("36");
      await page.locator('[data-tour="tour-actions"]').getByRole("button", { name: /应用|Apply changes/ }).click();
      await expect.poll(async () => (await api("/settings/chat-starters")).settings.trace_count).toBe(36);
      await page.reload({ waitUntil: "load" });
      await expect(input).toHaveValue("36");
      await input.fill("37");
      await page.locator('[data-tour="tour-actions"]').getByRole("button", { name: /存草稿|保存草稿|Save draft/ }).click();
      await expect.poll(async () => (await api("/settings/draft")).draft?.extensions?.["chat-starters"]?.trace_count).toBe(37);
      await page.locator('a[href="/settings/capabilities"]').first().click();
      await page.locator('[data-tour="tour-actions"]').getByRole("button", { name: /应用|Apply changes/ }).click();
      await expect.poll(async () => (await api("/settings/chat-starters")).settings.trace_count).toBe(37);
    });
  }
  const historyPath = process.argv.find(arg => arg.startsWith("--history-db="))?.slice(13);
  if (!baseline && historyPath) {
    await check("只读历史数据库的匿名用量在隔离实例中完整恢复", async () => {
      const originalHash = createHash("sha256").update(await readFile(historyPath)).digest("hex");
      const source = new DatabaseSync(path.resolve(historyPath), { readOnly: true });
      let records;
      try {
        records = source.prepare(`SELECT created_at, json_extract(metadata_json,'$.usage_summary') AS usage
          FROM messages WHERE role='assistant' AND json_valid(metadata_json)
          AND json_type(metadata_json,'$.usage_summary')='object'`).all();
      } finally { source.close(); }
      const expected = new Map();
      const session = database.prepare("SELECT id FROM sessions LIMIT 1").get().id;
      const insert = database.prepare(`INSERT INTO messages(session_id,role,content,metadata_json,created_at,parent_message_id) VALUES(?,?,?,?,?,?)`);
      for (const record of records) {
        const year = new Date(record.created_at * 1000).getUTCFullYear();
        if (!expected.has(year)) expected.set(year, (await api(`/settings/usage?year=${year}&timezone=UTC`)).totals.total_tokens);
        const raw = JSON.parse(record.usage);
        // 仅复制已记录的数值，不复制原始正文、密钥、模型地址、事件或身份信息。
        const usage = Object.fromEntries(Object.entries(raw).filter(([, value]) => typeof value === "number" && Number.isFinite(value)));
        expected.set(year, expected.get(year) + (usage.total_tokens ?? (usage.prompt_tokens ?? 0) + (usage.completion_tokens ?? 0)));
        const user = insert.run(session, "user", "匿名历史问题", "{}", record.created_at, null).lastInsertRowid;
        insert.run(session, "assistant", "匿名历史回答", JSON.stringify({ usage_summary: usage }), record.created_at, user);
      }
      for (const [year, tokens] of expected)
        assert.equal((await api(`/settings/usage?year=${year}&timezone=UTC`)).totals.total_tokens, tokens);
      assert.equal(createHash("sha256").update(await readFile(historyPath)).digest("hex"), originalHash);
    });
  }
  await browser.close(); browser = undefined;
  assert.equal(database.prepare("PRAGMA integrity_check").get().integrity_check, "ok");
  const report = {
    passed: failures.length === 0, binarySha256: createHash("sha256").update(await readFile(path.join(root, "DeepTutor.exe"))).digest("hex"),
    root, checks, pages, failures,
  };
  await writeFile(path.join(output, baseline ? "baseline-report.json" : "settings-runtime-report.json"), JSON.stringify(report, null, 2) + "\n", "utf8");
  console.log(JSON.stringify({ passed: report.passed, checks, failures }, null, 2));
  if (failures.length) process.exitCode = 1;
} finally {
  await browser?.close();
  database?.close();
  await stop();
  mock.closeAllConnections();
  await new Promise(resolve => mock.close(resolve));
}
