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

// 默认只使用本地模型夹具；--live 需用户已授权，仅读取真实模型配置到隔离目录。
const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const output = path.join(project, "artifacts/session-resume-verification");
const root = path.join(output, `exe-${Date.now()}`);
const require = createRequire(path.join(project, "web/package.json"));
const { chromium } = require("playwright");
const { expect } = require("@playwright/test");
const checks = [];
const requests = [];
const streams = new Map();
let child, database, serviceUrl, browser, page;
function passed(message) {
  checks.push(message);
  console.log(message);
}

const mock = createServer(async (request, response) => {
  try {
    assert.equal(request.url, "/custom/v3/chat/completions");
    let raw = "";
    for await (const chunk of request) raw += chunk;
    const body = JSON.parse(raw);
    const content = body.messages.findLast(message => message.role === "user")?.content;
    const text = Array.isArray(content)
      ? content.filter(part => part.type === "text").map(part => part.text).join("\n")
      : String(content ?? "");
    const mode = text.match(/^恢复验收[^\r\n]*/)?.[0] ?? text;
    requests.push(mode);
    response.writeHead(200, { "Content-Type": "text/event-stream" });
    const delta = content => response.write(
      `data: ${JSON.stringify({ choices: [{ delta: { content } }] })}\n\n`,
    );
    if (mode.startsWith("恢复验收")) {
      assert(!streams.has(mode), "切换页面重复启动了模型请求");
      const prefix = `${mode}：切换之前的正文。`;
      delta(prefix);
      streams.set(mode, {
        prefix, delta, response,
        finish() {
          delta("切回之后的正文。");
          response.end("data: [DONE]\n\n");
        },
      });
    } else {
      delta("普通对话验收完成。");
      response.end("data: [DONE]\n\n");
    }
  } catch (error) {
    console.error(error);
    if (!response.headersSent) response.writeHead(500);
    response.end();
  }
});

function pdf() {
  const commands = "BT /F1 16 Tf 20 60 Td (Session resume evidence) Tj ET";
  const objects = [
    "<< /Type /Catalog /Pages 2 0 R >>",
    "<< /Type /Pages /Kids [4 0 R] /Count 1 >>",
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 100] /Resources << /Font << /F1 3 0 R >> >> /Contents 5 0 R >>",
    `<< /Length ${commands.length} >>\nstream\n${commands}\nendstream`,
  ];
  let result = "%PDF-1.4\n";
  const offsets = [0];
  objects.forEach((object, i) => {
    offsets.push(Buffer.byteLength(result));
    result += `${i + 1} 0 obj\n${object}\nendobj\n`;
  });
  const start = Buffer.byteLength(result);
  result += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
  result += offsets.slice(1).map(offset => `${String(offset).padStart(10, "0")} 00000 n \n`).join("");
  return Buffer.from(result + `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${start}\n%%EOF\n`);
}

async function start() {
  child = spawn(path.join(root, "DeepTutor.exe"), [], {
    cwd: root, windowsHide: true, stdio: "ignore",
    env: { ...process.env, DEEPTUTOR_PORTABLE_ROOT: root, RUST_LOG: "deeptutor_desktop_lib=info,tauri=warn" },
  });
  let launchError;
  child.on("error", error => { launchError = error; });
  for (let i = 0; i < 160; i++) {
    if (launchError) throw launchError;
    assert.equal(child.exitCode, null, "隔离 EXE 提前退出");
    const log = await readFile(path.join(root, "logs/desktop.log"), "utf8").catch(() => "");
    serviceUrl = [...log.matchAll(/(?:^|\s)url=(http:\/\/127\.0\.0\.1:\d+)/g)].at(-1)?.[1];
    if (serviceUrl) {
      try {
        if ((await fetch(`${serviceUrl}/health`, { signal: AbortSignal.timeout(1000) })).ok) return;
      } catch { /* 等待本地服务。 */ }
    }
    await delay(250);
  }
  throw new Error("隔离 EXE 启动超时");
}

