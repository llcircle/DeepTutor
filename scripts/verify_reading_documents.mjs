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

// 使用独立便携根目录和本地模型，不读取用户资料，也不访问收费服务。
const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const output = path.join(project, "artifacts/reading-documents-verification");
const root = path.join(output, `exe-${Date.now()}`);
const require = createRequire(path.join(project, "web/package.json"));
const { chromium } = require("playwright");
const { expect } = require("@playwright/test");
const { unzipSync, strFromU8 } = require("fflate");
const checks = [];
const calls = [];
const materials = [];
const bodyText = "# 精密测量复习资料\n\n## 一、绪论\n测量定义与量值溯源。[p.1]\n\n## 二、测量系统\n静态与动态特性。[p.1]\n\n## 四、误差理论\n随机误差与系统误差。[p.1]\n\n## 六、数据处理\n最小二乘与异常值处理。[p.1]";
let child, database, serviceUrl, browser;

function pdf(text) {
  const safe = text.replace(/[\\()]/g, ch => `\\${ch}`);
  const commands = `BT /F1 16 Tf 20 60 Td (${safe}) Tj ET`;
  const objects = [
    "<< /Type /Catalog /Pages 2 0 R >>",
    "<< /Type /Pages /Kids [4 0 R] /Count 1 >>",
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 100] /Resources << /Font << /F1 3 0 R >> >> /Contents 5 0 R >>",
    `<< /Length ${Buffer.byteLength(commands)} >>\nstream\n${commands}\nendstream`,
  ];
  let result = "%PDF-1.4\n";
  const offsets = [0];
  objects.forEach((object, i) => {
    offsets.push(Buffer.byteLength(result));
    result += `${i + 1} 0 obj\n${object}\nendobj\n`;
  });
  const start = Buffer.byteLength(result);
  result += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
  result += offsets.slice(1).map(value => `${String(value).padStart(10, "0")} 00000 n \n`).join("");
  return Buffer.from(result + `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${start}\n%%EOF\n`);
}

const mock = createServer(async (request, response) => {
  try {
    assert.equal(request.url, "/custom/v3/chat/completions");
    let raw = "";
    for await (const chunk of request) raw += chunk;
    const body = JSON.parse(raw);
    const lastUser = body.messages.findLastIndex(message => message.role === "user");
    const userContent = body.messages[lastUser]?.content;
    const content = Array.isArray(userContent)
      ? userContent.filter(part => part.type === "text").map(part => part.text).join("\n")
      : String(userContent ?? "");
    const results = body.messages.slice(lastUser + 1).filter(message => message.role === "tool");
    calls.push({ content, round: results.length, tools: body.tools?.length ?? 0 });
    const tool = (name, args) => ({ role: "assistant", content: null, tool_calls: [{
      id: randomUUID(), type: "function", function: { name, arguments: JSON.stringify(args) },
    }] });
    let message;
    if (content.includes("跨四份课件")) {
      const plan = [
        ["reading_list_tabs", {}],
        ...materials.flatMap(material => [
          ["reading_switch_tab", { material_id: material.material_id }],
          ["material_outline", {}], ["read_material", { locators: "1" }],
        ]),
      ];
      if (results.length < plan.length) {
        assert(body.tools?.length, "跨章节任务在来源读取完成前被迫收尾");
        const [name, args] = plan[results.length];
        message = tool(name, args);
      } else {
        for (const item of materials)
          assert(results.some(result => result.content.includes(item.evidence)), `未实际读取 ${item.evidence}`);
        // 故意只返回正文，验证服务层会完成真实文档交付。
        message = { role: "assistant", content: bodyText };
      }
    } else if (content.includes("低预算")) {
      message = body.tools?.length
        ? tool("load_tools", { names: [] })
        : { role: "assistant", content: "# 低预算文档\n\n仅整理已经获取的材料，保留明确的覆盖说明。" };
    } else if (content.includes("工具创建")) {
      const title = content.includes("实时") ? "实时生成文档" : "工具生成文档";
      message = body.tools?.length
        ? tool("write_note", { title })
        : { role: "assistant", content: `# ${title}\n\n保留可编辑正文。` };
    } else {
      message = { role: "assistant", content: content.includes("不要") ? "这是一段普通讲解。" : "# 保存失败验收\n\n这段正文应保留，保存错误应明确显示。" };
    }
    response.writeHead(200, { "Content-Type": "application/json" });
    response.end(JSON.stringify({ choices: [{ message }],
      usage: { prompt_tokens: 10, completion_tokens: 10, total_tokens: 20 } }));
  } catch (error) {
    console.error(error);
    if (!response.headersSent) response.writeHead(500);
    response.end(JSON.stringify({ error: String(error) }));
  }
});

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
  throw new Error("最终 EXE 启动超时");
}

