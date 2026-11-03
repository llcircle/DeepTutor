import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { once } from "node:events";
import { cp, mkdir, readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

// Node 只负责开发验收；复制干净发行载荷，不读取用户的配置、密钥或数据库。
const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const output = path.join(project, "artifacts", "stream-runtime-verification");
const root = path.join(output, `exe-${Date.now()}`);
const stage = path.join(project, "artifacts", "portable-package", "DeepTutor");
const checks = [];
const records = [];
const thinkingChunks = 4000;
const contentChunks = 64;
const streamText = "并发正文".repeat(contentChunks);
let child;
let serviceUrl;
let database;

const mock = createServer(async (request, response) => {
  try {
    assert.equal(request.url, "/api/v3/chat/completions", "请求没有命中自定义网关");
    let raw = "";
    for await (const chunk of request) raw += chunk;
    const body = JSON.parse(raw);
    const mode = body.messages.filter(message => message.role === "user").at(-1)?.content;
    records.push({ mode, tools: Boolean(body.tools?.length) });
    if (mode === "完整JSON") {
      response.writeHead(200, { "Content-Type": "application/json" });
      response.end(JSON.stringify({ choices: [{ message: { content: [
        { type: "text", text: "完整" }, { type: "output_text", text: { value: "中文答案" } },
      ] } }], usage: { prompt_tokens: 1, completion_tokens: 1 } }, null, 2));
      return;
    }
    response.writeHead(200, { "Content-Type": "text/event-stream" });
    const event = value => `data: ${JSON.stringify(value)}\n\n`;
    const delta = value => event({ choices: [{ delta: value }] });
    if (mode === "工具上限" || mode === "工具违约") {
      if (body.tools?.length || mode === "工具违约") {
        response.end(delta({ tool_calls: [{ index: 0, id: `call-${randomUUID()}`,
          function: { name: "load_tools", arguments: '{"names":[]}' } }] }) + "data: [DONE]\n\n");
      } else {
        assert.equal(body.messages.filter(message => message.role === "tool").length, 7);
        response.end(delta({ content: "工具上限后的最终回答" }) + "data: [DONE]\n\n");
      }
    } else if (String(mode).startsWith("并发长流")) {
      for (let index = 0; index < thinkingChunks; index += 32) {
        let batch = "";
        for (let offset = 0; offset < Math.min(32, thinkingChunks - index); offset++)
          batch += delta({ reasoning_content: "推理片段" });
        response.write(batch);
        await delay(1);
      }
      for (let index = 0; index < contentChunks; index++)
        response.write(delta({ content: "并发正文" }));
      response.end("data: [DONE]\n\n");
    } else if (mode === "数组与末帧") {
      response.write(': 保活\r\nevent: message\r\ndata: {"choices":\r\ndata: [{"delta":{"content":[{"type":"text","text":"中文"}]}}]}\r\n\r\n');
      response.end('data: {"choices":[{"message":{"content":"中文末帧答案"}}]}');
    } else if (mode === "真正空响应") {
      response.end(event({ choices: [{ delta: {}, finish_reason: "stop" }] }) + "data: [DONE]\n\n");
    } else {
      response.end(delta({ content: "重启与落库验证答案" }) + "data: [DONE]\n\n");
    }
  } catch (error) {
    if (!response.headersSent) response.writeHead(500);
    response.end();
    console.error(error);
  }
});
await new Promise(resolve => mock.listen(0, "127.0.0.1", resolve));
const catalog = JSON.parse(await readFile(path.join(project, "src-tauri/tests/fixtures/referenced-catalog.json"), "utf8"));
catalog.connections[0].base_url = `http://127.0.0.1:${mock.address().port}/api/v3`;
catalog.connections[0].api_key = "local-stream-verification";
await mkdir(output, { recursive: true });
await cp(stage, root, { recursive: true });

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
        const response = await fetch(`${serviceUrl}/health`, { signal: AbortSignal.timeout(1000) });
        if (response.ok) return;
      } catch { /* 等待本地服务启动。 */ }
    }
    await delay(250);
  }
  throw new Error("最终 EXE 未在 30 秒内就绪");
}

