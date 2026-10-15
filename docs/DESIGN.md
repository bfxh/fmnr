# Rust 独立文件管理器（Fmnr）— 设计文档 v1（合并版）

> 唯一事实源。合并了 v1（架构/范围/论文/选型）与 v2（打开语义/Liquid Glass 视觉/Zexor 借避/仓库）。
> 输入：对 Zexor（https://zex.leoog.com/）UX 的认可 + 用户需求（独立于系统、自绘文件图标、统一预览+状态可见、iOS 26 Liquid Glass 视觉、跨协议/AI 兼容、WSL 感知）+ Rust 最新工具链。
> 版本：v1（2026-10-15）。后续改动只改本文件，并在 §12 记台账。

---

## 0. 用户需求（原话要点）

1. 「Zexor 的 UX 还是可以的」—— 参考对象，不等于照抄。
2. 用 **Rust** 写，**最新工具链**。
3. 文件管理器要**独立**：不依赖系统文件管理器。
4. 知道子系统里有其他系统 —— 本机 WSL 未启用，因此**按规范实现 WSL 感知，无法本机验证**（见 §6.3）。
5. **图标可以随意更改** —— 指**文件图标**：自绘渲染、不读系统 shell 图标。
6. **统一预览（UV）+ 状态可见**：部分文件用内建预览打开，部分交给默认软件打开，且**要让用户知道它已被默认软件打开**。
7. 视觉喜欢 **iOS 26 Liquid Glass**（透明玻璃、胶囊体）；Zexor 部分 UX 不喜欢（硬核、键位隐藏、双击直丢系统）。
8. 后续会**内置很多软件**。
9. 与**其他软件兼容**：给别的软件用（MCP/HTTP/CLI/协议句柄），也能被别的软件接管。
10. GitHub **公开**新仓库。

---

## 1. 需求翻译（原话 → 可执行要求，含陷阱）

| # | 原话 | 可执行要求 | 陷阱 |
|---|------|-----------|------|
| R1 | 用 Rust、最新工具链 | Rust **stable 1.99.0**（本机 `rustc 1.99.0 / cargo 1.99.0`）。工具链写 `stable = "1.99"`，不加 nightly | dist 的 `0.100.0` 是**预发布**编号，正式 stable 就是 1.99.0，别被误导。升级走 `rustup update`，CI 用 `@stable` |
| R2 | 独立于系统文件管理器 | 自研目录枚举、自绘图标、自带预览、自带打开逻辑；核心链路不 shell out 给 explorer/xdg-open | 「独立」≠ 完全不接系统：注册默认程序、调用系统库仍需系统 API。独立指**核心浏览/操作不经过资源管理器** |
| R3 | 文件图标随意更改 | 内置图标管线：内置集 + 每扩展名覆盖 + 每文件名覆盖 + 每文件 hash 覆盖 + 主题打包 | 用户强调是**文件图标**（非文件夹封面），两者分开治理（§4.5/§4.6） |
| R4 | 知道 WSL 里还有其他系统 | 识别 `\\wsl$\<distro>\...` UNC + `wslpath` 往返；区分 distro | **本机 WSL 未启用**（`wsl -l -v` 无输出）⇒ 按规范实现 + 契约测试桩 + 标注未真实验证 |
| R5 | 统一预览 + 状态可见 | 双击优先内建预览；外置打开走自 spawn 拿句柄 + UI 状态角标「已在 XX 打开」，进程退出消失 | 关键：**自己 spawn 而非 ShellExecute**，否则拿不到进程句柄做存活检测。见 §4.7 |
| R6 | iOS 26 Liquid Glass | 玻璃半透明层 + 胶囊圆角 + spring 动效，egui acrylic 近似 | immediate-mode 做不了像素级多层 backdrop blur，用窗口 acrylic + 面板半透明近似（§5） |
| R7 | Zexor 式 UX | 借：双栏+面包屑过滤、空间白板、单 EXE、滚轮调密度。避：硬核暗色、键位隐藏、双击直丢系统、全家桶 | Zexor 闭源（languages API 空），借鉴只能基于官网描述，属**假设**，需评测检验 |
| R8 | 后续内置很多软件 | 每个「内置软件」= WASM 插件，预览面板=插件宿主，内置=预装插件 | 不塞进 EXE，保持精简，生态靠插件 |
| R9 | 与其他软件兼容 | 入向：MCP/HTTP/CLI 给外部调用；出向：默认程序注册、`fmnr://` 协议、拖放 | MCP 与独立目标不冲突：共享同一内核 |
| R10 | GitHub 公开仓库 | `fmnr`，Public | 待用户名 + license |