async function api(endpoint, body, method = body === undefined ? "GET" : "POST") {
  const response = await fetch(`${serviceUrl}/api${endpoint}`, {
    method, signal: AbortSignal.timeout(30000),
    ...(body === undefined ? {} : { headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }),
  });
  assert(response.ok, `${endpoint}: ${response.status}`);
  return response.json();
}

async function chat(command) {
  const socket = new WebSocket(serviceUrl.replace("http:", "ws:") + "/ws");
  let timer;
  try {
    return await new Promise((resolve, reject) => {
      timer = setTimeout(() => reject(new Error("初始化会话超时")), 30000);
      socket.addEventListener("error", () => reject(new Error("WebSocket 连接失败")));
      socket.addEventListener("open", () => socket.send(JSON.stringify({
        type: "start_turn", protocol_version: "2.0", client_submission_id: randomUUID(),
        capability: "chat", enabled_tools: [], ...command,
      })));
      socket.addEventListener("message", event => {
        const frame = JSON.parse(event.data);
        if (frame.type === "protocol_error") reject(new Error(JSON.stringify(frame)));
        if (frame.type === "done") {
          if (frame.metadata.status !== "completed") reject(new Error(JSON.stringify(frame)));
          else resolve(frame.session_id);
        }
      });
    });
  } finally {
    clearTimeout(timer);
    socket.close();
  }
}

async function navigate(target, waitForSession = true) {
  const sessionId = target.match(/\/sessions\/([^/]+)$/)?.[1];
  const loaded = sessionId && waitForSession
    ? page.waitForResponse(response => new URL(response.url()).pathname === `/api/sessions/${sessionId}`)
    : null;
  await page.evaluate(target => {
    window.history.pushState(null, "", target);
    window.dispatchEvent(new PopStateEvent("popstate"));
  }, target);
  await expect.poll(() => new URL(page.url()).pathname).toBe(target);
  await loaded;
}

const composer = () => page.locator('textarea[maxlength="32000"]').first();
const stopButton = () => page.getByRole("button", { name: /停止生成|Stop generating/ });
const transcript = () => page.locator("[data-chat-scroll-root]").last();
async function send(text) {
  await expect(composer()).toBeVisible({ timeout: 30000 });
  await composer().fill(text);
  await composer().press("Enter");
  await expect.poll(() => streams.has(text), { timeout: 30000 }).toBe(true);
  await expect(transcript()).toContainText(streams.get(text).prefix, { timeout: 30000 });
}

async function finish(text, sessionId) {
  const stream = streams.get(text);
  stream.finish();
  await expect(stopButton()).toHaveCount(0, { timeout: 30000 });
  await expect(transcript()).toContainText(stream.prefix + "切回之后的正文。");
  assert.equal((await transcript().innerText()).split(stream.prefix).length - 1, 1, "正文被重复重放");
  const saved = await api(`/sessions/${sessionId}`);
  assert.equal(saved.messages.filter(message => message.role === "assistant").at(-1).content,
    stream.prefix + "切回之后的正文。");
  assert.equal(requests.filter(value => value === text).length, 1);
}

