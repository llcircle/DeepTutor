# DeepTutor 开发经验

## Tauri 纯 Rust 迁移

- 桌面客户端不能只替换外壳。Tauri 窗口、Rust Axum 本地服务、回合引擎、持久化、认证、文件、插件、更新和 WebSocket 都必须形成独立运行链路，才能真正解除 Python/Node 运行时绑定。
- v1.7.0 的 Python 代码可作为功能清单和接口参考，但不能成为桌面运行依赖。迁移验证应以 Rust 路由覆盖、能力提示、存储行为和前端交互为依据，而不是检查 Python 包是否可导入。
- 构建期 Node.js 与运行期 Node.js 必须明确区分。Vite 可以在开发机上生成静态资源，但发行目录不得包含 `node_modules`、Next.js standalone、Remotion 或 Node 可执行文件。

## 便携数据

- 便携性不能只设置 `DEEPTUTOR_HOME`。SQLite WAL、日志、上传、生成文件、插件、更新下载、WebView2、HOME、APPDATA/LOCALAPPDATA、XDG 和 TMP 都必须在初始化早期统一指向安装目录。
- WebView2 数据目录必须在窗口创建前设置；否则即使应用数据已便携，浏览器缓存仍可能落入 C 盘。
- 安装目录不可写时应失败并给出明确路径。静默回退到用户目录会破坏便携语义，并导致复制安装目录后丢失状态。
- 打包脚本应白名单复制，并对 Python、Node、旧 runtime、Next.js、Remotion 和用户数据做显式黑名单审计。

## 插件与外部程序

- 第三方 Python wheel 插件不适合作为纯 Rust 客户端扩展。初始迁移采用无 host import 的 WASM ABI，不暴露文件、网络、进程和环境变量能力。
- WASM 仍需限制模块、请求、响应、线性内存、燃料和墙钟时间，否则单个插件可拖垮桌面服务。
- `exec` 默认关闭，只调用用户系统已安装的运行时。清空环境、独立工作目录、输出限制和超时可以降低风险，但不能宣称是完整 OS 文件系统沙箱。
- 文档解析器应如实区分内置 Rust 格式、远程服务和外部 CLI。缺失外部解析器时返回明确 readiness 状态，不能伪造成功或启动 Python。

## 纯 Rust Tauri 迁移经验（2026-10-31）
- Windows 便携路径必须对已存在目标文件使用空后缀直接返回规范化路径，避免 join("") 产生尾部分隔符导致 is_file 误判。
- 插件运行时只接受 wasm: Tool/Capability/Reading Extension 入口；拒绝 Python entry_point，且每次执行都重新计算安装包 SHA-256。
- 插件 Tool、Capability、loop_capability 使用统一 JSON 契约；Reading Extension 需校验 selection/visible_text，并校验 card、quiz、feedback、browser_speech payload。
- visualizer 扩展通过插件内 visualizer.json 接入 Rust visualizer catalog，资源由沙箱 iframe 服务；persistence_schema/app_connector 保持 v1.7.0 的声明式契约语义。
- 便携构建审计要同时检查目录和 ZIP；缓存、日志、临时文件、WebView2 用户数据必须相对安装根目录解析。

## 测试契约与发布收口

- 旧测试若固定断言 Python、Next.js 或 Remotion 必须随桌面运行时启动，应改为验证纯 Rust/Tauri 构建链、静态 Vite 产物和便携路径约束，否则测试会阻止正确的架构迁移。
- 替换 CommonJS/Next.js 桌面 shim 时不要保留名为 `module` 的变量；它会与 ESLint 的 CommonJS 作用域规则冲突。使用明确的配置变量名，并用静态检查及时发现残留运行时假设。
- 便携 ZIP 必须在最终前端重建之后生成。任何后续前端构建都会使旧 ZIP 和 SHA-256 失效，发布记录应明确以最终哈希替代旧值。
- 发布审计应分别读取展开目录与 ZIP 条目，比较文件数并搜索禁入路径；只检查构建脚本参数不足以证明发行包实际没有夹带 Python、Node 或用户数据。