---

## 2. 范围（Scope）

### v0（MVP，约 2-3 周可跑）
- 本地磁盘双栏浏览（导航/不导航目录）+ 面包屑 + 列视图，键鼠双优（快捷键 + 全功能可见鼠标入口）
- 自绘文件图标：内置集 + 扩展名覆盖
- 即时搜索（名称子串 + 通配，作用域=当前子树）
- 基础文件操作：复制/移动/删除（回收站优先）/重命名/新建，原子化 + 冲突处理
- Liquid Glass 视觉基调（窗口 acrylic + 胶囊控件）
- 单 EXE 分发

### v1（核心差异化）
- **统一预览面板**（§4.7）：文本/代码/图片/压缩包内建渲染 + 外置打开状态角标
- 插件系统（WASM 沙箱）：图标提供器、自定义列、文件操作、预览渲染器（内置软件=预装插件）
- 虚拟来源层：归档（zip/tar/7z 只读挂载）、SSH/SFTP、UNC
- MCP server + HTTP API + CLI
- 白板/空间模式（Data Mountain 启发）
- 拖放 + 自定义协议注册

### v2（生态）
- WASM 插件市场（打包分发 + 版本解析）
- 更多虚拟来源（FTP、WebDAV、S3 只读）
- 深链 `fmnr://open?path=...&select=...`
- 主题引擎（token → 样式表，可换肤）
- 全盘索引引擎（即时搜索之上，候选，复杂度高收益不确定）
- PDF/视频预览（可能仍走外部+角标）

### 明确不做（Non-goals）
- **不做系统级 Shell 扩展**（IThumbnailProvider/IContextMenu COM 注册）：污染系统、版本管理复杂、与最小侵入冲突。改「可选 opt-in 扩展包」，独立 EXE 默认不带
- 不做 Windows 之外平台首发（Windows 优先，Linux 次之，macOS 不在承诺）
- 不做文件内容同步（OneDrive/Dropbox 类）
- **不内置 FFmpeg**（§4.7.4）
- 不做 iOS Files 的交互逻辑（只取 iOS 视觉语言）

---

## 3. 产品分析：Zexor

### 3.1 官网提取（事实）
- 定位：「Windows Native File Explorer & Modern Archive Formats」，Win10/11 x64，**单 EXE**，FFmpeg 集成版
- 功能宣称：`.zex` 归档虚拟执行、FastCopy 级多线程拷贝、无限白板、Everything 级毫秒搜索+即时文件夹大小、局域网发现、NAS/SSH/SFTP/FTP、二维码传输、torrent/magnet、浏览器下载接管、AI 自然语言操作、媒体/文本/代码/3D 即时预览与行内编辑、SMART、C 盘清理、查重、加密保险库、文件夹封面/颜色/树标签、滚轮缩放
- 仓库 `github.com/metalbug/Zexor` 仅发二进制（languages API 空 ⇒ 无源码可析）

### 3.2 借鉴 vs 规避（对应用户不喜欢项）

