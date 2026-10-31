# PDF 阅读导入：Python 与 Rust 实现对照

## 原 Python 实现

阅读材料走独立链路，而不是知识库的通用文本索引链路：

1. `deeptutor/api/routers/reading.py` 的 `upload_material` 接收文件。
2. `deeptutor/reading/store.py` 的 `ReadingStore.ingest` 保存原件、提取结果和材料清单。
3. `deeptutor/reading/extract.py` 的 `_extract_pdf` 使用 PyMuPDF，按物理页读取文本。
4. `_pdf_outline` 只使用文件内的真实书签；没有书签的 PDF 不生成虚构目录。
5. `_pdf_pages_with_image_markers` 和 `deeptutor/utils/document_images.py` 的
   `extract_pdf_images` 抽取页内图片，在对应页的文本末尾添加图片标记。
6. 页面图片的失败不能影响已有文本的导入；原 PDF 始终用于原文显示。

原阅读链路没有自动 OCR。扫描页可能通过图片标记保留下来，但这不等于已经识别
出了正文。如果所有页面既没有正文也没有可用图片标记，原链路仍会报需要 OCR。
MinerU 等通用解析引擎与这条按页阅读链路应当区分。

## Rust 修复

`src-tauri/src/server/pdf_text.rs` 实现纯 Rust 原生解析：

- 用 `pdf-extract` 读取已有文本层，保留空页和物理页号。
- 无文本页用 `pdf-render` 本地渲染，再通过 Windows WinRT OCR 识别。
- Windows OCR 依赖系统已安装的识别语言；不下载模型，不调用外部服务，不启动
  Python、Node、PowerShell 或其他子进程。
- OCR 失败或系统不支持 OCR 时，仍保留 PDF 原件供阅读，并明确返回识别状态和提示。
- 阅读图片采用整页渲染图，保留扫描文字、矢量图、公式与页码之间的关系，
  而不是只取 PDF 内嵌的栅格图片。图片处理属于尽力完成的辅助步骤。
- 使用真实 PDF 书签，包括命名目标，过滤非法页码，并限制书签链遍历。
- 原件不改写；OCR 文字单独存储，因此不会给原扫描 PDF 注入可选择的文本层。

`reading.rs` 在进入通用解析器前处理 PDF，直接消费每页 OCR 结果，避免再次按空
文本层提取而覆盖识别结果。没有书签时，详情接口也不再补造目录。重新处理材料
会同步更新页数、字符数、识别状态和原文显示模式。

通用知识库索引仍然拒绝没有可索引正文的文件，图片标记不能冒充识别文字。
原生通用解析和阅读 OCR 在阻塞任务线程执行，避免阻塞异步 HTTP 工作线程。

## 存储与边界

- 原始 PDF：`data/reading/materials/<id>/raw/`
- 页图：`data/reading/materials/<id>/media/`
- 每页文字、书签、页图映射：`data/reading/materials/<id>/units.json`
- 材料识别状态和统计：安装目录内 SQLite
- 通用解析缓存：`cache/parse/`，带原生 PDF 算法版本，防止复用旧结果

阅读 PDF 处理并发数为 1，限制同时渲染占用的内存。单文件最多处理 2000 页、
800 万正文字符；整页图片最多生成前 120 页，总容量不超过 60 MB。
图片长边最多 1600 像素，OCR 输入长边最多 2200 像素。
超过图片预算不影响原 PDF 的完整显示。

## 开发验收

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --locked --lib
npm --prefix web run build:desktop
.\scripts\build_portable.ps1 -SkipFrontend -StageName DeepTutor-pdf-fixed
node scripts/verify_pdf_import.mjs
node scripts/verify_pdf_import.mjs --pdf "D:\documents\scan.pdf"
```

`verify_pdf_import.mjs` 从干净的发行目录复制 EXE 到隔离目录，生成文本、扫描、
混合和损坏样本，验证原件哈希、范围读取、物理页号、OCR、页图、去重、重试、
处理期间服务响应和重启持久化，再用 Edge 实际渲染 EXE 提供的 PDF 阅读页面。
可重复使用 `--pdf` 增加本地只读样本；不复制用户的真实数据库或密钥。
验收工具所用 Node、Windows 绘图库、Edge 和 `pdfinfo` 都是开发工具，不进入发行包。

界面验收检查 PDF 画布确实包含正文像素、无脚本异常且资源和 API 请求成功。
现有桌面 SPA 的导航 HTML 回退响应可能带有 404 状态，但仍由壳页面接管路由；
该导航状态单独属于路由服务的行为，不作为 PDF 资源加载失败处理。

当原便携目录的 EXE 正在运行时，可用 `-StageName` 输出到独立目录，保留正在使用的
程序和数据。干净的发行目录仍为 `artifacts/portable-package/DeepTutor`。
