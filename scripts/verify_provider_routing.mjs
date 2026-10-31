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

// 仅开发验收使用 Node；被测客户端与发行包仍为独立 Rust 程序。
const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const stage = path.join(project, "artifacts", "portable-package", "DeepTutor");
const output = path.join(project, "artifacts", "provider-verification");
const root = path.join(output, `exe-${Date.now()}`);
const args = process.argv.slice(2);
const sourceIndex = args.indexOf("--source-db");
const original = sourceIndex < 0
  ? JSON.parse(await readFile(path.join(project, "src-tauri/tests/fixtures/referenced-catalog.json"), "utf8"))
  : readCatalog(path.resolve(args[sourceIndex + 1]));
const pick = (value, keys) => Object.fromEntries(
  keys.filter(key => value?.[key] !== undefined).map(key => [key, structuredClone(value[key])]),
);
const safeRef = value => value && pick(value, ["connection_id", "service", "profile_id", "binding", "default_base_url"]);
// 真实数据库只读，配置白名单复制，真实凭据和请求头均不会进入验收实例。
const catalog = {
  version: 1,
  connections: (original.connections ?? []).map(connection => ({
    ...pick(connection, ["id", "provider", "source_service", "api_format", "wire_api"]),
    name: "验收网关", api_key: "exe-test-key", extra_headers: { "X-Portable-Test": "1" },
  })),
  services: Object.fromEntries(["llm", "embedding"].map(service => {
    const bucket = original.services?.[service] ?? {};
    return [service, {
      ...pick(bucket, ["active_profile_id", "active_model_id"]),
      profiles: (bucket.profiles ?? []).map(profile => ({
        ...pick(profile, ["id", "binding", "provider", "connection_id", "api_format", "wire_api"]),
        name: "验收模型", base_url: "https://api.openai.com/v1", api_key: "stale-key",
        ...(profile.provider_ref ? { provider_ref: safeRef(profile.provider_ref) } : {}),
        models: (profile.models ?? []).map(model => ({
          ...pick(model, ["id", "model", "name"]),
          ...(model.provider_ref ? { provider_ref: safeRef(model.provider_ref) } : {}),
        })),
      })),
    }];
  })),
};
assert(catalog.connections.length && catalog.services.llm.profiles[0]?.models[0], "缺少可验收的提供方或语言模型");
const records = [];
const checks = [];
const mock = createServer(async (request, response) => {
  try {
    let raw = "";
    for await (const chunk of request) raw += chunk;
    const body = raw ? JSON.parse(raw) : {};
    records.push({
      path: request.url, method: request.method, model: body.model,
      credentialMatches: request.headers.authorization === "Bearer exe-test-key"
        || request.headers["x-api-key"] === "exe-test-key",
      extraHeaderMatches: request.headers["x-portable-test"] === "1",
    });
    if (request.url.endsWith("/chat/completions")) {
      response.writeHead(200, { "Content-Type": "text/event-stream" });
      response.end('data: {"choices":[{"delta":{"content":"路由校验成功"}}]}\n\ndata: [DONE]\n\n');
    } else if (request.url.endsWith("/responses")) {
      response.writeHead(200, { "Content-Type": "text/event-stream" });
      response.end('data: {"type":"response.output_text.delta","delta":"路由校验成功"}\n\ndata: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}\n\n');
    } else if (request.url.endsWith("/messages")) {
      response.writeHead(200, { "Content-Type": "text/event-stream" });
      response.end('data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"路由校验成功"}}\n\ndata: {"type":"message_stop"}\n\n');
    } else if (request.url.endsWith("/embeddings")) {
      response.writeHead(200, { "Content-Type": "application/json" });
      response.end(JSON.stringify({ data: body.input.map((_, index) => ({ index, embedding: [0.1, 0.2, 0.3] })) }));
    } else if (request.url.endsWith("/audio/speech")) {
      response.writeHead(200, { "Content-Type": "audio/mpeg" });
      response.end(Buffer.from("ID3-local-routing-test"));
    } else {
      response.writeHead(404);
      response.end();
    }
  } catch {
    response.writeHead(500);
    response.end();
  }
});
await new Promise(resolve => mock.listen(0, "127.0.0.1", resolve));
const mockBase = `http://127.0.0.1:${mock.address().port}`;
for (const connection of catalog.connections) connection.base_url = `${mockBase}/api/v3`;
await mkdir(output, { recursive: true });
await cp(stage, root, { recursive: true });
let child;
let serviceUrl;

function readCatalog(databasePath) {
  const database = new DatabaseSync(databasePath, { readOnly: true });
  try {
    const row = database.prepare("SELECT value_json FROM settings WHERE key='catalog'").get();
    assert(row, "数据库没有已应用的模型目录");
    return JSON.parse(row.value_json);
  } finally {
    database.close();
  }
}

