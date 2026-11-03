import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { once } from "node:events";
import { cp, mkdir, readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { DatabaseSync } from "node:sqlite";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

// 仅开发验收使用 Node、模拟网关和独立数据目录，发行客户端仍完全由 Rust 运行。
const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const output = path.join(project, "artifacts/reading-hybrid-verification");
const root = path.join(output, `exe-${Date.now()}`);
const require = createRequire(path.join(project, "web/package.json"));
const { chromium } = require("playwright");
const { expect } = require("@playwright/test");
const requests = [];
const checks = [];
let child, browser, database, serviceUrl;
let embeddingFailure = false;
const digest = bytes => createHash("sha256").update(bytes).digest("hex");

function pdf(texts) {
  const objects = [
    "<< /Type /Catalog /Pages 2 0 R >>", "",
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
  ];
  const pages = [];
  for (const text of texts) {
    const safe = text.replace(/[\\()]/g, ch => `\\${ch}`);
    const commands = `BT /F1 18 Tf 15 60 Td (${safe}) Tj ET`;
    const content = objects.push(`<< /Length ${Buffer.byteLength(commands)} >>\nstream\n${commands}\nendstream`);
    pages.push(objects.push(`<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 100] /Resources << /Font << /F1 3 0 R >> >> /Contents ${content} 0 R >>`));
  }
  objects[1] = `<< /Type /Pages /Kids [${pages.map(id => `${id} 0 R`).join(" ")}] /Count ${pages.length} >>`;
  let result = "%PDF-1.4\n";
  const offsets = [0];
  objects.forEach((object, i) => { offsets.push(Buffer.byteLength(result)); result += `${i + 1} 0 obj\n${object}\nendobj\n`; });
  const start = Buffer.byteLength(result);
  result += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
  result += offsets.slice(1).map(value => `${String(value).padStart(10, "0")} 00000 n \n`).join("");
  result += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${start}\n%%EOF\n`;
  return Buffer.from(result);
}

const mock = createServer(async (request, response) => {
  try {
    let raw = "";
    for await (const chunk of request) raw += chunk;
    const body = JSON.parse(raw);
    assert.equal(request.headers.authorization, "Bearer local-reading-fixture");
    requests.push({ url: request.url, body });
    if (request.url.endsWith("/embeddings") && embeddingFailure) {
      response.writeHead(503, { "Content-Type": "application/json" });
      response.end(JSON.stringify({ error: "本地模拟向量服务暂不可用。" }));
      return;
    }
    response.writeHead(200, { "Content-Type": "application/json" });
    if (request.url === "/custom/v3/embeddings") {
      assert(!("dimensions" in body));
      const data = body.input.map((text, index) => ({
        index, embedding: /quantum|microscopic|particles/i.test(text) ? [1, 0.05, 0.01] : [0.05, 1, 0.01],
      })).reverse();
      response.end(JSON.stringify({ data, usage: { prompt_tokens: 12, total_tokens: 12 } }));
      return;
    }
    assert.equal(request.url, "/custom/v3/chat/completions");
    const joined = JSON.stringify(body.messages);
    const viewed = [...joined.matchAll(/evidence_id=(evidence_[0-9a-f-]+)/g)].at(-1)?.[1];
    const tools = body.tools?.map(tool => tool.function.name) ?? [];
    let message;
    if (joined.includes("你为看不到文档图片")) message = { role: "assistant", content: "页面文字显示 quantum mechanics，当前提取的 wr0ng 应修正为 correct。" };
    else if (tools.includes("view_figure") && !viewed) message = { role: "assistant", content: null, tool_calls: [{
      id: randomUUID(), type: "function", function: { name: "view_figure", arguments: JSON.stringify({ locator: 1, question: "核对本页文字与当前 OCR。" }) },
    }] };
    else if (tools.includes("propose_ocr_correction") && viewed && !joined.includes("已保存第")) message = { role: "assistant", content: null, tool_calls: [{
      id: randomUUID(), type: "function", function: { name: "propose_ocr_correction", arguments: JSON.stringify({
        evidence_id: viewed, text: "quantum mechanics correct evidence", confidence: 0.96, summary: "将 wr0ng 修正为 correct。",
      }) },
    }] };
    else message = { role: "assistant", content: "已定位到第一页并提出 OCR 校正建议，等待您确认。[p.1]" };
    response.end(JSON.stringify({ choices: [{ message, finish_reason: message.tool_calls ? "tool_calls" : "stop" }],
      usage: { prompt_tokens: 10, completion_tokens: 8, total_tokens: 18 } }));
  } catch (error) {
    if (!response.headersSent) response.writeHead(500);
    response.end(JSON.stringify({ error: String(error) }));
  }
});
await new Promise(resolve => mock.listen(0, "127.0.0.1", resolve));
await mkdir(output, { recursive: true });
await cp(path.join(project, "artifacts/portable-package/DeepTutor"), root, { recursive: true });

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
      try { if ((await fetch(`${serviceUrl}/health`, { signal: AbortSignal.timeout(1000) })).ok) return; } catch { /* 等待启动完成。 */ }
    }
    await delay(250);
  }
  throw new Error("最终 EXE 启动超时");
}
async function stop() {
  if (!child || child.exitCode !== null) return;
  const exited = once(child, "exit");
  child.kill();
  await Promise.race([exited, delay(10_000)]);
}
async function api(endpoint, body, method = body === undefined ? "GET" : "POST") {
  const response = await fetch(`${serviceUrl}/api${endpoint}`, {
    method, signal: AbortSignal.timeout(120_000),
    ...(body === undefined ? {} : { headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }),
  });
  assert(response.ok, `${endpoint}: HTTP ${response.status} ${await response.clone().text()}`);
  return response.json();
}
async function waitReady(workspace) {
  for (let i = 0; i < 240; i++) {
    const status = await api(`/reading/workspaces/${workspace}/index`);
    if (status.status === "ready") return status;
    assert(!["failed", "cancelled"].includes(status.status), JSON.stringify(status));
    await delay(250);
  }
  throw new Error("阅读索引建立超时");
}
async function turn(workspace, material) {
  const socket = new WebSocket(serviceUrl.replace("http:", "ws:") + "/ws");
  const frames = [];
  let timer;
  try {
    await new Promise((resolve, reject) => {
      timer = setTimeout(() => reject(new Error("看图校正回合超时")), 90_000);
      socket.addEventListener("open", () => socket.send(JSON.stringify({
        type: "start_turn", protocol_version: "2.0", client_submission_id: randomUUID(),
        capability: "chat", workspace_mode: "immersive_reading", reading_workspace_id: workspace,
        reading_material_id: material, reading_material_revision: 1, reading_viewport: { locator: 100 },
        content: "microscopic particles 的证据在哪一页？请查看第一页并提出 OCR 校正建议。", enabled_tools: [],
      })));
      socket.addEventListener("error", () => reject(new Error("阅读 WebSocket 连接失败")));
      socket.addEventListener("message", event => {
        const frame = JSON.parse(event.data); frames.push(frame);
        if (frame.type === "protocol_error") reject(new Error(JSON.stringify(frame)));
        if (frame.type === "done") {
          if (frame.metadata.status !== "completed") reject(new Error(JSON.stringify(frame)));
          else resolve();
        }
      });
    });
  } finally { clearTimeout(timer); socket.close(); }
  return frames;
}

try {
  await start();
  database = new DatabaseSync(path.join(root, "data/deeptutor.sqlite3"));
  database.exec("PRAGMA busy_timeout=10000");
  const catalog = JSON.parse(await readFile(path.join(project, "src-tauri/tests/fixtures/referenced-catalog.json"), "utf8"));
  catalog.connections[0].base_url = `http://127.0.0.1:${mock.address().port}/custom/v3`;
  catalog.connections[0].api_key = "local-reading-fixture";
  for (const service of ["llm", "embedding"]) {
    catalog.services[service].active_profile_id = catalog.services[service].profiles[0].id;
    catalog.services[service].active_model_id = catalog.services[service].profiles[0].models[0].id;
    catalog.services[service].profiles[0].base_url = "https://api.openai.com/v1";
  }
  await api("/settings/catalog", catalog, "PUT");
  await api("/settings/ui", { language: "zh", response_language: "zh" }, "PUT");
  await api("/settings/tour/complete", {}, "POST");
  const bytes = pdf(["quantum mechanics wr0ng evidence", ...Array.from({ length: 120 }, (_, i) => `gardening soil physical page ${i + 2}`)]);
  const form = new FormData();
  form.append("file", new Blob([bytes], { type: "application/pdf" }), "混合检索验收.pdf");
  const uploaded = await fetch(`${serviceUrl}/api/reading/materials`, { method: "POST", body: form });
  assert(uploaded.ok, await uploaded.clone().text());
  const material = await uploaded.json();
  const id = material.material_id;
  assert.equal(material.unit_count, 121);
  const base = `/reading/materials/${id}`;
  const { workspace } = await api("/reading/workspaces", { title: "混合检索与校正验收", material_ids: [id] });
  const workspaceId = workspace.workspace_id;
  const original = await api(`${base}/units/1`);
  const keyword = await api(`/reading/workspaces/${workspaceId}/search`, { query: "quantum", limit: 4 });
  assert.equal(keyword.mode, "keyword");
  assert.equal(keyword.hits[0].locator, 1);
  assert.equal(requests.filter(r => r.url.endsWith("/embeddings")).length, 0);
  const rejected = await fetch(`${serviceUrl}/api/reading/retrieval/settings`, {
    method: "PUT", headers: { "Content-Type": "application/json" }, body: '{"enabled":true}',
  });
  assert.equal(rejected.status, 400);
  checks.push("默认关键词检索，首次开启必须明确确认文本发送");
  await api("/reading/retrieval/settings", { enabled: true, consented: true, workspace_id: workspaceId }, "PUT");
  await waitReady(workspaceId);
  const result = await api(`/reading/workspaces/${workspaceId}/search`, { query: "microscopic particles", limit: 4 });
  assert.equal(result.mode, "hybrid");
  assert.equal(result.hits[0].locator, 1);
  checks.push("121 页文档按物理页建立页与段落向量，自定义网关返回乱序向量仍正确定位第一页");
  const latePage = await api(`${base}/pages/121/image`);
  assert.equal(latePage.sha256.length, 64);
  const lateImage = await fetch(`${serviceUrl}${latePage.url}`);
  assert(lateImage.ok && lateImage.headers.get("content-type").startsWith("image/"));
  checks.push("第 121 页原始页图按需生成，不受前 120 页预览限制");
  await api(`${base}/annotations`, { annotation_id: "old-mark", locator: 1, quote: "wr0ng", kind: "highlight" }, "PUT");
  await api(`${base}/references/resolve`, { revision: 1, locator: 1, quote: "wr0ng" });
  const frames = await turn(workspaceId, id);
  await writeFile(path.join(output, "reading-turn-frames.json"), JSON.stringify(frames, null, 2), "utf8");
  const firstChat = requests.find(r => r.url.endsWith("/chat/completions") && r.body.tools?.some(tool => tool.function.name === "view_figure"));
  assert(firstChat, "未捕获到实际阅读回合的模型请求");
  const context = firstChat.body.messages.flatMap(message => typeof message.content === "string"
    ? [message.content] : (message.content ?? []).filter(part => part.type === "text").map(part => part.text)).join("\n");
  assert(context.includes("当前阅读资料集的检索结果（覆盖全部页面，不受当前视口限制）"));
  assert(context.includes('"locator":1') && context.includes("quantum mechanics wr0ng evidence"));
  const proposals = await api(`${base}/ocr-corrections`);
  assert.equal(proposals.pending_count, 1, JSON.stringify(frames));
  assert.equal((await api(`${base}/units/1`)).text, original.text);
  assert.equal((await api(base)).revision, 1);
  checks.push("实际阅读回合停留第 100 页仍检索第一页，看图后仅生成建议且不自动覆盖 OCR");
  const before = database.prepare("SELECT locator,kind,ordinal,hex(embedding) AS vector FROM reading_vectors WHERE material_id=? ORDER BY locator,kind,ordinal").all(id);
  const callCount = requests.filter(r => r.url.endsWith("/embeddings")).length;
  browser = await chromium.launch({ channel: "msedge", headless: true });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
  const errors = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.goto(serviceUrl);
  await page.waitForFunction(() => document.getElementById("root")?.childElementCount > 0);
  await page.evaluate(target => {
    window.history.pushState(null, "", target);
    window.dispatchEvent(new PopStateEvent("popstate"));
  }, `/learning/reading/${workspaceId}`);
  await page.locator('[data-reader-unit="1"] canvas').first().waitFor({ state: "visible", timeout: 60_000 });
  const review = page.getByRole("complementary", { name: "OCR 校正审核" });
  await review.getByRole("button", { name: /OCR 校正建议/ }).click();
  const editor = review.getByRole("textbox", { name: "校正后的完整页文本" });
  await expect(editor).toBeVisible();
  await editor.fill("quantum mechanics user confirmed correct evidence");
  await page.screenshot({ path: path.join(output, "ocr-review-before.png"), fullPage: true });
  await review.getByRole("button", { name: "确认并更新本页索引" }).click();
  await expect(editor).toBeHidden({ timeout: 20_000 });
  await waitReady(workspaceId);
  assert.equal((await api(base)).revision, 2);
  assert.equal((await api(`${base}/units/1`)).text, "quantum mechanics user confirmed correct evidence");
  const after = database.prepare("SELECT locator,kind,ordinal,hex(embedding) AS vector FROM reading_vectors WHERE material_id=? ORDER BY locator,kind,ordinal").all(id);
  assert.deepEqual(after.filter(r => r.locator !== 1), before.filter(r => r.locator !== 1));
  assert.equal(requests.filter(r => r.url.endsWith("/embeddings")).length, callCount + 1);
  const history = await api(`${base}/ocr-corrections`);
  assert.equal(history.items[0].suggested_text, "quantum mechanics correct evidence");
  assert.equal(history.items[0].final_text, "quantum mechanics user confirmed correct evidence");
  assert(history.items[0].original_ocr.includes("wr0ng"));
  assert.equal(history.items[0].prompt_version, "reading-ocr-v1");
  assert.equal((await api(`${base}/revisions/1/units/1`)).text, original.text);
  assert.equal((await api(`${base}/revisions`)).length, 2);
  const annotations = await api(`${base}/annotations`);
  assert.equal(annotations.find(r => r.annotation_id === "old-mark").resolution, "unresolved");
  assert.equal((await api(`${base}/references/resolve`, { revision: 1, locator: 1, quote: "wr0ng" })).resolution, "unresolved");
  assert.equal(digest(Buffer.from(await (await fetch(`${serviceUrl}/api${base}/raw`)).arrayBuffer())), digest(bytes));
  assert.deepEqual(errors, []);
  checks.push("真实界面编辑并逐页确认；仅第一页重新向量化，其余 120 页向量逐字节不变");
  checks.push("原件、原始 OCR、模型建议、用户最终文本和旧版本均保留，旧标注及引用重新校验");
  await page.screenshot({ path: path.join(output, "ocr-review-after.png"), fullPage: true });
  await page.evaluate(materialId => { window.location.hash = `dt-material-${materialId}-revision-1-locator-1`; }, id);
  const oldReference = page.getByRole("dialog", { name: "旧版引用校验" });
  await expect(oldReference).toBeVisible();
  await expect(oldReference).toContainText("quantum mechanics wr0ng evidence");
  await expect(oldReference).toContainText("quantum mechanics user confirmed correct evidence");
  await page.screenshot({ path: path.join(output, "old-reference-review.png"), fullPage: true });
  await oldReference.getByRole("button", { name: "打开当前页，手动核对" }).click();
  await expect(oldReference).toBeHidden();
  checks.push("点击旧版引用时实际界面展示历史原文与当前文本，允许人工核对后打开当前页");
  await browser.close(); browser = undefined;
  await stop();
  await start();
  assert.equal((await api(base)).revision, 2);
  assert.equal((await api(`${base}/units/1`)).text, "quantum mechanics user confirmed correct evidence");
  embeddingFailure = true;
  const degraded = await api(`/reading/workspaces/${workspaceId}/search`, { query: "confirmed" });
  assert.equal(degraded.mode, "keyword");
  assert.equal(degraded.hits[0].locator, 1);
  assert(degraded.warning.includes("503"));
  embeddingFailure = false;
  checks.push("向量网关返回 503 时有限重试并降级，关键词仍能定位校正后的第一页");
  await api("/reading/retrieval/settings", { enabled: false }, "PUT");
  assert.equal((await api(`/reading/workspaces/${workspaceId}/search`, { query: "confirmed" })).mode, "keyword");
  checks.push("重启后版本与校正保留，关闭向量后关键词检索继续工作");
  await api(`/reading/workspaces/${workspaceId}/index`, undefined, "DELETE");
  assert.equal(database.prepare("SELECT count(*) AS count FROM reading_vectors WHERE material_id=?").get(id).count, 0);
  assert.equal((await api(`/reading/workspaces/${workspaceId}/index`)).status, "not_indexed");
  catalog.services.embedding.profiles = [];
  catalog.services.embedding.active_profile_id = null;
  catalog.services.embedding.active_model_id = null;
  await api("/settings/catalog", catalog, "PUT");
  await api("/reading/retrieval/settings", { enabled: true, consented: true }, "PUT");
  const noModel = await api(`/reading/workspaces/${workspaceId}/search`, { query: "confirmed" });
  assert.equal(noModel.mode, "keyword");
  assert.equal(noModel.hits[0].locator, 1);
  assert(noModel.warning.includes("未配置向量模型"));
  checks.push("删除索引真实释放向量记录，未配置向量模型时继续使用关键词检索");
  await writeFile(path.join(output, "reading-hybrid-report.json"), JSON.stringify({
    passed: true, executable: path.join(root, "DeepTutor.exe"),
    executableSha256: digest(await readFile(path.join(root, "DeepTutor.exe"))),
    portableRoot: root, checks, embeddingRequests: requests.filter(r => r.url.endsWith("/embeddings")).length,
    runtimeNote: "视觉与向量服务均为本地模拟，用于验证路由和审核流程，不代表真实模型识别准确率。",
  }, null, 2) + "\n", "utf8");
  console.log(JSON.stringify({ passed: true, checks, output }, null, 2));
} catch (error) {
  await writeFile(path.join(output, "reading-hybrid-failure.json"), JSON.stringify({
    passed: false, error: String(error), stack: error.stack, checks, portableRoot: root,
  }, null, 2) + "\n", "utf8");
  const page = browser?.contexts()[0]?.pages()[0];
  if (page) await page.screenshot({ path: path.join(output, "ocr-review-failure.png"), fullPage: true }).catch(() => {});
  throw error;
} finally {
  await browser?.close();
  database?.close();
  await stop();
  await new Promise(resolve => mock.close(resolve));
}