## 2026-10-31 10:07:18 +08:00
- Windows 便携 Tauri 包必须把 WebView2Loader.dll 视为一等原生依赖，并在构建、manifest、升级说明和自动验证中同时覆盖。
- Axum 路由迁移后应增加“路由可构建”测试；同一路由由多个模块注册会在运行时直接 panic，单看编译无法发现。
- 桌面静态导出的根页面不能只返回 index.html，应显式进入应用主路由，否则 React 路由会把空 path 解释成 404。
- 前端能力门控依赖的 /api/modules 必须在 Rust 后端提供等价契约，否则界面会退化为通用错误/重试状态。
- 便携性验证不能只检查目录结构；应从隔离副本启动最终 EXE，同时核对 SQLite、WebView2 缓存、日志、temp、home 和动态端口 API。
- 发行验证应把 PE 导入表、目录/ZIP 文件集合、SHA-256 和真实 WebView 首屏纳入同一套可重复脚本。

## 2026-10-31 11:26:22 +08:00
- 桌面 `next/dynamic` 兼容层必须同时接受“Promise 组件”和“Promise 模块”。常见写法 `.then(module => module.NamedComponent)` 最终拿到的是组件本身，把它当模块读取 `default` 会报“动态模块没有可渲染的 React 组件”。
- React 组件判别不能只检查 `typeof value === "object"`；普通模块对象、React 元素和 memo/forwardRef 的结构不同，应分别识别函数组件、类组件、`render` 函数和 `$$typeof`，并对最终导出做显式错误提示。
- 聚合型导航接口要严格遵守前端读取契约。`/api/dashboard/learning-index` 必须返回 `sources.books/mastery/reading/watching/practice` 和 `failed`，旧的 `{items}` 即使 HTTP 200 也会在 React 状态更新阶段崩溃。
- Rust 聚合接口应逐数据源隔离错误并记录 `failed`，不能让一个损坏资源使整个导航失败。书籍、掌握路径和阅读集合应返回各自 API 的完整视图，避免前端读取嵌套统计字段时出现二次错误。
- 便携应用正在运行时，WebView2 会锁定安装目录缓存。重新清空 stage 前先关闭旧 EXE；最终 GUI 验证后也应关闭测试实例，再进行 Git 收口。

[2026-10-31 15:00:04]
- 不要在 React 渲染函数内部创建 lazy() 或 dynamic() 组件。每次渲染都会生成新的组件类型，导致 Suspense/惰性内容反复卸载或永不提交；应提升到模块级，路由参数通过 props 传入。
- 桌面静态路由的特殊页面也需要显式组合应用布局，不能只返回页面内容，否则会绕过既定 Shell 与 Provider。
- 对动态分页做 GUI 验证时，应在点击后等待一次稳定的刷新再判断空白，区分瞬时加载与真实渲染故障。

## 2026-10-31 19:16:44 +08:00
- 设置“应用”接口若由前端发送无请求体，后端不能强制使用 Json extractor；应读取原始 Bytes，空体回放服务端保存的草稿，同时保持显式 catalog、密钥掩码恢复和响应契约。
- 便携安装目录与发行包目录必须分离。构建时更新安装目录中的发行文件但保留用户状态，ZIP 只从白名单组成的干净目录生成，否则一次构建就会删除 SQLite 和未应用草稿。
- 便携审计要区分“发行载荷”和“运行时数据”。data/cache/temp/logs/home 中的数据库、WebView2 缓存和日志是预期产物，不能被禁入扫描误判；ZIP 则必须完全禁止这些目录夹带。
- EXE 冒烟测试不要复制现有用户数据，尤其是 Windows INetCache 等带 ACL 或被锁定的目录。只复制发行载荷到全新目录，才能验证首次启动创建 data/cache/temp/logs/home 的真实行为。
- 动态设置路由首屏可能短暂为空。GUI 验证应等待一次稳定刷新再判断空白，随后用界面状态和本地 API 双重确认“草稿提升为正式配置”。

## 模型连接与设置接口经验
- 模型 profile 中的 connection_id 是运行时凭据和地址的权威关联；任何诊断、聊天、后台任务路径都必须先执行连接解析，不能直接读取 profile.base_url。
- 回归测试应故意保留过期的 api.openai.com 地址，再断言连接地址覆盖成功，才能防止默认地址回退再次发生。
- Axum 中同一路径的多个 .route 调用可能覆盖此前方法组合；GET/PUT 共用路径时应在同一次 method router 中声明，避免 GET 被静态 PUT 路由挤掉。
- 前端页面个性化状态需要完整的 chat/learning 文档；后端读取和保存都应合并默认值并返回完整文档，不能只返回当前分段或原始稀疏对象。
- 验证自定义 LLM 地址时，使用本地模拟 OpenAI 服务并让配置保留旧地址，可同时证明请求目标解析和真实 HTTP 调用正确，也避免向外部服务泄露 API Key。