**借鉴**
- **双栏 + 面包屑行内过滤**：论文支撑（§7.2），但改鼠标友好——面包屑每段可点、过滤框常驻可清空
- **空间/白板模式**：Data Mountain 有据（§7.1），独立 EXE 自绘画布无系统依赖
- **归档只读挂载**为虚拟目录：`.zex` 直接执行的噱头不复制（需自研运行时），但 zip/tar/7z 只读浏览收益明确
- **单 EXE 无安装污染**：独立目标的一部分

**规避（用户明说不喜欢）**
| Zexor 问题 | 我们的做法 |
|-----------|-----------|
| 双击直接交给系统打开、无状态 | §4.7 统一预览 + 状态角标，默认预览已知类型 |
| 视觉硬核暗色 | §5 Liquid Glass 默认浅色玻璃风 |
| 键盘优先、鼠标党难用 | 键鼠双优：保留快捷键，但所有功能有可见鼠标入口（按钮/菜单/右键），无隐藏键位 |
| 功能全家桶臃肿 | 范围硬约束（§2），新功能先删旧承诺 |

**Zexor 风险警示**：功能面极广（文件管理到磁盘维护到 BT），**膨胀是最大风险**。对策：范围表硬约束。

### 3.3 定位差
Zexor = Windows 闭源全家桶；我们 = **可编程、跨协议、AI 可驱**的 Rust 独立文件管理器，插件生态 + 自定义图标 + 统一预览 + Liquid Glass。交集是「本地浏览 UX 好」，差异是「内核 API 化 + 插件化 + 图标自由 + 预览状态可见」。

---

## 4. 架构

### 4.1 设计原则（写死）
1. **内核是纯库**：`fmnr-core` 不依赖 UI/窗口。UI 和 MCP server 都是它的客户端。
2. **来源抽象**：所有「能列出/读/写的东西」实现 `Volume` trait。本地盘、归档、SSH、UNC 统一抽象。
3. **无系统核心依赖**：枚举目录、读文件、画图标全在进程内。系统 API 仅用于「注册默认程序」「spawn 外部程序打开文件」两个明确边界。
4. **一切异步**：IO 走 tokio，CPU 密集（图标渲染、hash、拷贝校验）走 rayon，UI 线程永不阻塞。
5. **配置即数据**：图标覆盖、主题、插件清单都是可版本化文本/TOML。
6. **能力可测试**：每个 kernel API 有契约测试；UI 之外逻辑 100% CI 可验证。

### 4.2 工作区布局
```
fmnr/
├── Cargo.toml
├── crates/
│   ├── fmnr-core/      # 纯库：卷/文件树/操作/搜索/配置（无 UI 依赖）
│   ├── fmnr-fs/        # 本地文件系统 Volume（jwalk + tokio）
│   ├── fmnr-archive/   # zip/tar/7z 只读挂载 Volume
│   ├── fmnr-remote/    # SSH/SFTP、WebDAV Volume（russh / reqwest）
│   ├── fmnr-icons/     # 图标解析/渲染：内置集+覆盖解析+光栅化
│   ├── fmnr-plugins/   # WASM 插件宿主（wasmtime + wit-bindgen）
│   ├── fmnr-ai/        # MCP server + HTTP API
│   ├── fmnr-ui/        # egui 前端（可执行）
│   └── fmnr-cli/       # 命令行入口（可执行）
├── assets/icons/       # 内置图标集（SVG，构建时合成）
├── plugins/            # 官方插件（WASM）
└── docs/
```

### 4.3 核心抽象
```rust
#[async_trait]
pub trait Volume: Send + Sync {
    fn id(&self) -> VolumeId;                    // "local:C" "wsl:Ubuntu" "ssh:host" "archive:/x/a.zip"
    fn kind(&self) -> VolumeKind;                // Local | Archive | Remote | Unc | Wsl
    async fn list(&self, path: &VPath, opts: ListOpts) -> Result<Vec<Entry>>;
    async fn read_stream(&self, path: &VPath) -> Result<BoxStream<Bytes>>;
    async fn write(&self, path: &VPath, data: Bytes) -> Result<()>;   // 只读卷返回 Unsupported
    fn writable(&self) -> bool;
    fn native_path(&self, path: &VPath) -> Option<PathBuf>;            // 拖放/外部打开用
}

pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub icon_key: IconKey,        // fmnr-icons 解析（hash/文件名/扩展名/内置 优先级链）
    pub attrs: Vec<Attr>,         // hidden, system, symlink, readonly...
}
```