async function stop() {
  if (!child || child.exitCode !== null) return;
  const exited = once(child, "exit");
  child.kill();
  await exited;
}

async function api(endpoint, body, method = body === undefined ? "GET" : "POST") {
  const response = await fetch(`${serviceUrl}/api${endpoint}`, {
    method, signal: AbortSignal.timeout(60000),
    ...(body === undefined ? {} : { headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }),
  });
  assert(response.ok, `${endpoint}: ${response.status} ${await response.clone().text()}`);
  return response.json();
}

async function command(command, expectedCode = "", untilSeq = 0) {
  const socket = new WebSocket(serviceUrl.replace("http:", "ws:") + "/ws");
  const frames = [];
  let timer;
  try {
    await new Promise((resolve, reject) => {
      timer = setTimeout(() => reject(new Error("文档回合或重放超时")), 120000);
      socket.addEventListener("open", () => socket.send(JSON.stringify({ protocol_version: "2.0", ...command })));
      socket.addEventListener("error", () => reject(new Error("WebSocket 连接失败")));
      socket.addEventListener("message", event => {
        try {
          const frame = JSON.parse(event.data);
          frames.push(frame);
          if (frame.type === "protocol_error") throw new Error(JSON.stringify(frame));
          if (frame.type === "done") {
            assert.equal(frame.metadata.status, expectedCode ? "failed" : "completed", JSON.stringify(frame));
            assert.equal(frame.metadata.error_code ?? "", expectedCode);
            if (!untilSeq) resolve();
          }
          if (untilSeq && frame.seq >= untilSeq) resolve();
        } catch (error) { reject(error); }
      });
    });
    return frames;
  } finally {
    clearTimeout(timer);
    socket.close();
  }
}