async function stop() {
  if (!child || child.exitCode !== null) return;
  const exited = once(child, "exit");
  child.kill();
  await exited;
}

async function api(route, body, method = "POST") {
  const response = await fetch(`${serviceUrl}/api${route}`, {
    method, signal: AbortSignal.timeout(30000),
    ...(body === undefined ? {} : { headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }),
  });
  assert(response.ok, `${route} 返回 HTTP ${response.status}`);
  return response.json();
}

async function save() {
  await api("/settings/draft", { catalog, extensions: {} }, "PUT");
  const applied = await api("/settings/apply");
  assert.deepEqual(applied.catalog, catalog, "保存改变了网关配置");
}

async function socketCommand(command, expectedCode = "", untilSeq = 0) {
  const socket = new WebSocket(serviceUrl.replace("http:", "ws:") + "/ws");
  const frames = [];
  let timer;
  try {
    await new Promise((resolve, reject) => {
      timer = setTimeout(() => reject(new Error("聊天或重放在 120 秒内未完成")), 120000);
      socket.addEventListener("error", () => reject(new Error("WebSocket 连接失败")));
      socket.addEventListener("open", () => socket.send(JSON.stringify({ protocol_version: "2.0", ...command })));
      socket.addEventListener("message", event => {
        try {
          const frame = JSON.parse(event.data);
          frames.push(frame);
          if (frame.type === "protocol_error") throw new Error(JSON.stringify(frame));
          if (frame.type === "done") {
            assert.equal(frame.metadata?.status, expectedCode ? "failed" : "completed");
            assert.equal(frame.metadata?.error_code ?? "", expectedCode);
            if (!untilSeq) resolve();
          }
          // 重放还包含 done 后的阶段结束和会话信息，等到已知持久化序列的末尾。
          if (untilSeq && frame.seq >= untilSeq) resolve();
        } catch (error) { reject(error); }
      });
    });
    if (!expectedCode) assert(!frames.some(frame => frame.type === "error"), "成功回合含错误事件");
    return frames;
  } finally {
    clearTimeout(timer);
    socket.close();
  }
}

async function chat(mode, expectedCode = "") {
  const frames = await socketCommand({ type: "start_turn", protocol_version: "2.0",
    client_submission_id: randomUUID(), content: mode, capability: "chat", enabled_tools: [] }, expectedCode);
  const turnId = frames.find(frame => frame.turn_id)?.turn_id;
  assert(turnId, "回合缺少 ID");
  for (let attempt = 0; attempt < 100; attempt++) {
    const row = database.prepare("SELECT * FROM turns WHERE id=?").get(turnId);
    if (row && !["queued", "running", "waiting_input"].includes(row.status)) {
      assert.equal(row.error_code, expectedCode);
      assert.equal(row.status, expectedCode ? "failed" : "completed");
      return { turnId, sessionId: row.session_id, frames };
    }
    await delay(50);
  }
  throw new Error("终态没有持久化");
}

function storedEvents(turnId) {
  return database.prepare("SELECT seq,event_json FROM turn_events WHERE turn_id=? ORDER BY seq")
    .all(turnId).map((row, index) => {
      assert.equal(row.seq, index + 1, "事件序号重复或丢失");
      const event = JSON.parse(row.event_json);
      assert.equal(event.seq, row.seq);
      return event;
    });
}

function assistantText(turn) {
  return database.prepare("SELECT content FROM messages WHERE session_id=? AND role='assistant' ORDER BY id DESC LIMIT 1")
    .get(turn.sessionId)?.content;
}

