# DeepTutor 工作日志

## 2026-10-29

- 从最新稳定标签 `v1.7.0` 创建分支 `codex/tauri-rust-port`。
- 确认仓库工作区干净；当前 Python 能力层约 28 万行，包含 25 个官方模块、CLI、WebSocket API、Python SDK 与 Next.js 前端。
- 确定迁移策略：保留完整能力引擎，以 Tauri 2 + Rust 作为桌面后端、进程监督器和 HTTP/WebSocket 统一网关，避免重写造成功能缺失。
- 开始实现便携式安装目录数据布局、缓存重定向、运行时打包与桌面启动链路。

## 2026-10-31

- 根据用户澄清修正技术路线：桌面端改为不绑定 Python 的纯 Rust/Tauri 独立客户端，Node.js 仅在构建期生成 Vite 静态资源。
- 已在 `codex/tauri-rust-port` 分支完成 Rust Axum API、WebSocket、回合引擎、认证、存储、工具、能力、文件、更新、定时任务与桌面生命周期迁移。
- 新增无宿主能力 WASM 插件运行时，支持 `wasm:` 路由并拒绝 Python wheel 插件；请求、响应、内存、燃料与执行时间均有限制。
- 所有正式运行状态与 WebView2 缓存指向安装目录；便携构建禁止包含 Python、Node、Next.js、Remotion、旧运行时和用户数据。
- 代码当前处于开发验证阶段；完整测试、OpenAPI 审计和便携包构建结果将在本轮完成后记录。

## 2026-10-31 01:27:57 +08:00
- 分支：codex/tauri-rust-port（基于 v1.7.0 / f9a68757）。
- Rust 验证：cargo check --locked 通过；cargo test --locked 通过（118 passed, 0 failed）。
- 新增回归：插件 WASM Tool、Capability、loop_capability、Reading Extension、Visualizer manifest、便携路径与篡改包保护。
- 前端验证：npm --prefix web run build:desktop 通过。
- OpenAPI 审计：695 个契约操作，710 个 Rust 操作，缺失契约 0，额外 Rust 操作 30，脚本退出码 0。
- 便携构建：artifacts/DeepTutor-1.7.0-portable-X64.zip；SHA-256 300FB03E224946B5963D47CDA611BFE688C1F0CDA965A735BAC90234C5D28DFA。
- ZIP 审计：927 个文件；Python/Node 运行时、node_modules、.py/.pyc/.pyd、data/cache/temp/logs/home 均未进入发行包。
- 代码可用状态：可用于纯 Rust Tauri 便携桌面构建；外部 MCP、CLI、解析器和模型服务仍按用户配置启用。

## 2026-10-31 01:45:13 +08:00

- 最终 Rust 验证：`cargo check --locked` 与 `cargo test --locked` 均通过；测试结果 118 passed、0 failed。
- 最终前端验证：`build:desktop`、`typecheck` 通过；`test:unit` 为 197 files / 998 tests 通过；`test:node` 为 1306 / 1306 通过；`lint` 为 0 errors、159 warnings。
- OpenAPI 契约审计通过：695 个契约操作、710 个 Rust 操作、缺失契约 0、额外 Rust 操作 30，脚本退出码 0。
- 最新前端构建完成后重新执行便携打包，生成 `artifacts/DeepTutor-1.7.0-portable-X64.zip`。
- 最终目录与 ZIP 各包含 927 个文件；Python/Node 运行时、`node_modules`、`.py/.pyc/.pyd`、`runtime/python`、`runtime/node`、用户 `data/cache/temp/logs/home` 禁入项均为 0，审计通过。
- 最终 ZIP SHA-256：`F23726558DDFF1A03A052714CB8627F9CD308B9302A31C9B03E8104D850C4577`。上一条日志中的哈希对应旧前端构建产物，已由本条最终值替代。
- 代码可用状态：纯 Rust/Tauri 便携桌面构建可用于验收；外部 MCP、CLI、文档解析器和模型服务仍需按用户环境配置后启用。