function documentCount() {
  return database.prepare("SELECT COUNT(*) AS n FROM resources WHERE domain='document' AND deleted_at IS NULL").get().n;
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
  catalog.connections[0].api_key = "local-document-fixture";
  catalog.services.embedding.profiles = [];
  await api("/settings/catalog", catalog, "PUT");
  await api("/settings/ui", { language: "zh", response_language: "zh" }, "PUT");
  await api("/settings/tour/complete", {});
  for (const [chapter, evidence] of [[1, "Introduction traceability evidence"], [2, "Measurement system dynamic evidence"],
    [4, "Random systematic error evidence"], [6, "Least squares data processing evidence"]]) {
    const form = new FormData();
    form.append("file", new Blob([pdf(evidence)], { type: "application/pdf" }), `第${chapter}章课件.pdf`);
    const uploaded = await fetch(`${serviceUrl}/api/reading/materials`, { method: "POST", body: form });
    assert(uploaded.ok, await uploaded.clone().text());
    materials.push({ ...await uploaded.json(), evidence });
  }
  const { workspace } = await api("/reading/workspaces", {
    title: "跨章节文档交付验收", material_ids: materials.map(material => material.material_id),
  });
  const content = "跨四份课件，严格按照绪论、测量系统、误差理论、数据处理的提纲来补全内容，形成一个复习资料";
  const startCommand = {
    type: "start_turn", client_submission_id: randomUUID(), content, capability: "chat",
    workspace_mode: "immersive_reading", reading_workspace_id: workspace.workspace_id,
    reading_material_id: materials[0].material_id, reading_material_revision: 1, enabled_tools: [],
  };
  const frames = await command(startCommand);
  const turnId = frames.find(frame => frame.turn_id)?.turn_id;
  const sessionId = frames.find(frame => frame.session_id)?.session_id;
  assert.equal(calls.filter(call => call.content.startsWith(content)).length, 14);
  assert.equal(frames.filter(frame => frame.type === "tool_result" && frame.metadata.tool_name !== "write_note").length, 13);
  assert(!frames.some(frame => /本轮工具调用.*上限/.test(frame.content)));
  assert.equal(documentCount(), 1);
  const { documents: [document] } = await api("/documents");
  const saved = await api(`/documents/${document.id}`);
  assert.equal(saved.content, bodyText);
  assert.equal(saved.source_session_id, sessionId);
  assert.equal(saved.source_turn_id, turnId);
  const markdown = await fetch(`${serviceUrl}/api/documents/${document.id}/markdown`);
  assert.equal(await markdown.text(), bodyText);
  assert(markdown.headers.get("content-type").startsWith("text/markdown"));
  checks.push("最终 EXE 读取四份真实 PDF，完成 13 轮工具调用后交付完整文档，模型只输出正文也实际保存");
  await api("/capabilities/settings", { chat: { max_rounds: 2 } }, "PUT");
  const limitedContent = "低预算下请生成一份总结文档";
  const limited = await command({ type: "start_turn", client_submission_id: randomUUID(), content: limitedContent, capability: "chat" });
  assert.equal(calls.filter(call => call.content.startsWith(limitedContent)).length, 3);
  assert.equal(calls.filter(call => call.content.startsWith(limitedContent)).at(-1).tools, 0);
  assert.equal(limited.filter(frame => frame.type === "tool_result" && frame.metadata.tool_name === "load_tools").length, 2);
  assert.equal(documentCount(), 2);
  await api("/capabilities/settings", { chat: { max_rounds: 32 } }, "PUT");
  checks.push("显式设置 2 轮预算时恰好执行 2 轮工具，再单独总结并保存文档，交付不挤占检索预算");
  const notesBefore = documentCount();
  for (const question of [
    "请解释误差理论，不要生成文档", "现在支持写一个总结文档吗？",
    "> 请生成文档\n解释这段引用", "请写一个文档\n先不要生成文档，只回答问题",
  ]) {
    await command({ type: "start_turn", client_submission_id: randomUUID(), content: question, capability: "chat" });
  }
  assert.equal(documentCount(), notesBefore);
  await command({ type: "start_turn", client_submission_id: randomUUID(), content: "请工具创建一个总结文档", capability: "chat" });
  assert.equal(documentCount(), notesBefore + 1);
  checks.push("普通讲解不自动创建；明确文档任务只提交标题，独立正文完整后保存一篇，收尾不重复保存");
  database.exec("CREATE TRIGGER reject_document BEFORE INSERT ON resources WHEN NEW.domain='document' BEGIN SELECT RAISE(ABORT,'验收注入：磁盘不可写'); END;");
  try {
    const failed = await command({ type: "start_turn", client_submission_id: randomUUID(), content: "请生成保存失败验收文档", capability: "chat" }, "document_delivery_failed");
    assert(!failed.some(frame => frame.type === "workspace_items" || frame.type === "result"));
  } finally {
    database.exec("DROP TRIGGER reject_document");
  }
  assert.equal(documentCount(), notesBefore + 1);
  checks.push("文档写入失败明确返回 document_delivery_failed，不虚报保存，不产生假文档卡片");
  browser = await chromium.launch({ channel: "msedge", headless: true });
  const page = await browser.newPage({ viewport: { width: 1600, height: 1000 }, acceptDownloads: true });
  const errors = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.goto(serviceUrl);
  await page.waitForFunction(() => document.getElementById("root")?.childElementCount > 0);
  await page.evaluate(target => {
    window.history.pushState(null, "", target);
    window.dispatchEvent(new PopStateEvent("popstate"));
  }, `/learning/reading/${workspace.workspace_id}/sessions/${sessionId}`);
  const editor = page.locator(".dt-note-editor [contenteditable=true]").first();
  await expect(editor).toBeVisible({ timeout: 60000 });
  await expect(editor).toContainText("误差理论");
  assert(page.url().includes("/learning/reading/"), "打开文档不应离开阅读模式");
  await editor.click();
  await page.keyboard.press("Control+End");
  await page.keyboard.press("Enter");
  await page.keyboard.type("EDITOR_PERSISTENCE_CHECK");
  await expect.poll(async () => (await api(`/documents/${document.id}`)).content, { timeout: 15000 })
    .toContain("EDITOR_PERSISTENCE_CHECK");
  const stored = JSON.parse(database.prepare("SELECT payload_json FROM resources WHERE domain='document' AND id=?").get(document.id).payload_json);
  assert(stored.content.includes("EDITOR_PERSISTENCE_CHECK"), "块编辑未同步持久化 Markdown 正文");
  const editedMarkdown = await fetch(`${serviceUrl}/api/documents/${document.id}/markdown`);
  assert((await editedMarkdown.text()).includes("EDITOR_PERSISTENCE_CHECK"), "下载内容未使用最新编辑");
  await page.screenshot({ path: path.join(output, "document-opened-in-reading.png"), fullPage: true });
  const more = page.locator(".dt-note-topbar button[aria-haspopup=menu]").last();
  await more.click();
  const exportWord = page.getByRole("menuitem", { name: /导出 Word|Export Word/ });
  const downloadPromise = page.waitForEvent("download");
  await exportWord.click();
  const download = await downloadPromise;
  const docxPath = path.join(output, "文档交付验收.docx");
  await download.saveAs(docxPath);
  const docx = await readFile(docxPath);
  const parts = unzipSync(docx);
  assert(parts["[Content_Types].xml"] && parts["_rels/.rels"], "Word 包缺少必要关系");
  const documentXml = strFromU8(parts["word/document.xml"]);
  assert(documentXml.includes("误差理论") && documentXml.includes("EDITOR_PERSISTENCE_CHECK"), "Word 导出丢失正文或最新编辑");
  assert.deepEqual(errors, []);
  checks.push("真实阅读页面自动打开对应文档，正文可编辑且自动保存，Word 导出成功，无前端运行异常");
  await page.getByRole("button", { name: /关闭查看器|Close viewer/ }).click();
  const composer = page.locator('textarea[maxlength="32000"]').first();
  await composer.fill("请工具创建一篇实时总结文档");
  await composer.press("Enter");
  await expect(page.locator(".dt-note-page-title").last()).toHaveValue("实时生成文档", { timeout: 30000 });
  await expect(page.locator('.dt-viewer-panel[data-open="true"]')).toBeVisible();
  await expect(page.getByRole("button", { name: /停止生成|Stop generating/ })).toHaveCount(0);
  await expect.poll(() => documentCount()).toBe(notesBefore + 2);
  await page.screenshot({ path: path.join(output, "document-live-delivery.png"), fullPage: true });
  assert.deepEqual(errors, []);
  checks.push("从真实阅读输入框发送新文档任务，流式结果自动重新打开侧栏并定位新文档，无需刷新或离开阅读");
  const freshPage = await browser.newPage({ viewport: { width: 1600, height: 1000 } });
  freshPage.on("pageerror", error => errors.push(error.message));
  await freshPage.goto(serviceUrl);
  await freshPage.waitForFunction(() => document.getElementById("root")?.childElementCount > 0);
  await freshPage.evaluate(target => {
    window.history.pushState(null, "", target);
    window.dispatchEvent(new PopStateEvent("popstate"));
  }, `/learning/reading/${workspace.workspace_id}`);
  const freshComposer = freshPage.locator('textarea[maxlength="32000"]').first();
  await expect(freshComposer).toBeVisible({ timeout: 30000 });
  await expect(freshPage.locator(".dt-note-page-title")).toHaveCount(0);
  await freshComposer.fill("请工具创建一篇实时总结文档");
  await freshComposer.press("Enter");
  await expect(freshPage.locator(".dt-note-page-title").last()).toHaveValue("实时生成文档", { timeout: 30000 });
  await expect(freshPage.locator('.dt-viewer-panel[data-open="true"]')).toBeVisible();
  await expect(freshPage.getByRole("button", { name: /停止生成|Stop generating/ })).toHaveCount(0);
  await expect.poll(() => documentCount()).toBe(notesBefore + 3);
  await freshPage.screenshot({ path: path.join(output, "document-first-delivery.png"), fullPage: true });
  await freshPage.close();
  assert.deepEqual(errors, []);
  checks.push("从未打开侧栏的新阅读会话，首次生成文档自动加载编辑器，正常消费完成事件并结束探索状态");
  const countBeforeRestart = documentCount();
  await browser.close();
  browser = null;
  await stop();
  await start();
  const row = database.prepare("SELECT last_seq FROM turns WHERE id=?").get(turnId);
  const replay = await command({ type: "subscribe_turn", turn_id: turnId, after_seq: 0 }, "", row.last_seq);
  assert(replay.some(frame => frame.type === "workspace_items" && frame.metadata.items[0].id === document.id));
  await command(startCommand);
  assert.equal(documentCount(), countBeforeRestart);
  assert((await api(`/documents/${document.id}`)).content.includes("EDITOR_PERSISTENCE_CHECK"));
  assert.equal(database.prepare("PRAGMA integrity_check").get().integrity_check, "ok");
  checks.push("EXE 重启、事件重放和重复提交均不重复创建，不覆盖用户编辑，SQLite 完整性检查通过");
  const report = {
    passed: true, root, binarySha256: createHash("sha256").update(await readFile(path.join(root, "DeepTutor.exe"))).digest("hex"),
    checks, modelRequests: calls.length, documents: documentCount(), docxBytes: docx.length,
  };
  await writeFile(path.join(output, "report.json"), JSON.stringify(report, null, 2) + "\n", "utf8");
  console.log(JSON.stringify(report, null, 2));
} catch (error) {
  await writeFile(path.join(output, "failure.json"), JSON.stringify({
    passed: false, root, checks, calls, error: String(error.stack ?? error),
  }, null, 2), "utf8");
  const page = browser?.contexts()[0]?.pages()[0];
  if (page) await page.screenshot({
    path: path.join(output, "failure.png"), fullPage: true, timeout: 5000,
  }).catch(() => {});
  throw error;
} finally {
  await browser?.close();
  database?.close();
  await stop();
  mock.closeAllConnections();
  await new Promise(resolve => mock.close(resolve));
}