try {
  await start();
  await save();
  database = new DatabaseSync(path.join(root, "data", "deeptutor.sqlite3"));
  database.exec("PRAGMA busy_timeout=10000;");
  const toolTurn = await chat("工具上限");
  assert.equal(assistantText(toolTurn), "工具上限后的最终回答");
  const toolRequests = records.filter(record => record.mode === "工具上限");
  assert.equal(toolRequests.length, 8);
  assert.equal(toolRequests.at(-1).tools, false);
  checks.push("默认 7 轮工具调用后发送一次不带工具的最终回答请求，正文持久化成功");
  await chat("工具违约", "tool_round_limit");
  assert.equal(records.filter(record => record.mode === "工具违约").length, 8);
  checks.push("达到上限后仍返回工具调用时明确失败，不继续执行或误报空响应");
  for (const [mode, expected] of [["完整JSON", "完整中文答案"], ["数组与末帧", "中文末帧答案"]]) {
    const turn = await chat(mode);
    assert.equal(assistantText(turn), expected);
    assert.equal(turn.frames.filter(frame => frame.type === "content").map(frame => frame.content).join(""), expected);
  }
  checks.push("兼容完整 JSON、数组正文、多行 SSE、无换行末帧和最终快照，中文不丢失不重复");
  console.log("开始最终 EXE 并发长流测试：4 个回合，各 4000 条推理及 64 条正文，同时保存设置。");
  const concurrent = Array.from({ length: 4 }, (_, index) => chat(`并发长流-${index}`));
  const settings = (async () => {
    for (let index = 0; index < 30; index++) { await save(); await delay(3); }
  })();
  const lockWait = (async () => {
    await delay(100);
    database.exec("BEGIN IMMEDIATE; UPDATE settings SET updated_at=updated_at WHERE key='catalog';");
    try { await delay(300); } finally { database.exec("COMMIT;"); }
  })();
  const [turns] = await Promise.all([Promise.all(concurrent), settings, lockWait]);
  for (const turn of turns) {
    const events = storedEvents(turn.turnId);
    assert.equal(events.filter(event => event.type === "thinking").length, thinkingChunks);
    assert.equal(events.filter(event => event.type === "content").length, contentChunks);
    assert.equal(assistantText(turn), streamText);
    assert.equal(events.length, database.prepare("SELECT last_seq FROM turns WHERE id=?").get(turn.turnId).last_seq);
  }
  checks.push("4 个并发回合落库 16000 条推理及 256 条正文，30 次设置应用、外部写锁等待均成功，事件连续且没有丢失");
  await chat("真正空响应", "empty_response");
  checks.push("真正空响应保留明确错误，不使用伪造答案掩盖故障");
  database.exec("CREATE TRIGGER reject_content BEFORE INSERT ON turn_events WHEN json_extract(NEW.event_json,'$.type')='content' BEGIN SELECT RAISE(ABORT,'验收注入：落库失败'); END;");
  let failed;
  try { failed = await chat("落库失败", "event_persist_failed"); }
  finally { database.exec("DROP TRIGGER reject_content;"); }
  assert(!storedEvents(failed.turnId).some(event => event.type === "result"));
  checks.push("故障注入证明落库失败保留原始错误，不标为成功也不产生成功结果");
  const beforeRestart = storedEvents(turns[0].turnId);
  await stop();
  await start();
  assert.deepEqual(await api("/settings/catalog", undefined, "GET"), catalog);
  const replay = await socketCommand(
    { type: "subscribe_turn", turn_id: turns[0].turnId, after_seq: 0 },
    "",
    beforeRestart.at(-1).seq,
  );
  assert.deepEqual(replay.filter(frame => frame.seq), beforeRestart, "重启重放与持久化事件不一致");
  assert.equal(assistantText(await chat("重启后继续聊天")), "重启与落库验证答案");
  assert.equal(database.prepare("PRAGMA integrity_check").get().integrity_check, "ok");
  checks.push("重启后模型配置、聊天和事件完整保留，WebSocket 逐条重放一致，数据库完整性检查通过");
  const binarySha256 = createHash("sha256").update(await readFile(path.join(root, "DeepTutor.exe"))).digest("hex");
  const report = { passed: true, binarySha256, root, checks,
    turns: database.prepare("SELECT status,error_code,COUNT(*) AS count FROM turns GROUP BY status,error_code").all(),
    eventCount: database.prepare("SELECT COUNT(*) AS count FROM turn_events").get().count, mockRequests: records.length };
  await writeFile(path.join(output, "stream-runtime-report.json"), JSON.stringify(report, null, 2) + "\n", "utf8");
  console.log(JSON.stringify(report, null, 2));
} finally {
  database?.close();
  await stop();
  mock.closeAllConnections();
  await new Promise(resolve => mock.close(resolve));
}