async function start() {
  child = spawn(path.join(root, "DeepTutor.exe"), [], {
    cwd: root, windowsHide: true, stdio: "ignore",
    env: {
      ...process.env, DEEPTUTOR_PORTABLE_ROOT: root,
      RUST_LOG: "deeptutor_desktop_lib=info,tauri=warn",
    },
  });
  let launchError;
  child.on("error", error => { launchError = error; });
  for (let attempt = 0; attempt < 120; attempt++) {
    if (launchError) throw launchError;
    assert(child.exitCode === null, "EXE 提前退出");
    const log = await readFile(path.join(root, "logs/desktop.log"), "utf8").catch(() => "");
    const matches = [...log.matchAll(/(?:^|\s)url=(http:\/\/127\.0\.0\.1:\d+)/g)];
    if (matches.length) {
      serviceUrl = matches.at(-1)[1];
      try {
        const response = await fetch(`${serviceUrl}/health`, { signal: AbortSignal.timeout(1000) });
        if (response.ok) return;
      } catch { /* 等待 Rust 本地服务就绪。 */ }
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
    method, signal: AbortSignal.timeout(15000),
    ...(body === undefined ? {} : {
      headers: { "Content-Type": "application/json" }, body: JSON.stringify(body),
    }),
  });
  assert(response.ok, `${route} 返回 HTTP ${response.status}`);
  return response.json();
}

async function save(value) {
  await api("/settings/draft", { catalog: value, extensions: {} }, "PUT");
  const applied = await api("/settings/apply");
  assert.equal(applied.draft, null, "草稿未清除");
  assert.deepEqual(applied.catalog, value, "应用配置改变了提供方引用");
}

async function diagnose(service, value, expectedSuccess = true) {
  const selected = structuredClone(value);
  const bucket = selected.services[service];
  const profile = bucket.profiles[0];
  bucket.active_profile_id = profile.id;
  bucket.active_model_id = profile.models[0].id;
  const { run_id } = await api(`/settings/tests/${service}/start`, { catalog: selected });
  const response = await fetch(`${serviceUrl}/api/settings/tests/${service}/${run_id}/events`, {
    signal: AbortSignal.timeout(15000),
  });
  assert(response.ok, "诊断事件流未建立");
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const events = [];
  let buffer = "";
  try {
    while (true) {
      const chunk = await reader.read();
      assert(!chunk.done, "诊断流未返回终态");
      buffer += decoder.decode(chunk.value, { stream: true });
      const frames = buffer.split("\n\n");
      buffer = frames.pop();
      for (const frame of frames) {
        const data = frame.split("\n").find(line => line.startsWith("data: "));
        if (!data) continue;
        const event = JSON.parse(data.slice(6));
        events.push(event);
        if (["completed", "failed"].includes(event.type)) {
          assert.equal(event.type, expectedSuccess ? "completed" : "failed", event.message);
          return events;
        }
      }
    }
  } finally {
    await reader.cancel();
  }
}

async function chat(expectedSuccess = true) {
  const socket = new WebSocket(serviceUrl.replace("http:", "ws:") + "/ws");
  const frames = [];
  let timer;
  try {
    await new Promise((resolve, reject) => {
      timer = setTimeout(() => reject(new Error("聊天回合在 15 秒内没有结束")), 15000);
      socket.addEventListener("error", () => reject(new Error("聊天 WebSocket 失败")));
      socket.addEventListener("open", () => socket.send(JSON.stringify({
        type: "start_turn", protocol_version: "2.0", content: "请回复路由校验成功",
        capability: "chat", enabled_tools: [],
      })));
      socket.addEventListener("message", event => {
        try {
          const frame = JSON.parse(event.data);
          frames.push(frame);
          if (["error", "protocol_error"].includes(frame.type)) {
            if (expectedSuccess) reject(new Error(frame.message ?? JSON.stringify(frame)));
            else resolve();
          }
          if (frame.type === "done") {
            assert(expectedSuccess, "失效配置竟然建立了模型回合");
            assert.equal(frame.metadata?.status, "completed");
            resolve();
          }
        } catch (error) {
          reject(error);
        }
      });
    });
    if (expectedSuccess) assert(JSON.stringify(frames).includes("路由校验成功"), "聊天结果未包含模拟响应");
  } finally {
    clearTimeout(timer);
    socket.close();
  }
}

function lastRequest(expected) {
  assert.equal(records.at(-1)?.path, expected, "实际请求路径错误");
  assert(records.at(-1).credentialMatches, "没有使用关联提供方的凭据");
  assert(records.at(-1).extraHeaderMatches, "没有使用关联提供方的额外请求头");
}