## 2026-10-31 10:07:18 +08:00
- 任务：修复 DeepTutor.exe 缺少 WebView2Loader.dll，并对独立 Tauri/Rust 便携版进行充分校验。
- 分支：codex/tauri-rust-port；基准提交：4c965ac7d7698fa697cf786b0d0a441e08c82e5b。
- 修复：build_portable.ps1 随包复制 WebView2Loader.dll，manifest 记录 native_dependencies，升级说明同步替换 EXE、DLL 与 resources/web。
- 修复：删除 Rust HTTP 路由中的重复内存路由；新增 GET /api/modules 的 25 项纯 Rust 模块清单；根路由改为 /chat；修正日志过滤器。
- 新增：scripts/verify_portable.ps1，覆盖必需文件、PE 架构、EXE/DLL 导入、禁入项、目录/ZIP 一致性、SHA-256、隔离启动、安装目录数据与 /api/modules。
- 测试：cargo check 通过；cargo test 122/122 通过；前端 Node 1307/1307 通过；TypeScript typecheck 通过；PowerShell 语法通过。
- 运行验收：最终 EXE 隔离启动通过；WebView/Rust 服务就绪；SQLite、WebView2 缓存、日志、temp、home 均位于安装目录；/api/modules 返回 25 个且全部 installed/enabled/ready。
- 可视验收：初始地址为 /chat；聊天欢迎页、输入框和能力入口正常；日历、任务看板、笔记本（/co-writer）均可正常渲染，不再出现 404 或仅显示“重试”。
- 发布物：D:\VS_Projects\DeepTutor\artifacts\DeepTutor-1.7.0-portable-X64.zip。
- SHA-256：815702d7c0a67bcd4bba939d64a893e692b238df368b824c516430f162a470f0。
- 路由审计：OpenAPI 695 项，Rust 711 项，缺失 0 项，额外 31 项兼容/扩展路由。
- 代码可用：是。

## 2026-10-31 11:26:22 +08:00
- 任务：修复桌面端点击“学习/资料”后的动态组件与学习索引契约问题，并重新验收最终便携 EXE。
- 修改：桌面 `next/dynamic` 兼容层支持命名组件直接返回；`/api/dashboard/learning-index` 改为 Rust 汇总 `{sources, failed}`，覆盖书籍、精通之路、阅读、观看和练习五类数据并逐类容错。
- 测试：Rust 124/124 通过；前端 1312/1312 通过；TypeScript、ESLint、Vite desktop build、OpenAPI 路由审计通过。
- 产物：`artifacts/DeepTutor-1.7.0-portable-X64.zip`，32,938,236 字节，SHA-256 `3ECDAC0D70B87A3E75B6512D72A1D4E970E7F297E853B031295E338E8576E09E`。
- 发布校验：X64、30 项原生导入、928 个目录/ZIP 文件一致、禁入项 0、WebView/Rust 启动和 25 个模块接口通过。
- 实机校验：“学习”“资料”“全部学习”均正常显示；学习索引五类字段和 `failed` 契约正确；SQLite、cache、temp、home、logs 均写入安装目录，用户目录无 DeepTutor 运行数据。
- 代码可用：是。
[2026-10-31 15:00:04]
- 修复桌面端点击“设置”后主内容空白的问题。
- 根因：React 渲染函数内部反复创建 lazy 组件，导致惰性内容无法稳定提交。
- 同步修正阅读文件夹与观看会话特殊路由的模块级惰性加载和布局组合。
- 验证：TypeScript typecheck、Node 1314/1314、ESLint、desktop Vite build、便携包构建、verify_portable 静态与 SmokeTest 均通过。
- EXE 实测：“常规”“工作区”“外观”设置分页正常，工作区数据路径位于安装目录。
- 代码状态：可用。

## 2026-10-31 19:16:44 +08:00
- 修复模型设置保存：POST /api/settings/apply 接受空请求体，并将服务端保存的 settings_draft.catalog 提升为正式 catalog；响应返回前端需要的 catalog 与 draft。
- 修复便携构建脚本：artifacts\portable\DeepTutor 只更新发行文件，保留 data/cache/temp/logs/home；干净 ZIP 从 artifacts\portable-package\DeepTutor 生成，避免清除用户数据库与模型草稿。
- 修复便携验证脚本：发行载荷审计排除安装目录运行时数据，ZIP 仍严格执行禁入项检查；EXE 冒烟测试只复制发行载荷到全新目录，验证首次启动自行创建安装目录状态。
- 验证结果：cargo fmt/check/test 通过（125 项）；前端 Node 测试通过（1314 项）；OpenAPI 审计无缺失契约操作；ZIP 890 个文件、SHA-256、PE X64 与 30 项导入依赖通过；EXE 启动、Rust API、25 个模块和安装目录数据写入通过。
- GUI 验证：设置页正常显示，语言模型页面恢复 tju/deepseek-v4.1-flash；点击应用后显示“已应用”，无 400 Bad Request；API 确认正式配置包含 1 个连接、1 个语言模型、1 个嵌入模型，草稿为空。
- 代码状态：可用。