### 4.4 文件图标管线（R3）
解析顺序（先命中先用）：
1. **按文件 hash 覆盖**（内容寻址，同文件任何位置同图标）
2. **按具体文件名覆盖**（如 `Cargo.toml` 专用图标）
3. **按扩展名覆盖**（如 `.rs` → rust.svg，用户可换）
4. **内置集**（按扩展名分类：代码/图片/视频/压缩/文档/系统）
5. **fallback**：MIME/魔数嗅探泛型图标

存储 `icons/overrides/<hash|name|ext>.svg|png`，热加载，改文件立即生效。
渲染：`usvg`+`resvg`（或 `tiny-skia`）在独立 rayon 线程光栅化到缓存纹理，UI 只取 `egui::TextureHandle`。**不调用 SHGetFileInfo**。
远程卷只走「扩展名/文件名」覆盖（hash 在远程卷代价高，不做）。

### 4.5 文件夹封面（与文件图标分开治理）
Zexor 有文件夹封面。轻量版：右键「设为封面」，存 `folder.svg`/`cover.png`，仅白板/大图标视图显示。**分表存储**，避免与文件图标规则混用。

### 4.6 虚拟视图（组合）
- **归档挂载**：zip/tar/7z 暴露为 `Volume`，路径 `archive:/D/x/a.zip/sub/dir`，只读、懒加载目录。
- **白板模式**：布局层非 Volume。`Board` 记录节点（文件引用+画布坐标+连线），持久化 `board.json`。文件双击经 `native_path` 交系统默认程序（本地）或拉取后打开（远程）。
- **搜索**：即时搜索 = 遍历当前子树 `Entry` 流 + 谓词过滤（jwalk 并行 + 谓词下推）。全盘索引引擎 v2 候选。

### 4.7 统一预览与打开语义（R5，核心 UX）

#### 4.7.1 统一预览面板（UV）
文件管理器的**内建渲染区**，双击/空格优先在此渲染，不立刻丢给外部程序。

| 类型 | 预览方式 | 说明 |
|------|---------|------|
| 文本/代码/HTML/JSON/Markdown | 内置渲染（syntect 高亮） | 只读；行内编辑 v2 候选 |
| 图片 | resvg/image 解码 | 缩放/平移 |
| 压缩包 | 导航进虚拟卷 | 双击进 zip 浏览 |
| 视频/音频 | **不内置解码**：内嵌播放器调系统 codec 或降级「交给默认程序」+角标 | 避免 FFmpeg 体积/许可 |
| PDF | pdfium-render 或降级外部 | v2 候选 |
| 其他/未知 | **交给默认程序**，UI 标注「已由 XX 打开」 | 见 4.7.2 |

#### 4.7.2 「被默认软件打开」的状态可见（关键）
三条硬规则：
1. **外置打开必须留痕**：经系统默认程序打开的文件，Fmnr 内出现状态角标/横幅「已在 记事本 中打开」，持续到外部进程退出或用户手动关闭。
2. **打开方式可切换**：右键「用预览打开 / 用默认程序打开 / 选择其他程序…」，按扩展名记忆。默认：已知类型→预览，未知→默认程序。
3. **进程感知**：外置打开时记录启动进程句柄（Windows: `CreateProcess` HANDLE；Unix: PID），轮询存活驱动角标。进程退出 → 角标自动消失。