await mkdir(output, { recursive: true });
await cp(path.join(project, "artifacts/portable-package/DeepTutor"), root, { recursive: true });
await new Promise(resolve => mock.listen(0, "127.0.0.1", resolve));
try {
  await start();
  database = new DatabaseSync(path.join(root, "data/deeptutor.sqlite3"));
  database.exec("PRAGMA busy_timeout=10000");
  const catalog = JSON.parse(await readFile(path.join(project, "src-tauri/tests/fixtures/referenced-catalog.json"), "utf8"));
  catalog.connections[0].base_url = `http://127.0.0.1:${mock.address().port}/custom/v3`;
  catalog.connections[0].api_key = "local-resume-fixture";
  catalog.services.embedding.profiles = [];
  await api("/settings/catalog", catalog, "PUT");
  await api("/settings/ui", { language: "zh", response_language: "zh" }, "PUT");
  await api("/settings/tour/complete", {});
  const form = new FormData();
  form.append("file", new Blob([pdf()], { type: "application/pdf" }), "对话恢复验收.pdf");
  const uploaded = await fetch(`${serviceUrl}/api/reading/materials`, { method: "POST", body: form });
  assert(uploaded.ok);
  const material = await uploaded.json();
  const { workspace } = await api("/reading/workspaces", {
    title: "切换会话恢复验收", material_ids: [material.material_id],
  });
  const base = { workspace_mode: "immersive_reading", reading_workspace_id: workspace.workspace_id,
    reading_material_id: material.material_id };
  const first = await chat({ ...base, content: "会话甲初始化" });
  const second = await chat({ ...base, content: "会话乙初始化" });
  const readingRoute = id => `/learning/reading/${workspace.workspace_id}/sessions/${id}`;
  const errors = [];
  browser = await chromium.launch({ channel: "msedge", headless: true });
  page = await browser.newPage({ viewport: { width: 1600, height: 1000 } });
  await page.addInitScript(() => {
    const NativeSocket = window.WebSocket;
    window.__resumeSockets = [];
    window.WebSocket = class extends NativeSocket {
      constructor(...args) {
        super(...args);
        window.__resumeSockets.push(this);
      }
    };
  });
  page.on("pageerror", error => errors.push(error.message));
  await page.goto(serviceUrl);
  await navigate(readingRoute(first));
  await send("恢复验收同页切换");
  await navigate(readingRoute(second));
  await expect(transcript()).toContainText("会话乙初始化");
  await navigate(readingRoute(first));
  await expect(transcript()).toContainText(streams.get("恢复验收同页切换").prefix);
  await finish("恢复验收同页切换", first);
  passed("阅读会话甲生成中切到乙再回甲，保留原正文、继续显示增量并完成，不重复请求模型");

  await send("恢复验收快速切换");
  let releaseSecond, secondRequested, secondReleased;
  const released = new Promise(resolve => { releaseSecond = resolve; });
  const requested = new Promise(resolve => { secondRequested = resolve; });
  const continued = new Promise(resolve => { secondReleased = resolve; });
  const delayedRoute = async route => {
    secondRequested();
    await released;
    await route.continue().catch(() => {});
    secondReleased();
  };
  await page.route(`**/api/sessions/${second}*`, delayedRoute);
  await navigate(readingRoute(second), false);
  await Promise.race([requested, delay(10000).then(() => { throw new Error("没有拦截到乙会话的加载请求"); })]);
  await navigate(readingRoute(first), false);
  releaseSecond();
  await Promise.race([continued, delay(10000).then(() => { throw new Error("乙会话的旧请求没有释放"); })]);
  await page.unroute(`**/api/sessions/${second}*`, delayedRoute);
  await expect(transcript()).toContainText(streams.get("恢复验收快速切换").prefix);
  await finish("恢复验收快速切换", first);
  passed("乙会话请求被刻意延迟时快速切回甲，旧请求取消且不会把甲覆盖为乙");

  await send("恢复验收长任务");
  // 只修改隔离库的会话时间，模拟开始五分钟前且后端仍在执行的任务。
  database.prepare("UPDATE sessions SET updated_at=? WHERE id=?").run(Date.now() / 1000 - 300, first);
  await navigate("/settings/general");
  await expect(composer()).toHaveCount(0);
  await navigate(readingRoute(first));
  await expect(transcript()).toContainText(streams.get("恢复验收长任务").prefix);
  await finish("恢复验收长任务", first);
  passed("超过空闲窗口的真实活动任务，离开阅读到设置后切回，仍能从事件库恢复并完成");

  await send("恢复验收刷新");
  await page.reload();
  await expect(transcript()).toContainText(streams.get("恢复验收刷新").prefix, { timeout: 30000 });
  await finish("恢复验收刷新", first);
  passed("生成中刷新页面，从零重放完整正文且完成后可继续发送");

  await send("恢复验收断线");
  await page.evaluate(() => {
    for (const socket of window.__resumeSockets) {
      if (socket.readyState === WebSocket.OPEN) socket.close();
    }
  });
  await finish("恢复验收断线", first);
  passed("生成中断开 WebSocket，重连补齐断线期间正文及完成事件，正文不重复");

  await send("恢复验收后台完成");
  await navigate("/settings/general");
  streams.get("恢复验收后台完成").finish();
  await expect.poll(() => database.prepare("SELECT status FROM sessions WHERE id=?").get(first).status,
    { timeout: 30000 }).toBe("completed");
  await navigate(readingRoute(first));
  await expect(transcript()).toContainText(streams.get("恢复验收后台完成").prefix + "切回之后的正文。");
  await expect(stopButton()).toHaveCount(0);
  passed("离开页面期间完成的任务，返回后显示持久化的完整答案且输入框正常");

  await send("恢复验收停止");
  await navigate(readingRoute(second));
  await expect(transcript()).toContainText("会话乙初始化");
  await navigate(readingRoute(first));
  await expect(transcript()).toContainText(streams.get("恢复验收停止").prefix);
  await stopButton().click();
  await expect(stopButton()).toHaveCount(0, { timeout: 30000 });
  await expect.poll(() => database.prepare("SELECT status FROM sessions WHERE id=?").get(first).status,
    { timeout: 30000 }).toBe("cancelled");
  await composer().fill("停止之后继续发送");
  await composer().press("Enter");
  await expect(transcript()).toContainText("普通对话验收完成。");
  await expect.poll(() => database.prepare("SELECT status FROM sessions WHERE id=?").get(first).status,
    { timeout: 30000 }).toBe("completed");
  await expect(stopButton()).toHaveCount(0);
  await expect(transcript()).toContainText("停止之后继续发送");
  const continuation = database.prepare("SELECT parent_message_id FROM messages WHERE session_id=? AND content=?")
    .get(first, "停止之后继续发送");
  assert(continuation.parent_message_id > 0, "取消后继续发送被错误保存为根分支");
  await navigate(readingRoute(second));
  await expect(transcript()).toContainText("会话乙初始化");
  await navigate(readingRoute(first));
  await expect(transcript()).toContainText("停止之后继续发送");
  await page.reload();
  await expect(transcript()).toContainText("停止之后继续发送", { timeout: 30000 });
  await expect(stopButton()).toHaveCount(0);
  passed("切回后停止仍能取消对应后台任务，继续发送后切换和刷新均保留新消息及正确分支");

  const ordinary = await chat({ content: "普通会话初始化" });
  await navigate(`/chat/${ordinary}`);
  await expect(transcript()).toContainText("普通会话初始化");
  await send("恢复验收普通对话");
  await navigate("/settings/general");
  await navigate(`/chat/${ordinary}`);
  await expect(transcript()).toContainText(streams.get("恢复验收普通对话").prefix);
  await finish("恢复验收普通对话", ordinary);
  passed("普通对话生成中离开到设置再返回，正文、增量和终态均正常");
  await navigate(readingRoute(first));

  if (process.argv.includes("--live")) {
    const source = new DatabaseSync(path.join(project, "artifacts/portable/DeepTutor/data/deeptutor.sqlite3"),
      { readOnly: true });
    let realCatalog;
    try {
      const row = source.prepare("SELECT value_json FROM settings WHERE key='catalog'").get();
      assert(row, "用户未保存真实模型配置");
      realCatalog = JSON.parse(row.value_json);
    } finally {
      source.close();
    }
    await api("/settings/catalog", realCatalog, "PUT");
    realCatalog = null;
    const previousTurns = database.prepare("SELECT COUNT(*) AS n FROM turns WHERE session_id=?").get(first).n;
    await composer().fill("这是客户端恢复验收。请只用纯文本连续解释三十个基础数学概念，每个概念约80至100字。" +
      "不要调用工具，不保存文档。首行单独写“真实恢复验收开始”，最后单独写“真实恢复验收结束”。");
    await composer().press("Enter");
    await expect.poll(() => database.prepare("SELECT COUNT(*) AS n FROM turns WHERE session_id=?").get(first).n,
      { timeout: 60000 }).toBe(previousTurns + 1);
    const turn = database.prepare("SELECT id FROM turns WHERE session_id=? ORDER BY created_at DESC LIMIT 1").get(first);
    const partial = () => database.prepare(
      "SELECT event_json FROM turn_events WHERE turn_id=? AND json_extract(event_json,'$.type')='content' ORDER BY seq",
    ).all(turn.id).map(row => JSON.parse(row.event_json).content ?? "").join("");
    await expect.poll(partial, { timeout: 180000 }).toContain("真实恢复验收开始");
    const statusBeforeSwitch = database.prepare("SELECT status FROM turns WHERE id=?").get(turn.id).status;
    assert(["queued", "running"].includes(statusBeforeSwitch), "真实模型在切换前已结束，未覆盖生成中恢复");
    await navigate("/settings/general");
    await expect(composer()).toHaveCount(0);
    await navigate(readingRoute(first));
    await expect(transcript()).toContainText("真实恢复验收开始", { timeout: 30000 });
    await expect.poll(() => database.prepare("SELECT status FROM turns WHERE id=?").get(turn.id).status,
      { timeout: 360000 }).toBe("completed");
    await expect(stopButton()).toHaveCount(0, { timeout: 30000 });
    await expect(transcript()).toContainText("真实恢复验收结束");
    assert.equal((await transcript().innerText()).split("真实恢复验收开始").length - 1, 1);
    const realSaved = (await api(`/sessions/${first}`)).messages.filter(message => message.role === "assistant").at(-1);
    assert(realSaved.content.includes("真实恢复验收开始") && realSaved.content.includes("真实恢复验收结束"));
    assert(realSaved.content.length > 1000, "真实模型未生成足够长的恢复验收内容");
    const turnCount = database.prepare("SELECT COUNT(*) AS n FROM turns WHERE session_id=?").get(first).n;
    await composer().fill("只回复 REAL_RESUME_CONTINUE_OK，不调用工具。");
    await composer().press("Enter");
    await expect.poll(() => database.prepare("SELECT COUNT(*) AS n FROM turns WHERE session_id=?").get(first).n,
      { timeout: 30000 }).toBe(turnCount + 1);
    await expect.poll(() => database.prepare("SELECT status FROM sessions WHERE id=?").get(first).status,
      { timeout: 120000 }).toBe("completed");
    await expect(transcript()).toContainText("REAL_RESUME_CONTINUE_OK");
    await expect(stopButton()).toHaveCount(0);
    passed(`用户当前配置的真实模型：生成 ${realSaved.content.length} 字符期间切到设置再返回，完整收尾且可继续对话`);
  }
  assert.deepEqual(errors, []);
  await page.screenshot({ path: path.join(output, "session-resume.png"), fullPage: true });
  const executable = await readFile(path.join(root, "DeepTutor.exe"));
  await writeFile(path.join(output, "report.json"), JSON.stringify({
    verified_at: new Date().toISOString(), root,
    exe_sha256: createHash("sha256").update(executable).digest("hex"),
    checks, page_errors: errors, requests: requests.length,
  }, null, 2));
  console.log(JSON.stringify({ checks, root }, null, 2));
} catch (error) {
  await page?.screenshot({ path: path.join(output, "failure.png"), fullPage: true }).catch(() => {});
  throw error;
} finally {
  for (const stream of streams.values()) stream.response.end();
  await browser?.close();
  if (child && child.exitCode === null) {
    const exited = once(child, "exit");
    child.kill();
    await exited;
  }
  database?.close();
  mock.closeAllConnections();
  await new Promise(resolve => mock.close(resolve));
}