## 2026-10-31 21:44:41 - 修复模型连接地址解析并完善页面个性化接口
- 分支：codex/tauri-rust-port。
- 根因：模型配置通过 connection_id 关联提供方时，模型配置保留了旧 base_url；服务诊断和聊天路径未先解析关联连接，导致空地址或旧地址回退到 https://api.openai.com/v1。
- 修复：catalog::service_selection、engine::resolve_llm_settings 和 provider_probe::execute 统一解析连接覆盖后的有效地址、密钥、协议与附加请求头。
- 连带修复：补齐 GET /api/settings/page-customization 聚合接口和静态分段 GET，保存接口统一返回包含默认值的完整配置文档，避免聊天页显示“未能加载页面设置”。
- 回归测试：cargo fmt、cargo check --locked、cargo test --locked 全部通过，129 个测试通过、0 个失败。
- 本地集成验证：模型配置故意保留 api.openai.com 旧地址，连接配置指向本地模拟服务；实际请求命中 /v1/chat/completions，诊断事件中的请求目标为连接地址，测试成功。
- UI 验证：学习、资料、常规设置、语言模型和提供方页面均正常；模型绑定 tju，提供方地址为用户配置的自定义地址，API Key 仅以掩码显示。
- 发行验证：便携 EXE 启动冒烟、25 个模块接口、安装目录数据、PE 架构、30 项 DLL 导入依赖、890 个 ZIP 文件一致性及发行载荷禁入项全部通过。
- 最终包：artifacts/DeepTutor-1.7.0-portable-X64.zip，SHA-256=66055BC1D6BAA3347C95462DFB272FB843BF10E3A7872C605665BFA4611535A3。
- 代码状态：可用于提交；未推送。

## 2026-10-31 22:46:24 +08:00 修复新版提供方引用导致自定义地址失效

- 用户反馈：已设置自定义网关，但模型请求仍访问 OpenAI；本次在 `codex/tauri-rust-port` 分支继续修复。
- 根因：真实便携数据库将提供方关联保存在配置和模型的 `provider_ref.connection_id`，Rust 只识别旧版顶层 `connection_id`，因而读不到关联地址及凭据并回退官方地址；原端点拼接还会向 `/api/v3` 错加 `/v1`。
- 修复：统一按模型级引用、配置级引用、旧版连接引用解析提供方，复用聊天、诊断、搜索、语音入口；保留用户 API 路径、协议、服务覆盖、凭据和额外请求头。失效引用或缺失自定义地址在发送前拒绝，不按名称推测账户，不回退 OpenAI。
- 回归：加入脱敏的真实配置结构样本及 Rust 测试；加入 `scripts/verify_provider_routing.mjs`，从只读源数据库白名单复制结构，以假密钥和本地网关验收最终发行 EXE。
- 验证：cargo fmt/check/test 通过，Rust 136 项通过；相关前端契约测试 30 项通过；OpenAPI 695 项契约操作、714 项 Rust 路由，缺失 0；最终 EXE 的 10 组路由验收、12 次网关请求全部通过，包括地址变更、模型级优先、Chat/Responses/Anthropic、嵌入、TTS、无效引用/空地址拒绝和重启持久化。
- 发行：重新构建 Release EXE 和 `DeepTutor-1.7.0-portable-X64.zip`；X64/30 项导入依赖/890 个 ZIP 文件一致性及禁入项审计通过；EXE 启动和 25 个模块接口、安装目录内状态检查通过。
- 数据保护：验收前后原用户 SQLite SHA-256 一致；未使用用户密钥调用任何远程服务，未改写原有模型配置。
- 可用性：可编译、可测试、可运行；存在既有 unused/dead_code 编译警告。本次未验证用户远程网关的实时可用性。
- 发行 EXE SHA-256：`230617950d47f8bf71559c2d701b38e02e90a4f63e6b40d6b4a14c49e2032d87`。

## 2026-10-31 23:27:37 +08:00 PDF 阅读导入修复与 Python 实现对照