> 实现要点：**自己 `CreateProcess`/`Command::spawn` 启动外部程序，而非 `ShellExecute`**，才能拿句柄/PID 做存活检测。这是「独立」与「状态可见」的共同要求。

#### 4.7.3 打开决策表（UI 与 MCP 共用）
```
双击/空格 文件 X：
  if X 扩展名 ∈ 内置可预览集 且 用户未强制外部:
      在 Preview 面板渲染 X            → 状态: 「预览中」
  else:
      确定目标程序 P（按扩展名配置 / 系统默认 / 用户选择）
      spawn P(X) 并记录 (P, handle)
      UI 标记 X → 状态: 「已在 <P名> 中打开」(角标)
      P 退出 → 角标消失
```

#### 4.7.4 不内置 FFmpeg 的决定
体积（+30~80MB）、许可（LGPL/GPL 边界）、维护成本都不划算。视频音频走「默认程序打开 + 状态角标」，预览面板只显示封面/元信息。外置打开因此是一等公民，不是缺陷。

### 4.8 内置软件 = 预装插件（R8）
后续「内置很多软件」不塞进 EXE。每个是一个 WASM 插件（M5）：Markdown 预览器、图片查看器、HEIC 转换器、压缩工具、颜色取样器等。**预览面板是插件宿主**：内置预览集 = 一组预装插件，按 MIME 注册渲染器；第三方软件同理插件接入。EXE 保持精简，生态靠插件市场。

### 4.9 UI：egui 的理由
- **纯 Rust**，单 EXE，无 webview 运行时依赖 —— 符合「独立」。
- 自绘一切 —— 符合「图标随意更改」，不受系统控件主题干扰。
- 性能/内存优于 webview。
- 代价：手搓控件。对策：v0 复用 `egui_extras`/`egui_dock`，白板用 `egui::Painter` 自绘。
- 备选不选：Slint（1.x API 变动）、Iced 0.14（Elm 对复杂布局不友好）、Tauri（webview 与「独立」冲突）。

### 4.10 外部兼容（R9 三向）
| 方向 | 载体 | 说明 |
|------|------|------|
| 被外部程序/智能体调用 | **MCP server**（`fmnr-ai`，stdio JSON-RPC） | 工具：`list_volume` `list_dir` `search` `copy` `move` `delete` `open` `set_icon` `board_*`。与 UI 共享内核 |
| 被浏览器/其他应用调用 | **HTTP API**（`--serve 127.0.0.1:port`，axum） | 配合 `fmnr://` 协议注册 |
| 调用外部程序 | 注册默认程序 + `std::process` spawn | 拖放源实现 `CF_HDROP`，接受外部拖入 |

---

## 5. 视觉语言：iOS 26 Liquid Glass（R6）

### 5.1 设计令牌（可换肤）
| 令牌 | 默认（浅色玻璃） | 用途 |
|------|----------------|------|
| `bg.base` | `rgba(255,255,255,0.72)` over 壁纸模糊 | 窗口底 |
| `bg.panel` | `rgba(250,250,252,0.65)` + blur 24px | 侧栏/面板 |
| `bg.elevated` | `rgba(255,255,255,0.85)` | 弹层/菜单 |
| `stroke.hairline` | `rgba(0,0,0,0.08)` 1px | 分隔线 |
| `accent` | iOS 蓝 `#007AFF` | 选中/主操作 |
| `radius.capsule` | 999px | 按钮/搜索框/标签/面包屑段 |
| `radius.card` | 14px | 卡片/面板 |
| `blur.radius` | 24px | 背景模糊 |
| `shadow.float` | `0 8px 32px rgba(0,0,0,0.12)` | 悬浮元素 |
| `motion.spring` | damping 0.7, stiffness 180 | 所有转场 |

深色模式：`rgba(28,28,30,0.72)` 系，accent 不变。

