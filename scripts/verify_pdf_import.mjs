import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { createRequire } from "node:module";
import { cp, mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

// 仅开发验收使用 Node 和 Windows 绘图库，发行客户端不依赖这些工具。
const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const option = (name, fallback) => args.includes(name) ? args[args.indexOf(name) + 1] : fallback;
const stage = path.resolve(option("--stage", path.join(project, "artifacts/portable-package/DeepTutor")));
const output = path.join(project, "artifacts/pdf-verification", `exe-${Date.now()}`);
const root = path.join(output, "DeepTutor");
const originals = args.flatMap((value, index) => value === "--pdf" ? [path.resolve(args[index + 1])] : []);
const snapshots = [];
const checks = [];
let child;
let serviceUrl;
await mkdir(output, { recursive: true });
await cp(stage, root, { recursive: true });

const digest = data => createHash("sha256").update(data).digest("hex");

function makePdf(kinds, image) {
  const objects = [null, null, Buffer.from("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")];
  const add = value => (objects.push(Buffer.isBuffer(value) ? value : Buffer.from(value)), objects.length);
  const stream = (header, bytes) => Buffer.concat([
    Buffer.from(`<< ${header} /Length ${bytes.length} >>\nstream\n`), bytes, Buffer.from("\nendstream"),
  ]);
  const imageId = image ? add(stream("/Type /XObject /Subtype /Image /Width 1600 /Height 1000 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode", image)) : null;
  const pages = kinds.map((kind, index) => {
    const commands = kind === "scan"
      ? "q 400 0 0 250 0 0 cm /Scan Do Q"
      : kind === "blank" ? "" : `BT /F1 18 Tf 20 180 Td (Physical page ${index + 1}: text layer) Tj ET`;
    const content = add(stream("", Buffer.from(commands)));
    return add(`<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 250] /Resources << /Font << /F1 3 0 R >> ${imageId ? `/XObject << /Scan ${imageId} 0 R >>` : ""} >> /Contents ${content} 0 R >>`);
  });
  objects[0] = Buffer.from("<< /Type /Catalog /Pages 2 0 R >>");
  objects[1] = Buffer.from(`<< /Type /Pages /Count ${pages.length} /Kids [${pages.map(id => `${id} 0 R`).join(" ")}] >>`);
  const chunks = [Buffer.from("%PDF-1.5\n")];
  const offsets = [0];
  let size = chunks[0].length;
  objects.forEach((object, index) => {
    offsets.push(size);
    const data = Buffer.concat([Buffer.from(`${index + 1} 0 obj\n`), object, Buffer.from("\nendobj\n")]);
    chunks.push(data);
    size += data.length;
  });
  chunks.push(Buffer.from(`xref\n0 ${objects.length + 1}\n0000000000 65535 f \n${offsets.slice(1).map(offset => `${String(offset).padStart(10, "0")} 00000 n \n`).join("")}trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${size}\n%%EOF\n`));
  return Buffer.concat(chunks);
}

function makeScanImage() {
  const destination = path.join(output, "scan-fixture.jpg");
  const code = `
Add-Type -AssemblyName System.Drawing
$bitmap = New-Object System.Drawing.Bitmap 1600, 1000
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.Clear([System.Drawing.Color]::White)
$graphics.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
$font = New-Object System.Drawing.Font 'Arial', 64
$graphics.DrawString('SCANNED PDF TEST 2026', $font, [System.Drawing.Brushes]::Black, 65, 120)
$graphics.DrawString('DIGITAL SIGNAL PROCESSING', $font, [System.Drawing.Brushes]::Black, 65, 280)
$graphics.DrawString('Local OCR works offline', $font, [System.Drawing.Brushes]::Black, 65, 440)
$bitmap.Save($env:PDF_FIXTURE_OUTPUT, [System.Drawing.Imaging.ImageFormat]::Jpeg)
$font.Dispose()
$graphics.Dispose()
$bitmap.Dispose()
`;
  const result = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", code], {
    encoding: "utf8", windowsHide: true, env: { ...process.env, PDF_FIXTURE_OUTPUT: destination },
  });
  assert.equal(result.status, 0, `无法创建扫描验收样本：${result.stderr}`);
  return destination;
}