- 用户要求：对照原 Python 阅读实现修复无法导入 PDF，并验收独立 Rust EXE。
- 对照结论：Python 阅读链路使用 PyMuPDF 按物理页保存原文、原生书签和页内图片，不自动 OCR；通用知识库解析不应提前拦截原页阅读。
- 实现：新增 Rust PDF 按页提取与 Windows 本地 OCR；保留空页、原件、书签、整页预览与图片页码映射；识别不可用仍能阅读原件，显示真实识别状态；知识库继续拒绝空正文。原生处理移到阻塞工作线程，阅读渲染并发限制为 1。
- 新增验证脚本 scripts/verify_pdf_import.mjs、对照文档 docs-for-dev/PDF_READING_MIGRATION.md；构建脚本支持独立 StageName。
- 可用性：cargo check、cargo fmt 检查、148 项 Rust 测试、1314 项前端逻辑测试、27 项阅读界面测试、Vite 桌面构建通过；OpenAPI 695 个操作无缺失。
- 最终 EXE 实测：DSP2.pdf 68 页全部 OCR，13125 字符，14.39 秒；DSP9.pdf 44 页全部 OCR，9882 字符，9.30 秒。原件哈希不变；逐页定位、范围读取、图片访问、重复复用、重新处理、重启持久化、5 份 PDF 的界面渲染均通过。
- 回归：模型自定义地址路由 10 组 EXE 检查通过；25 个模块启动检查、31 项 PE 导入依赖、890 个 ZIP 文件一致性与禁入项审计通过，无 Python/Node 运行依赖。
- 过程修正：预览图不再放大小图；打包目录冲突改为串行重建。桌面单实例导致并发 EXE 验收启动失败，改为串行验收；界面检查区分 SPA 导航 HTML 的现有回退状态与实际 API/资源错误，另用画布像素和截图验证 PDF 正文。
- 最终程序：artifacts/portable/DeepTutor/DeepTutor.exe（原有 data 保留）；干净发行目录 artifacts/portable-package/DeepTutor；ZIP artifacts/DeepTutor-1.7.0-portable-X64.zip。
- EXE SHA-256：7b72f41ff4bd602de179d4c308c06c429f52c928be87047600df5658c28f4c24。
- 验收报告：artifacts/pdf-verification/exe-1793460337217/report.json。真实文档只读分析，未发送到外部服务；用户 PDF、数据库、密钥均不进入 Git。

## 2026-10-31 23:52:41 +08:00 修复文件选择框位置警告并同步更新便携包

- 根因：便携 USERPROFILE 指向安装目录 home，但未创建 Desktop，Windows 原生文件选择框因此弹出“位置不可用”。
- 修复：在初始化 WebView 前创建 Desktop、Documents、Downloads、Pictures、Music、Videos、Favorites；首次启动、旧目录升级和隔离用户目录均生效，保留已有文件。
- 交付约定：在 AGENTS.md 和桌面开发文档中明确每次交付重新编译的 EXE，必须同时更新 ZIP 和 .sha256；统一调用 build_portable.ps1，交付不得使用 -SkipZip。
- 自动校验：150 项 Rust 测试、1314 项前端 Node 测试通过；OpenAPI 695 项契约操作无缺失；EXE 25 个模块、7 个用户文件夹与 31 项 PE 依赖校验通过；ZIP 890 文件一致性通过。
- 真实 EXE 验收：旧版隔离副本成功复现警告且被新增冒烟检查拒绝；从新 ZIP 解压的 EXE 连续两次打开原生文件选择框均无警告；旧目录升级自动补齐文件夹且已有文件哈希不变。
- 回归：自定义模型地址 10 项 EXE 校验通过；文本/空页、扫描 PDF、混合文档、本地 OCR、重试与重启持久化通过；真实用户数据库哈希保持不变。
- 构建：scripts/build_portable.ps1 -SkipFrontend，release 编译与便携打包成功，现有数据未进入发行包。
- EXE：artifacts/portable/DeepTutor/DeepTutor.exe；SHA-256 f591851124d36cd69512dff7c0c1667df9a8f4f9f85560c619c7d0e8483e0eca。
- ZIP：artifacts/DeepTutor-1.7.0-portable-X64.zip；SHA-256 c540d9408ec363458cc8a3b59479327a9fcc4b36505b439b49421436bbb62c9b。
- 验收记录：artifacts/file-dialog-verification/result.json；代码与发行产物状态：可用。未推送远端。

## 2026-11-01 00:05:29 +08:00 清理项目 Rust 构建缓存

- 用户要求清理项目根目录，重点处理过大的 target。清理前全目录文件大小为 58.45 GiB，src-tauri/target 为 50.4 GiB，其中 debug 为 46.61 GiB。
- 操作前确认无 Cargo/Rust 编译进程，target 是当前项目内的普通目录且没有 Git 跟踪文件；现用 EXE 与发行 EXE 哈希一致，便携 ZIP 完整性通过。
- 通过 cargo clean --manifest-path D:/VS_Projects/DeepTutor/src-tauri/Cargo.toml 清除 121806 个生成文件，释放约 50.4 GiB；清理后约 8.05 GiB，target 目录已不存在。
- 保留源码、Git 历史、用户数据、node_modules、静态前端资源，以及最新 EXE、便携 ZIP 和 SHA-256。清理后再次通过 31 项 PE 依赖和 ZIP 890 文件一致性校验，现用 EXE 与发行 EXE 哈希仍一致。
- 自动执行策略拒绝了批量递归删除以及对 .tauri-build-cache 的显式递归删除；改用 Cargo 官方清理命令完成 target 清理，其余缓存和历史验收副本保留，未尝试绕过策略。
- 本轮未修改应用代码或重新编译；现有发行产物完整。下次编译将重新生成 target，首次构建会更慢。清理报告：artifacts/cleanup-result.json。未推送远端。