### 5.2 egui 实现路径（acrylic 近似）
egui immediate-mode **无原生 backdrop blur**。方案：
1. **推荐：窗口级半透明 + 自绘层次近似**
   - eframe 窗口透明（`with_transparent(true)`）。
   - 窗口背景磨砂：Windows `SetWindowCompositionAttribute`（DWM acrylic/blur-behind，`windows` crate 可调）。
   - 面板间「玻璃感」：半透明色叠加 + hairline 描边 + 内阴影模拟，不逐像素模糊。
   - 效果：窗口整体磨砂，内部面板半透明层次表达层级。iOS 观感核心（层次+磨砂）够用。
2. 不做：离屏模糊贴图（复杂、性能差）。
3. 不做：webview（破坏「独立」）。

**诚实边界**：immediate-mode 做**真正**多层 backdrop blur 成本极高。本方案是近似。要像素级 Liquid Glass 需换 wgpu 后端，列 v3 候选。

### 5.3 胶囊控件
搜索框、工具栏按钮、路径胶囊（面包屑每段）、文件类型标签、主操作按钮——全 `radius.capsule` + 玻璃底 + 细描边。iOS 观感最强单点。

---

## 6. 技术选型（版本已核实）

| 用途 | crate | 版本（2026-10-15） | 说明 |
|------|-------|------|------|
| 工具链 | rustc/cargo | **1.99.0**（本机实测） | rustup 1.29.1；CI `@stable` |
| 异步 | tokio | 1.x latest | |
| 并行遍历 | jwalk | 0.9.0 | |
| 目录监听 | notify | 9.0.0-rc.5 | rc，锁 `=9.0.0-rc.5` |
| GUI | egui + eframe + egui_extras | latest | 自绘 |
| 图标渲染 | resvg / usvg / tiny-skia | latest | SVG→纹理 |
| 语法高亮 | syntect | latest | 代码预览 |
| 归档 | zip / tar / sevenz-rust | latest | 只读 |
| SSH/SFTP | russh / russh-sftp | latest | |
| HTTP | axum | latest | |
| MCP | 自实现 JSON-RPC over stdio（薄封装） | 视生态 | 可控 |
| WASM | wasmtime + wit-bindgen | latest | 沙箱 |
| 配置 | serde + toml | latest | |
| 错误 | thiserror + anyhow | latest | |
| 日志 | tracing | latest | |
| 单 EXE | 静态链接 VCRT | — | `fmnr.exe` 无运行时依赖 |

**决策**：egui 决定性因素 = 纯 Rust + 全自绘，直接支撑 R2/R3/R6。

---

## 7. 学术依据（论文分析）

> 以下均为真实文献（检索得到）。结论受限于摘要级理解，落地前应读全文。

### 7.1 Data Mountain（Robertson et al., CHI 1998）
文档放 3D 倾斜平面任意位置，利用**空间记忆**检索比列表/树更快、满意度更高。→ 白板/空间模式有理论根基。设计要点：位置即语义，不自动排布（破坏空间记忆）。

### 7.2 Improving navigation-based file retrieval（CHI 2013 线）
导航式检索瓶颈在**层级间切换开销**；行内过滤（当前层输入即过滤）显著降低「点错层」回溯成本。→ 面包屑行内过滤低成本高收益，v0 就做，且改鼠标友好（每段可点、过滤框可清空）。

### 7.3 File hyper-searching explained（Bergman, 2024, HCI）
检索分导航式 vs 查询式，用户**策略切换**，界面应同时支持而非二选一。→ 目录树 + 即时搜索 + 搜索直达并存。

### 7.4 The effect of folder structure on personal file navigation（Bergman et al., 2013）
文件夹深度与导航效率强相关，用户倾向浅而宽。→ 默认视图给宽目录优化（列视图、面包屑），不强制深层折叠。

### 7.5 可用性框架（ISO 9241-11 + Nielsen）
效率、满意度、容错是硬指标，评审按 Nielsen 十条（§8）。

