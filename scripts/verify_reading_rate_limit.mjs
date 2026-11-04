import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { cp, mkdir, readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

// 使用隔离 EXE 副本和本地限流网关，不读取用户配置、不调用真实收费接口。
const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const output = path.join(project, "artifacts/reading-rate-verification");
const root = path.join(output, `exe-${Date.now()}`);
const checks = [];
const requests = [];
function passed(label) { checks.push(label); console.log(`通过：${label}`); }
let child, database, serviceUrl;
let onceLimited = false;
let persistent = false;
let dateLimited = false;
let dateDeadline;

function pdf(texts) {
  const objects = ["<< /Type /Catalog /Pages 2 0 R >>", "", "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"];
  const pages = [];
  for (const text of texts) {
    const content = `BT /F1 18 Tf 15 60 Td (${text}) Tj ET`;
    const stream = objects.push(`<< /Length ${Buffer.byteLength(content)} >>\nstream\n${content}\nendstream`);
    pages.push(objects.push(`<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 100] /Resources << /Font << /F1 3 0 R >> >> /Contents ${stream} 0 R >>`));
  }
  objects[1] = `<< /Type /Pages /Kids [${pages.map(id => `${id} 0 R`).join(" ")}] /Count ${pages.length} >>`;
  let data = "%PDF-1.4\n";
  const offsets = [0];
  objects.forEach((object, index) => { offsets.push(Buffer.byteLength(data)); data += `${index + 1} 0 obj\n${object}\nendobj\n`; });
  const start = Buffer.byteLength(data);
  data += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
  data += offsets.slice(1).map(offset => `${String(offset).padStart(10, "0")} 00000 n \n`).join("");
  data += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${start}\n%%EOF\n`;
  return Buffer.from(data);
}

const mock = createServer(async (request, response) => {
  try {
    let raw = "";
    for await (const chunk of request) raw += chunk;
    const body = JSON.parse(raw);
    assert.equal(request.url, "/rate/v3/embeddings");
    assert.equal(request.headers.authorization, "Bearer local-rate-fixture");
    assert.equal(body.model, "bge-m3");
    const call = { at: Date.now(), texts: body.input, status: 200 };
    requests.push(call);
    let retryAfter;
    if (!onceLimited && body.input.some(text => text.includes("slow second page"))) {
      onceLimited = true; retryAfter = "6";
    } else if (persistent && body.input.some(text => text.includes("blocked second page"))) {
      retryAfter = "0";
    } else if (dateLimited && body.input.includes("HTTP date question")) {
      dateLimited = false;
      dateDeadline = Math.floor((Date.now() + 8000) / 1000) * 1000;
      retryAfter = new Date(dateDeadline).toUTCString();
    }
    if (retryAfter !== undefined) {
      call.status = 429; call.retryAfter = retryAfter;
      response.writeHead(429, { "Content-Type": "application/json", "Retry-After": retryAfter });
      response.end(JSON.stringify({ error: { code: "rate_limit_exceeded", message: "本地限流验收" } }));
      return;
    }
    response.writeHead(200, { "Content-Type": "application/json" });
    response.end(JSON.stringify({ data: body.input.map((_, index) => ({ index, embedding: [1, 0.2, 0.1] })) }));
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
      try { if ((await fetch(`${serviceUrl}/health`, { signal: AbortSignal.timeout(1000) })).ok) return; } catch { /* 等待本地服务。 */ }
    }
    await delay(250);
  }
  throw new Error("EXE 启动超时");
}

async function stop() {
  if (!child || child.exitCode !== null) return;
  const exited = once(child, "exit");
  child.kill();
  await Promise.race([exited, delay(10000)]);
}

async function api(endpoint, body, method = body === undefined ? "GET" : "POST") {
  const response = await fetch(`${serviceUrl}/api${endpoint}`, {
    method, signal: AbortSignal.timeout(30000),
    ...(body === undefined ? {} : { headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }),
  });
  assert(response.ok, `${endpoint}: HTTP ${response.status} ${await response.clone().text()}`);
  return response.json();
}

async function waitStatus(workspace, predicate, timeout = 180000) {
  const end = Date.now() + timeout;
  let status;
  while (Date.now() < end) {
    status = await api(`/reading/workspaces/${workspace}/index`);
    if (predicate(status)) return status;
    await delay(100);
  }
  throw new Error(`索引等待超时：${JSON.stringify(status)}`);
}

async function importWorkspace(texts) {
  const form = new FormData();
  form.append("file", new Blob([pdf(texts)], { type: "application/pdf" }), "限流验收.pdf");
  const response = await fetch(`${serviceUrl}/api/reading/materials`, { method: "POST", body: form });
  assert(response.ok, await response.clone().text());
  const material = await response.json();
  assert.equal(material.unit_count, texts.length);
  const { workspace } = await api("/reading/workspaces", { title: "限流验收资料集", material_ids: [material.material_id] });
  return { material: material.material_id, workspace: workspace.workspace_id };
}

function pageVectors(material, locator) {
  return database.prepare("SELECT kind, ordinal, hex(embedding) AS vector FROM reading_vectors WHERE material_id=? AND locator=? ORDER BY kind,ordinal").all(material, locator);
}

try {
  await start();
  database = new DatabaseSync(path.join(root, "data/deeptutor.sqlite3"));
  database.exec("PRAGMA busy_timeout=10000");
  const catalog = JSON.parse(await readFile(path.join(project, "src-tauri/tests/fixtures/referenced-catalog.json"), "utf8"));
  catalog.connections[0].base_url = `http://127.0.0.1:${mock.address().port}/rate/v3`;
  catalog.connections[0].api_key = "local-rate-fixture";
  await api("/settings/apply", { catalog });
  const first = await importWorkspace(["stable first page evidence", "slow second page evidence", "third page evidence", "fourth page evidence"]);
  await api("/reading/retrieval/settings", { enabled: true, consented: true, workspace_id: first.workspace }, "PUT");
  const waiting = await waitStatus(first.workspace, value => value.status === "waiting" && value.wait_reason?.includes("429"));
  assert.equal(waiting.completed_pages, 1);
  assert.equal(waiting.total_pages, 4);
  assert.equal(waiting.model, "bge-m3");
  assert(!waiting.error);
  assert(Date.parse(waiting.retry_at) >= requests[1].at + 5900);
  assert(requests[1].at - requests[0].at >= 1950, "正常请求应至少间隔两秒");
  const original = pageVectors(first.material, 1);
  assert(original.length > 0);
  const atQuery = Date.now();
  const fallback = await api(`/reading/workspaces/${first.workspace}/search`, { query: "stable first page" });
  assert.equal(fallback.mode, "keyword");
  assert.equal(fallback.hits[0].locator, 1);
  assert(Date.now() - atQuery < 3500, "查询不得等待长时间索引退避");
  assert.equal(requests.length, 2, "冷却期间查询不能继续请求向量网关");
  passed("正常请求间隔至少两秒；429 显示等待状态、预计时间和完整页进度");
  passed("索引限流期间查询在两秒左右回退关键词，不新增网关请求");

  await api("/reading/retrieval/settings", { enabled: false }, "PUT");
  const cancelled = await waitStatus(first.workspace, value => value.status === "cancelled", 3000);
  assert.equal(cancelled.completed_pages, 1);
  assert(!cancelled.retry_at);
  await delay(200);
  assert.equal(requests.length, 2);
  await api("/reading/retrieval/settings", { enabled: true, consented: true }, "PUT");
  await api(`/reading/workspaces/${first.workspace}/index`, {});
  await waitStatus(first.workspace, value => value.status === "ready");
  assert(requests[2].at - requests[1].at >= 5950, "不能早于 Retry-After 再次请求");
  assert(requests[3].at - requests[2].at >= 3950, "限流后应维持加倍的正常间隔");
  assert.equal(requests.filter(call => call.texts.some(text => text.includes("stable first page"))).length, 1);
  assert.deepEqual(pageVectors(first.material, 1), original);
  const completedCount = requests.length;
  await api(`/reading/workspaces/${first.workspace}/index`, {});
  await waitStatus(first.workspace, value => value.status === "ready");
  assert.equal(requests.length, completedCount, "重复更新不能重传已完成页面");
  passed("等待可取消；六秒 Retry-After 不被手动更新绕过，之后四秒间隔续建");
  passed("断点续建不重发第一页，其向量逐字节不变；完整索引再次更新零请求");

  await api("/reading/retrieval/settings", { enabled: false }, "PUT");
  const blocked = await importWorkspace(["blocked stable first page", "blocked second page", "blocked third page"]);
  persistent = true;
  await api("/reading/retrieval/settings", { enabled: true, consented: true, workspace_id: blocked.workspace }, "PUT");
  const failed = await waitStatus(blocked.workspace, value => value.status === "failed");
  assert.equal(failed.completed_pages, 1);
  assert.equal(failed.total_pages, 3);
  assert.equal(failed.model, "bge-m3");
  assert(failed.error.includes("429"));
  assert(failed.error.includes("配额"));
  const blockedCalls = requests.filter(call => call.texts.some(text => text.includes("blocked second page")));
  assert.equal(blockedCalls.length, 5, "首次加四次重试后必须停止");
  for (let i = 1; i < blockedCalls.length; i++) {
    assert(blockedCalls[i].at - blockedCalls[i - 1].at >= [5, 10, 20, 40][i - 1] * 1000 - 50);
  }
  const blockedOriginal = pageVectors(blocked.material, 1);
  const stoppedCount = requests.length;
  for (let i = 0; i < 5; i++) await api(`/reading/workspaces/${blocked.workspace}/index`);
  await delay(1000);
  assert.equal(requests.length, stoppedCount, "状态轮询不得重新发送失败请求");
  passed("持续 429 仅额外重试四次，5/10/20/40 秒退避后停止，保留完成进度并提示配额");

  await stop();
  persistent = false;
  await start();
  await api(`/reading/workspaces/${blocked.workspace}/index`, {});
  await waitStatus(blocked.workspace, value => value.status === "ready");
  assert.deepEqual(pageVectors(blocked.material, 1), blockedOriginal);
  assert.equal(requests.filter(call => call.texts.some(text => text.includes("blocked stable first page"))).length, 1);
  passed("重启后缺失页继续建立，已成功持久化的第一页不重复向量化");

  await delay(2100);
  dateLimited = true;
  const dateResult = await api(`/reading/workspaces/${blocked.workspace}/search`, { query: "HTTP date question" });
  assert.equal(dateResult.mode, "keyword");
  assert(dateResult.warning.includes("429"));
  const dateCount = requests.length;
  const cooled = await api(`/reading/workspaces/${first.workspace}/search`, { query: "stable first page" });
  assert.equal(cooled.mode, "keyword");
  assert.equal(requests.length, dateCount, "不同资料集查询共享 HTTP 日期冷却");
  await delay(Math.max(0, dateDeadline - Date.now()) + 100);
  const recovered = await api(`/reading/workspaces/${blocked.workspace}/search`, { query: "HTTP date question" });
  assert.equal(recovered.mode, "hybrid");
  assert(requests.at(-1).at >= dateDeadline);
  passed("查询 429 不重试；不同资料集共享 HTTP 日期冷却，时间到达后恢复向量检索");

  const report = {
    passed: true, checks,
    executableSha256: createHash("sha256").update(await readFile(path.join(root, "DeepTutor.exe"))).digest("hex"),
    portableRoot: root, requests,
    runtimeNote: "仅本地模拟限流服务验收，不调用用户真实服务，也不代表提供方实际配额。",
  };
  await writeFile(path.join(output, "reading-rate-report.json"), JSON.stringify(report, null, 2) + "\n", "utf8");
  console.log(JSON.stringify({ passed: true, checks, output }, null, 2));
} catch (error) {
  await writeFile(path.join(output, "reading-rate-failure.json"), JSON.stringify({
    passed: false, error: String(error), stack: error.stack, checks, requests, portableRoot: root,
  }, null, 2) + "\n", "utf8");
  throw error;
} finally {
  database?.close();
  await stop();
  await new Promise(resolve => mock.close(resolve));
}