## 2026-11-01 00:11:36 +08:00 - 构建产物占用说明
- 用户询问目录占用和编译产物的含义；本轮仅核对并解释，没有修改应用代码或构建配置。
- 清理前 target 共 50.40 GiB，其中 debug/deps 21.18 GiB、debug/incremental 19.74 GiB、debug/build 3.34 GiB、debug/resources 2.33 GiB、release 3.79 GiB。
- 复核清理结果：项目由 58.45 GiB 降至 8.05 GiB，target 已清除；现有 EXE 为 44,621,824 字节，便携 ZIP 为 32,941,254 字节。
- 已核对 Cargo 官方构建缓存及编译配置文档，说明磁盘缓存与运行内存的区别、清理对后续编译的影响。
- 代码可用性：应用代码未变更，沿用上一轮 EXE/ZIP 校验结果；本轮未重新编译或重新运行应用测试，避免生成已清除的缓存。

## 2026-11-01 10:32:18 +08:00 沉浸式阅读上下文修复与构建产物自动清理

- 用户反馈：沉浸式阅读中的文档没有被普通对话读取；要求继续对照 Python 阅读链路，并在以后编译完成后自动删除中间产物。
- 修复回合恢复：自动恢复阅读资料集、当前材料、材料版本和视口；校验材料属于资料集；切换材料后更新会话绑定；重新生成时恢复原始材料、选区和视口。
- 修复模型上下文：对当前材料执行本地全文检索，将命中片段、目录、定位信息和当前 PDF 页图加入模型请求；阅读回合注册 8 个阅读工具；支持中文归一化、关键词检索和页码定位。
- 构建脚本：`scripts/build_portable.ps1` 默认在便携包校验成功后清理 `src-tauri/target`；增加 `-KeepBuildArtifacts` 选项保留中间产物，并对清理路径和重解析点执行安全检查。
- 验证：Rust 156 项通过；前端 Node 1314 项通过；OpenAPI 695 个契约操作无缺失；便携 EXE、ZIP、SHA-256、PE 依赖、隔离启动、25 个模块及安装目录数据路径检查通过。
- 当前发行物：`artifacts/portable/DeepTutor/DeepTutor.exe` 与 `artifacts/portable-package/DeepTutor/DeepTutor.exe` 哈希均为 `ee2fc65fbaa2b9f87b74acc3d80ea61451c592599a8b6bd1cfc4b30ac825ce4a`；ZIP 哈希为 `98f2c32eae0609362cef2691325533096045ee03fc326a54d9df4c762492814c`。
- `src-tauri/target` 已清理，不重复编译；本轮代码与发行物状态：可用。未推送远端。

## 2026-11-01 13:19:00 +08:00 沉浸式阅读历史会话恢复关联

- 用户反馈：主页打开某条沉浸式阅读聊天记录后，没有切换到该记录对应的书籍。
- 根因：历史会话缺少持久化的资料集/材料关联；阅读界面一直读取资料集的全局活动材料，而非当前路由会话的材料。
- Rust 启动时从旧会话偏好、待处理回合和消息请求快照恢复明确关联，校验材料确实属于对应资料集；隔离用户数据库先初始化完整 schema，迁移保持幂等且不猜测缺失关联。
- 前端根据路由会话派生材料选择；同一会话内的手动切书优先，资料集轮询和会话切换不会覆盖或串用选择。
- 验证：Rust 163 项通过，`cargo fmt --all -- --check` 与 `cargo check --locked` 通过；前端 TypeScript 检查通过，Node 1321 项、Vitest 998 项通过，ESLint 无错误（159 条既有警告）。
- 发行验证：便携启动冒烟通过；890 个 ZIP 文件、X64 PE 和 31 项导入依赖检查通过。发行目录、便携目录及隔离启动 EXE SHA-256 均为 `f5d3b042a1b1f1c468b41188611d0a06b36d4f17a12aa5a7d2a74d222bf7fb31`；便携 ZIP SHA-256 为 `c02a644b4ddff87a335323feaa0ecc7276be72de05fd9a27388c1e7366a080e8`，与 `.sha256` 一致。
- `src-tauri/target` 已由发布脚本清理；便携用户目录保留。代码和发行包可用，未推送。