### 7.6 文献局限
上述多为 1998–2013 经典 + 2024 综述，**没有「AI 可驱动文件管理器」专门文献**。MCP/智能体交互无成熟理论，按工程惯例 + 快速迭代。Liquid Glass 是 2025–2026 新设计语言，学术文献极少，本文 token 体系基于 Apple HIG 公开材料 + 通用玻璃质感设计原则。

---

## 8. 评测口径（Nielsen + 性能）

| 维度 | 指标 | 判据 |
|------|------|------|
| 效率 | 打开 10k 文件目录到可交互 | <300ms |
| 效率 | 全局搜索（当前子树）首屏 | <500ms |
| 效率 | 拷贝 1GB 时 UI 帧率 | ≥30fps |
| 容错 | 删除走回收站可恢复 | 回收站 API 成功 |
| 满意度 | Nielsen SUS（5 人） | ≥68 均值 |
| 独立性 | 禁用 explorer.exe 后核心功能可用 | 浏览/搜索/操作仍工作 |
| 状态可见 | 外置打开后角标出现、进程退出后消失 | 契约测试 |
| 兼容 | MCP 契约测试套件 | 全绿 |

---

## 9. 风险与对策

| 风险 | 等级 | 对策 |
|------|------|------|
| 功能膨胀（Zexor 前车之鉴） | 高 | 范围硬约束；新功能先删旧承诺 |
| egui 手搓控件工作量大 | 高 | v0 复用 egui_extras；白板 Painter 自绘接受粗糙 |
| Liquid Glass 近似不够「真玻璃」 | 中 | 文档标注近似；v3 评估 wgpu 后端 |
| 外置进程句柄泄漏/僵死 | 中 | spawn 后登记，UI 关闭时 kill 子进程；心跳超时兜底 |
| 图标 hash 覆盖在远程卷失效 | 中 | 远程卷只走扩展名/文件名覆盖 |
| WSL 无法本机验证 | 中 | 契约测试桩 + 标注；启用 WSL 后补验证 |
| MCP 生态协议变更 | 中 | 薄封装，协议层可替换 |
| 归档/远程卷性能 | 中 | 只读 + 懒加载；大 zip 不全量枚举 |
| 单 EXE 与 VCRT 静态链接体积/兼容 | 低 | 默认静态链接 CRT |

---

## 10. 里程碑

| 阶段 | 内容 | 出口判据 |
|------|------|---------|
| M0 骨架 | workspace + egui 窗口 + 本地目录浏览（双栏/面包屑/列视图）+ 键鼠双优 | 浏览 C:\ 任意目录，<100ms 列目录 |
| M1 图标+视觉 | 图标管线 + 内置集 + 扩展名覆盖 + Liquid Glass 基调 | 自定义 `.rs` 图标生效；窗口 acrylic 磨砂可见 |
| M2 搜索+操作 | 即时搜索 + 复制/移动/删除/重命名/冲突 | 拷 1GB 不卡 UI |
| M3 预览+状态 | 统一预览面板（文本/图片/压缩包）+ 外置打开状态角标 | 双击 txt 预览；打开未知类型挂角标，进程退出消失 |
| M4 AI/兼容 | MCP server + CLI + HTTP + 拖放 + 默认程序注册 | 外部 MCP 客户端能 list/search/copy |
| M5 插件 | WASM 宿主 + 图标/预览插件契约 | 装一个自定义预览插件并生效 |
| M6 白板 | Board 布局 + 空间记忆模式 | 布置+持久化+恢复 |
| M7 远程+WSL | SSH/SFTP 卷 + WSL UNC 识别 + wslpath | 连一台 SSH 主机浏览（WSL 仅契约测试） |

---

## 11. GitHub 仓库