async function start() {
  child = spawn(path.join(root, "DeepTutor.exe"), [], {
    cwd: root, windowsHide: true, stdio: "ignore",
    env: { ...process.env, DEEPTUTOR_PORTABLE_ROOT: root, RUST_LOG: "deeptutor_desktop_lib=info,tauri=warn" },
  });
  let launchError;
  child.on("error", error => { launchError = error; });
  for (let attempt = 0; attempt < 160; attempt++) {
    if (launchError) throw launchError;
    assert.equal(child.exitCode, null, "验收 EXE 提前退出");
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
  throw new Error("验收 EXE 未在 40 秒内就绪");
}

async function stop() {
  if (!child || child.exitCode !== null) return;
  const exited = once(child, "exit");
  child.kill();
  await exited;
}

async function api(route, options = {}) {
  return fetch(`${serviceUrl}/api${route}`, { signal: AbortSignal.timeout(300_000), ...options });
}

async function json(route, options) {
  const response = await api(route, options);
  assert(response.ok, `${route}: HTTP ${response.status} ${await response.clone().text()}`);
  return response.json();
}

async function upload(filename, bytes) {
  const form = new FormData();
  form.append("file", new Blob([bytes], { type: "application/pdf" }), filename);
  return json("/reading/materials", { method: "POST", body: form });
}

async function verifyMaterial(filename, bytes, expectedPages, requireOcr = false) {
  const started = Date.now();
  let finished = false;
  const pending = upload(filename, bytes).finally(() => { finished = true; });
  let healthChecks = 0;
  while (!finished) {
    const response = await fetch(`${serviceUrl}/health`, { signal: AbortSignal.timeout(5000) });
    assert(response.ok, "PDF 处理期间本地服务无响应");
    healthChecks++;
    await Promise.race([pending, delay(500)]);
  }
  const material = await pending;
  const route = `/reading/materials/${material.material_id}`;
  assert.equal(material.unit, "page", "PDF 没有按物理页定位");
  assert.equal(material.unit_count, expectedPages, "PDF 页数改变");
  assert.equal(material.has_raw_view, true);
  assert.equal(material.render_mode, "pdf");
  assert.deepEqual(material.outline, [], "无书签 PDF 被编造目录");
  assert.equal(digest(Buffer.from(await (await api(`${route}/raw`)).arrayBuffer())), digest(bytes), "原件被修改");
  const range = await api(`${route}/raw`, { headers: { Range: "bytes=0-31" } });
  assert.equal(range.status, 206, "PDF 原件不支持范围读取");
  assert.equal((await range.arrayBuffer()).byteLength, 32);
  const units = await Promise.all(Array.from({ length: expectedPages }, (_, index) => json(`${route}/units/${index + 1}`)));
  const media = await json(`${route}/media`);
  assert.equal(media.length, Math.min(expectedPages, 120), "页面预览数量错误");
  for (const item of [media[0], media.at(-1)]) {
    assert(item.locator >= 1 && item.locator <= expectedPages);
    const response = await api(`${route}/media/${item.name}`);
    assert(response.ok && response.headers.get("content-type").startsWith("image/"));
    assert((await response.arrayBuffer()).byteLength > 50, "图片内容为空");
    assert(units[item.locator - 1].text.includes(item.name), "图片标记与物理页不一致");
  }
  if (requireOcr) {
    assert(material.ocr_page_count > 0, `扫描页没有完成 OCR：${material.ocr_warning}`);
    assert(material.char_count > 50, "OCR 没有产生真实可用文字");
  }
  const duplicate = await upload(filename, bytes);
  assert.equal(duplicate.material_id, material.material_id, "重复文件未复用");
  snapshots.push({ route, material, units });
  checks.push({
    filename, pages: expectedPages, ocrPages: material.ocr_page_count,
    chars: material.char_count, textStatus: material.text_status,
    media: media.length, seconds: Number(((Date.now() - started) / 1000).toFixed(2)),
    healthChecks, rawUnchanged: true, duplicateReused: true,
  });
  console.log(`通过：${filename}，${expectedPages} 页，OCR ${material.ocr_page_count} 页，${material.char_count} 字符`);
  return units;
}

async function verifyReaderUi() {
  const require = createRequire(path.join(project, "web/package.json"));
  const { chromium } = require("playwright");
  const browser = await chromium.launch({ channel: "msedge", headless: true });
  try {
    const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
    const page = await context.newPage();
    const errors = [];
    const failures = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("response", response => {
      // 桌面 SPA 的壳页面可能沿用回退响应状态，单独验证实际渲染；资源和 API 必须成功。
      if (response.request().resourceType() !== "document"
        && response.url().startsWith(serviceUrl) && response.status() >= 400) {
        failures.push({ url: response.url(), status: response.status() });
      }
    });
    // 覆盖纯文本、扫描件以及真实文件，实际加载 EXE 提供的静态页面与 PDF.js。
    for (const sample of snapshots) {
      const { workspace } = await json("/reading/workspaces", {
        method: "POST", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ title: `PDF 验收 ${sample.material.filename}`, material_ids: [sample.material.material_id] }),
      });
      await page.goto(serviceUrl);
      await page.waitForFunction(() => document.getElementById("root")?.childElementCount > 0);
      // 按桌面客户端的前端路由方式进入资料集。
      await page.evaluate(target => {
        window.history.pushState(null, "", target);
        window.dispatchEvent(new PopStateEvent("popstate"));
      }, `/learning/reading/${workspace.workspace_id}`);
      await page.locator('[data-reader-unit="1"] canvas').first().waitFor({ state: "visible", timeout: 60_000 });
      await page.waitForFunction(() => {
        const canvas = document.querySelector('[data-reader-unit="1"] canvas');
        if (!canvas || canvas.width <= 100 || canvas.height <= 100) return false;
        const pixels = canvas.getContext("2d").getImageData(0, 0, canvas.width, canvas.height).data;
        let dark = 0;
        let light = 0;
        for (let index = 0; index < pixels.length; index += 16) {
          if (pixels[index + 3] < 200) continue;
          const sum = pixels[index] + pixels[index + 1] + pixels[index + 2];
          if (sum < 600) dark++;
          if (sum > 740) light++;
        }
        return dark > 30 && light > 100;
      }, undefined, { timeout: 60_000 });
      await page.screenshot({ path: path.join(output, `reader-${sample.material.material_id}.png`), fullPage: true });
    }
    assert.deepEqual(errors, [], "阅读界面发生脚本异常");
    assert.deepEqual(failures, [], "阅读界面资源或接口加载失败");
    checks.push({ case: "桌面发行页面实际渲染文本与扫描 PDF", passed: true });
  } finally {
    await browser.close();
  }
}