try {
  await start();
  await save(catalog);
  await chat();
  lastRequest("/api/v3/chat/completions");
  checks.push("原始 provider_ref 结构、空默认选择：保存草稿、应用、真实聊天均命中 /api/v3/chat/completions");
  await diagnose("llm", catalog);
  lastRequest("/api/v3/chat/completions");
  checks.push("设置页语言模型诊断命中关联地址");
  if (catalog.services.embedding.profiles.length) {
    await diagnose("embedding", catalog);
    lastRequest("/api/v3/embeddings");
    checks.push("嵌入模型诊断命中 /api/v3/embeddings");
  }
  const changed = structuredClone(catalog);
  changed.connections.forEach(connection => { connection.base_url = `${mockBase}/changed/api/v3`; });
  await save(changed);
  await chat();
  lastRequest("/changed/api/v3/chat/completions");
  checks.push("修改提供方地址后，原模型立即使用新地址");
  const modelOverride = structuredClone(catalog);
  modelOverride.connections.push({
    ...modelOverride.connections[0], id: "model-override",
    base_url: `${mockBase}/model-override/api/v3`,
  });
  modelOverride.services.llm.profiles[0].models[0].provider_ref = {
    connection_id: "model-override", binding: "custom",
  };
  await save(modelOverride);
  await chat();
  lastRequest("/model-override/api/v3/chat/completions");
  await diagnose("llm", modelOverride);
  lastRequest("/model-override/api/v3/chat/completions");
  checks.push("同一配置中的模型级引用优先于配置级引用，聊天与诊断保持一致");
  for (const [format, endpoint] of [
    ["openai_responses", "/api/v3/responses"],
    ["anthropic", "/api/v3/v1/messages"],
  ]) {
    const variant = structuredClone(catalog);
    variant.connections.forEach(connection => { connection.api_format = format; });
    await save(variant);
    await chat();
    lastRequest(endpoint);
    await diagnose("llm", variant);
    lastRequest(endpoint);
    checks.push(`${format} 协议使用关联地址`);
  }
  const voice = structuredClone(catalog);
  voice.services.tts = {
    active_profile_id: "tts-profile", active_model_id: "tts-model",
    profiles: [{
      id: "tts-profile", binding: "custom", base_url: "https://api.openai.com/v1",
      provider_ref: { connection_id: voice.connections[0].id, binding: "custom" },
      models: [{ id: "tts-model", model: "mock-tts", voice: "alloy", response_format: "mp3" }],
    }],
  };
  await save(voice);
  const audio = await fetch(`${serviceUrl}/api/voice/tts`, {
    method: "POST", signal: AbortSignal.timeout(15000),
    headers: { "Content-Type": "application/json" }, body: JSON.stringify({ text: "朗读路由测试" }),
  });
  assert(audio.ok, "语音合成接口失败");
  assert((await audio.arrayBuffer()).byteLength > 0, "语音结果为空");
  lastRequest("/api/v3/audio/speech");
  checks.push("TTS 运行入口命中关联地址 /api/v3/audio/speech");
  const invalid = structuredClone(catalog);
  invalid.connections = [];
  await save(invalid);
  const count = records.length;
  await chat(false);
  await diagnose("llm", invalid, false);
  assert.equal(records.length, count, "失效引用发送了请求");
  const empty = structuredClone(catalog);
  empty.connections.forEach(connection => { connection.base_url = ""; });
  for (const bucket of Object.values(empty.services))
    for (const profile of bucket.profiles) {
      if (profile.provider_ref) profile.provider_ref.default_base_url = "";
      for (const model of profile.models)
        if (model.provider_ref) model.provider_ref.default_base_url = "";
    }
  await save(empty);
  await chat(false);
  await diagnose("llm", empty, false);
  assert.equal(records.length, count, "空地址发送了请求");
  checks.push("提供方删除或自定义地址为空时，诊断与聊天均在发送前拒绝");
  await save(catalog);
  await stop();
  await start();
  const saved = await api("/settings/catalog", undefined, "GET");
  assert.deepEqual(saved, catalog, "重启后提供方引用改变");
  await chat();
  lastRequest("/api/v3/chat/completions");
  checks.push("最终 EXE 重启后保持配置并继续使用关联地址");
  assert(records.every(record => record.credentialMatches && record.extraHeaderMatches), "出现未预期的网关请求");
  const binarySha256 = createHash("sha256")
    .update(await readFile(path.join(root, "DeepTutor.exe"))).digest("hex");
  const report = {
    passed: true, binarySha256,
    source: sourceIndex < 0 ? "脱敏契约样本" : "真实便携数据库的白名单脱敏副本",
    checks, requests: records,
  };
  await writeFile(path.join(output, "provider-routing-report.json"), JSON.stringify(report, null, 2) + "\n", "utf8");
  console.log(JSON.stringify(report, null, 2));
} finally {
  await stop();
  mock.closeAllConnections();
  await new Promise(resolve => mock.close(resolve));
}