## 2026-10-31 提供方引用与最终发行包验收

- 前端 `modelProviderRef()` 的优先级是模型 `provider_ref`、配置 `provider_ref`、旧版 `connection_id`。Rust 必须实现同一契约；仅扩充旧版顶层字段的测试不能覆盖真实用户数据。引用只能按显式 ID 解析，不能按名字或唯一账户猜测。
- 配置和模型中的空地址是正常的引用结构，实际地址和凭据属于提供方。不要把这些空字段当成配置错误并回退官方服务；关联失效应在发送前报错。
- 自定义 `/api/v3`、`/compatible-mode/v1` 等路径必须保留，不能一律补 `/v1`。完整接口地址、查询参数以及不同 API 协议需分别回归。
- 以真实数据库只读取得结构，白名单脱敏并替换全部凭据、地址，在独立便携目录运行最终 EXE，通过 HTTP 保存、WebSocket 聊天、SSE 诊断、本地模拟网关请求记录和重启验证整条链路。不要仅用旧版合成配置的单元测试宣布修复。
- 启动验收需显式设置 RUST_LOG；解析服务地址时应限定独立 `url=` 字段，避免把 `target_url=` 误认为本地服务端口。WebSocket 的 `protocol_error` 也是发送前拒绝的终态。
- 同一配置应分别验证聊天与诊断，不能只断言最后一次请求；同时核对关联凭据和额外请求头，报告记录被测 EXE 的 SHA-256。不得使用真实用户密钥做外部探测。

## 2026-10-31 23:27:37 +08:00 PDF 阅读迁移验收经验

- 对照旧实现要追到阅读专用入口，而不能把知识库通用解析器当作阅读规范。PDF 的 locator 必须按物理页保留，不能按远程解析后的段落重新编号。
- 扫描 PDF 缺文本层不代表不能阅读。原件、图片及识别状态应分别保存；图片标记不能算作 OCR 正文，也不能让知识库误以为已完成文字索引。
- Windows WinRT OCR 可从 Rust 直接使用。COM 初始化、识别及释放须在同一阻塞工作线程中完成；先释放 OCR 引擎，再反初始化 COM。实际 EXE 的识别验收比只检查语言包或编译更可靠。
- OCR 结果必须直接交给阅读分片，不能随后用文本层再次覆盖；无书签 PDF 在提取与详情接口两层都不得生成虚构目录。
- 页图尽力生成并限制尺寸、数量和总容量；不要把小图自动放大。原始 PDF 始终完整保留。
- 桌面应用具有单实例限制，EXE 验收必须串行；多个打包任务也不能同时清理和写入同一发行目录。开发用户数据与干净发行载荷应分开。
- 实际 UI 验收要检查 PDF 画布有正文像素及资源/API 成功，并阅读截图。SPA 导航 HTML 的回退状态需要和 PDF 加载错误区分；相关路由状态行为已记录在迁移文档。
- 当前本机 68 页、44 页的真实扫描课件分别在约 14.4 秒和 9.3 秒完成本地 OCR；这些数字是本机验收结果，不能作为其他机器的性能保证。

## 2026-10-31 23:52:41 +08:00 便携 Windows 用户目录与交付一致性

- 重定向 USERPROFILE 后，必须在创建 WebView 之前补齐 Desktop 等标准用户文件夹；仅创建 home 本身会使原生文件选择框弹出位置不可用，虽然文件选择仍能继续。
- 初始化采用 create_dir_all，保证新安装、旧目录升级及多用户隔离均可重复执行，且不覆盖已有文件；无需更改系统 KnownFolder 注册表，也不回退 C 盘用户目录。
- 回归应同时覆盖新目录创建、已有文件保留和真实 EXE 文件选择框；本次旧版复现警告，新版 ZIP 内 EXE 连续两次无警告，冒烟脚本已纳入 7 个标准文件夹的检查。
- 用户要求：以后每次重新编译并交付 EXE，同步更新便携 ZIP 及 .sha256。统一运行 scripts/build_portable.ps1，可按需 -SkipFrontend，不得在交付时 -SkipZip；最后比较 ZIP 与已更新 EXE 哈希。打包和单实例 EXE 验收均串行执行。
