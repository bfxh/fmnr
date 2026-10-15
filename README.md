# fmnr — 一个独立的 Rust 文件管理器

> file manager in rust

不接管系统、不依赖 Explorer 的文件管理器。三个跟别人不一样的卖点：

### 1. Liquid Glass 视觉

自绘文件图标 + 玻璃拟态外壳（胶囊体控件、acrylic 磨砂、令牌化换肤）。
**图标完全自己画**（`resvg` → `tiny-skia` 光栅化内置 SVG），不去读系统 shell 图标，
所以任何一个扩展名的图标都能随手改。

### 2. 统一预览 + 状态可见（UV）

双击的行为分两类，而且**都看得见**：

- 能内建的（文本 / 代码 / 图片 / 目录大纲插件）→ 右侧统一预览面板直接给你看，不弹窗、不丢给系统。
- 交给默认程序的 → **列表上挂一个角标**，并且这个角标跟着**真实进程存活**（进程退出，角标自己消失）。

这是本项目最核心的一条 UX 主张：不要让人猜「我的文件现在到底在哪、被谁打开了」。

### 3. AI 可驱（MCP）

同一张 dispatch 表同时服务四种入口，行为保证一致：

| 入口 | 用途 |
|---|---|
| MCP stdio | 给 Claude / Cursor 之类的 AI 客户端调 `list_dir` / `search` / `open` / `copy` |
| HTTP (axum) | 本机 HTTP API，默认只允许回环地址 |
| CLI | `fmnr ls/search/cp/volumes/register`，人也能直接敲 |
| `fmnr://` 协议 | 从浏览器/别的软件点回来，注册在 HKCU 下且可完整回滚 |

### 4. 白板（Board）

把文件钉到一块无限画布上按位置摆放，坐标就是用户的空间记忆。
**刻意不做自动排布**：网格对齐、自动避让这类"智能整理"会把用户摆的位置冲掉，
所以白板只在你亲手拖动时改变布局。缺失的文件（拔盘、断网）只标记不删除。

---

## 构建与运行

需要 **Rust 1.99+**（用当前 stable）。

```sh
cargo build --workspace            # 全量构建
cargo run -p fmnr-ui               # 起 GUI
cargo test --workspace             # 全量测试
```

> `fmnr-fs` 有两个测试带 `#[ignore]`，它们会**动到你真实的回收站**，只在手动复核时才跑：
> ```sh
> cargo test -p fmnr-fs -- --ignored
> ```

CLI 试一下：

```sh
cargo run -p fmnr-cli -- ls .
cargo run -p fmnr-cli -- search --root . --query rs
cargo run -p fmnr-cli -- volumes
cargo run -p fmnr-cli -- register --dry-run   # 先看计划，不写注册表
```

MCP：`cargo run -p fmnr-ai` 走 stdio；加 `--serve 127.0.0.1:PORT` 走 HTTP。

---

## 工作区结构

```
crates/fmnr-core     核心模型：Volume trait / Entry / 打开决策 / 进度 / Board
crates/fmnr-fs       本地卷实现：ops（复制移动删除）、assoc（默认程序注册与回滚）
crates/fmnr-icons    图标管线：内置 SVG 集 → 光栅化 → egui 纹理，自绘
crates/fmnr-plugins  WASM 宿主（wasmi）+ 插件注册表，可以做预览和图标插件
crates/fmnr-ui       eframe/egui：列表 / 预览 / 白板 / 主题
crates/fmnr-ai       MCP stdio + HTTP，与 CLI 共用同一张 dispatch 表
crates/fmnr-cli      命令行入口
crates/fmnr-bench    基准
plugins/md-outline   示例真 Rust WASM 插件（Markdown 大纲预览）
```

设计文档见 [`docs/DESIGN.md`](docs/DESIGN.md)（唯一事实源）。

---

## 里程碑

| 阶段 | 内容 | 状态 |
|---|---|---|
| M0 | 骨架 + 本地目录浏览 | ✅ |
| M1 | 图标管线 + Liquid Glass 基调 | ✅ |
| M2 | 搜索 + 文件操作 + 真实进程角标 | ✅ |
| M3 | 统一预览面板 | ✅ |
| M4 | MCP / HTTP / CLI / `fmnr://` / 拖放 | ✅ |
| M5 | WASM 插件宿主 + 真插件验收 | ✅ |
| M6 | 白板布局 + 持久化 + 恢复 | ✅ |
| M7 | SSH/SFTP 卷 + WSL UNC 识别 + wslpath | ⬜ |

## 已知边界

- **M7 未开始。**
- WSL 支持只会做到「实现 + 契约测试」：开发机没启用 WSL，所以**没有真机验证**过。
- Liquid Glass 是 egui 上的 acrylic **近似**，不是像素级多层模糊。
- 不做：音视频转码（不内嵌 FFmpeg）、文件同步、云盘挂载。

## License

MIT