- **名字 `fmnr`**（file manager in rust 缩写），**Public**。
- 远端：https://github.com/bfxh/fmnr.git（用户名 **bfxh**，license **MIT**）。
- README 首屏三个卖点：Liquid Glass 视觉、统一预览+状态可见、AI 可驱（MCP）。
- 结构 §4.2。
- 本文档的**唯一事实源在仓内 `docs/DESIGN.md`**；`D:\KF\文件管理器设计\DESIGN.md` 只是历史副本，改了仓内那份才算数。

---

## 12. 变更台账

| 版本 | 日期 | 变更 |
|------|------|------|
| v1 | 2026-10-15 | 合并原 v1（架构/范围/论文/选型）+ v2（打开语义/Liquid Glass/Zexor 借避/仓库）。新增 §4.7 预览与状态、§5 视觉、§3.2 借避清单、§10 M3 预览+状态阶段 |
| v1.1 | 2026-10-15 | 文档收进仓内（`docs/DESIGN.md` 成为唯一事实源）；§11 落实远端 `bfxh/fmnr` + MIT。**里程碑实况：M0–M6 已完成并提交**（M6 分两笔：core 的 board 模型、UI 的白板视图），workspace 测试 168 passed / 0 failed / 2 ignored |

---

## 附录 A：事实核查台账

| 结论 | 来源 | 核实方式 |
|------|------|---------|
| Zexor 定位/功能/单EXE/仓库 | https://zex.leoog.com/ | WebFetch |
| Zexor 仓无源码 | api.github.com/repos/metalbug/Zexor | languages API 空 |
| Rust 本机 1.99.0 | `rustc/cargo --version` | 本机实测 |
| stable 预发布 0.100.0 | static.rust-lang.org channel-rust-stable.toml | HTTP |
| 本机 WSL 未启用 | `wsl -l -v` 无输出 | 本机实测 |
| iced 0.14.0 / slint 1.18.1 / notify 9.0.0-rc.5 / jwalk 0.9.0 | crates.io API | HTTP |
| Data Mountain 等论文 | microsoft.com/research / ACM / tandfonline | WebSearch |
| egui 选型、Liquid Glass 近似 | 本机 Rust GUI 经验 + 纯Rust自绘约束 | 推理（标注为判断，非实测） |
| iOS 26 Liquid Glass 令牌 | Apple HIG 公开材料 + 通用玻璃设计原则 | 推理（学术文献极少，见 §7.6） |

## 附录 B：诚实边界
- Zexor 实现细节**无法核实**（闭源），§3.2 借鉴清单基于官网宣称 + 通用 UX 常识，属**假设**，需 M1+ 评测验证。
- 论文结论为摘要级理解，落地应读全文。
- 「UV」经用户确认为「统一预览视图 + 让用户知道已被默认打开的 UX」，已落入 §4.7。
- Liquid Glass 为 egui acrylic **近似**，非像素级多层模糊。

### B.1 开发机资源约束（影响一切构建/测试命令）

构建机内存吃紧：**空闲内存常年只有 1.3–1.8 GB**，页面文件在 HDD 上（即便 `CARGO_TARGET_DIR`
已挪到 NVMe）。因此：

- 编译测试**一律用最小规格**：`CARGO_BUILD_JOBS=1 cargo test`（不要并发 `-j` 默认值）。
- 由此催生的一个**反模式**要避免：用 `for i in $(seq 1 N); do cargo test --workspace; done`
  反复跑全工作区来「抓偶发失败」。这么跑会因内存压力把机器拖死、且失败多为资源性
  flake 而非真 bug。要抓不确定性，用**定向重复单测**（`cargo test -p <crate> -- --exact <name>`）
  或 `cargo-nextest` 的重试，而不是整仓循环。
- 本约束来自开发期对话记录（Qoder 会话 `2026-10-15/cf9915da`），原始 5.2 MB 转写已清理；
  该会话同时确认：M0–M6 的实现级教训（`name_matches` 契约陷阱、`list` vs `search` 错误分类、
  角标 TTL 冒充存活的句柄泄漏）**均已落入代码与 commit message**，故不另存归档。