try {
  const image = await readFile(makeScanImage());
  await start();
  const text = await verifyMaterial("文本与空页.pdf", makePdf(["text", "blank", "text"]), 3);
  assert(text[0].text.includes("Physical page 1"));
  assert(text[2].text.includes("Physical page 3"), "空白页导致定位偏移");
  const scanned = await verifyMaterial("扫描件.pdf", makePdf(["scan"], image), 1, true);
  assert(/SCANNED|SIGNAL|PROCESSING/i.test(scanned[0].text), "识别文字与扫描原图不符");
  const mixed = await verifyMaterial("混合文档.pdf", makePdf(["text", "scan", "blank"], image), 3, true);
  assert(mixed[0].text.includes("Physical page 1"));
  assert(/SCANNED|SIGNAL|PROCESSING/i.test(mixed[1].text), "扫描页 OCR 被原文本提取覆盖");
  const badForm = new FormData();
  badForm.append("file", new Blob(["not a valid PDF"]), "损坏.pdf");
  assert.equal((await api("/reading/materials", { method: "POST", body: badForm })).status, 400);
  checks.push({ case: "损坏 PDF 返回可读错误", passed: true });
  const inputs = [];
  for (const filename of originals) {
    const bytes = await readFile(filename);
    const info = spawnSync("pdfinfo", [filename], { encoding: "utf8", windowsHide: true });
    assert.equal(info.status, 0, "无法只读核对真实 PDF 页数");
    const pages = Number(info.stdout.match(/^Pages:\s+(\d+)/m)?.[1]);
    assert(pages > 0);
    inputs.push({ filename, hash: digest(bytes) });
    await verifyMaterial(path.basename(filename), bytes, pages, true);
  }
  const sample = snapshots[0];
  await json(`${sample.route}/retry`, { method: "POST" });
  for (let attempt = 0; attempt < 120; attempt++) {
    const material = await json(sample.route);
    if (material.status === "ready") break;
    assert.notEqual(material.status, "failed", material.error_detail);
    assert(attempt < 119, "重试处理超时");
    await delay(250);
  }
  checks.push({ case: "重新处理后保持页数与原文模式", passed: true });
  await stop();
  await start();
  for (const sample of snapshots) {
    const material = await json(sample.route);
    assert.equal(material.unit_count, sample.material.unit_count);
    assert.equal(material.char_count, sample.material.char_count);
    assert.equal(material.text_status, sample.material.text_status);
    assert.deepEqual(material.outline, []);
    const last = await json(`${sample.route}/units/${sample.units.length}`);
    assert.equal(last.text, sample.units.at(-1).text, "重启后提取内容丢失");
  }
  checks.push({ case: "重启后原件与 OCR 结果持久化", passed: true });
  if (!args.includes("--skip-ui")) await verifyReaderUi();
  for (const input of inputs) assert.equal(digest(await readFile(input.filename)), input.hash, "用户源文件被修改");
  const report = {
    executable: path.join(root, "DeepTutor.exe"),
    executableSha256: digest(await readFile(path.join(root, "DeepTutor.exe"))),
    portableRoot: root, checks, passed: true,
  };
  await writeFile(path.join(output, "report.json"), JSON.stringify(report, null, 2) + "\n");
  console.log(`全部通过，验收报告：${path.join(output, "report.json")}`);
} finally {
  await stop();
}
