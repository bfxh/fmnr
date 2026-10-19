# fmnr — 一个独立的 Rust 文件管理器

> **产品名：终**（发布物/安装名/客户端显示名用「终」）/ 仓库名：`fmnr`
> file manager in rust

> 命名说明：仓库、crate、二进制、`fmnr://` 协议字面量一律保留 `fmnr`，
> 全量重命名推迟到发布范围确定后单独做（发布压缩包名也保留 `fmnr-<版本>-<平台>.zip`：非 ASCII 文件名在下载地址里会被百分号编码，脚本和 `SHA256SUMS.txt` 都容易踩坑）。

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
| CLI | `fmnr ls/search/cp/stat/apps/volumes/register`，人也能直接敲；`fmnr batch` 吃 NDJSON 一次跑完一批 |
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
cargo run -p fmnr-cli -- stat .\Cargo.toml      # 属性：类型/大小/时间/隐藏·系统·只读
cargo run -p fmnr-cli -- apps --limit 5         # 已装程序底账（只列，不含卸载命令行）
cargo run -p fmnr-cli -- volumes
# 一批命令共享一次挂载扫描：一行一条请求，一行一条结果
printf '%s\n' '{"tool":"create_dir","args":{"path":"out"}}' \
              '{"tool":"write_file","args":{"path":"out/a.txt","text":"hi"}}' \
  | cargo run -p fmnr-cli -- batch
cargo run -p fmnr-cli -- register --dry-run   # 先看计划，不写注册表
```

MCP：`cargo run -p fmnr-ai` 走 stdio；加 `--serve 127.0.0.1:PORT` 走 HTTP。

---

## 拿现成的二进制

不想自己编：去 [Releases](../../releases) 下对应平台的 zip
（`fmnr-<版本>-windows-x86_64.zip` / `fmnr-<版本>-linux-x86_64.zip`），解压即用，
`SHA256SUMS.txt` 里是校验和。zip 里三个可执行文件加一份 `RUNNING.md`，
**装完怎么跑、插件放哪、MCP 怎么配**都写在那份里（源文件在
[`packaging/RUNNING.md`](packaging/RUNNING.md)）。

发版流程：改 `Cargo.toml` 的 `version` → 在 [`CHANGELOG.md`](CHANGELOG.md) 补一节
`## <那个版本>` → 打 tag `v<那个版本>` 推上去。CI 会核对这三者一致，
然后编两个平台的 release 产物并建 Release；任何一步对不上就判红而不是发一个错版本。

> **这套流程本身还没真跑过一次**：`release.yml` 只在 `v*` tag 上触发，
> 目前仓库里没有 tag，所以除了 YAML 解析与每个 `run` 块的 `bash -n`，
> zip / sha256 / 建 release / 回读附件这几步**没有被执行验证过**。
> 第一次打 tag 时请盯着那一条流水线，别把"文件都提交了"当成"发布能跑"。

---

## 工作区结构

```
crates/fmnr-core     核心模型：Volume trait / Entry / 打开决策 / 进度 / Board / WSL 路径 / 跨卷搬运
crates/fmnr-fs       本地卷实现：ops（复制移动删除）、assoc（默认程序注册与回滚）
crates/fmnr-archive  归档只读卷：zip / tar / tar.gz / 7z，进卷时建条目索引、内容按需读
crates/fmnr-remote   SSH/SFTP 卷（接口层，未真机验证）
crates/fmnr-icons    图标管线：内置 SVG 集 → 光栅化 → egui 纹理，自绘
crates/fmnr-plugins  WASM 宿主（wasmi）+ 插件注册表，可以做预览和图标插件
crates/fmnr-ui       eframe/egui：列表 / 预览 / 白板 / 主题
crates/fmnr-ai       MCP stdio + HTTP，与 CLI 共用同一张 dispatch 表
crates/fmnr-cli      命令行入口
crates/fmnr-bench    基准
plugins/md-outline   示例真 Rust WASM 插件（Markdown 大纲预览）
```

设计文档见 [`docs/DESIGN.md`](docs/DESIGN.md)（唯一事实源）。
界面的**实测记录与机制**见 [`docs/UI-NEXT.md`](docs/UI-NEXT.md)
（为什么这么设计、当时量到多少、哪条路被证否；交付结论看 CHANGELOG）。

[`docs/NEXT-BATCH.md`](docs/NEXT-BATCH.md)：跨设备连接、7z 与"比它更强"、重复文件、
加密与文件锁、卸载残留、MCP 是不是瓶颈——每条标了一手来源/推断/未找到，
并写明哪三条我不同意提法本身（§6 尤其）。

[`docs/BUG-SCAN.md`](docs/BUG-SCAN.md)：用本地工具在独立沙盒里扫 bug 的记录——
确证并修掉的、被实测推翻的、以及**证否**（跑了发现没有）的，都留了可复算的判据。

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
| M7a | 归档只读卷（zip/tar/tgz/7z）+ WSL UNC 识别 + wslpath | ✅ 内核层**真机实测**；UI 侧见 M8 |
| M7b | SSH/SFTP 卷 | ⚠️ 只有接口与纯逻辑测试，**未连过真实主机** |
| M9 | `is`：增量扫描 + 归档预索引 + 小功能合集 | ❌ **撤案**（2026-10-19）：方案文档点名的 10 个文件一个没建过、文件地图与仓内布局不符，已删；两条还活着的口子（增量目录扫描 / 扫描缓存）挪进 `docs/NEXT-BATCH.md` §21 |
| M8 | 卷注册表 + 挂载入口（让用户真的能打开 zip / 切到 WSL / 连 SSH） | ⚠️ 入口在 UI 里；**挂 zip 实测通过**（对话框填路径→挂上→两栏列出包内内容）；**挂 WSL 实测通过**（WSL 页发现到 `Ubuntu-24.04`（来自 `wsl -l -q`）→ 挂上 → 顶栏变 `wsl:Ubuntu-24.04`、两栏与目录树都列出该发行版的根）；**SSH 那条没实测**（本机没有可连的真实主机） |
| 拖拽 | 应用内两栏拖放搬运（含原地/自嵌套预检、Ctrl=复制） | ✅ 四条落地路径 computer-use 点过并 `ls` 对账；**Ctrl+drag 未点验**（自动化按不住修饰键） |
| 包更新 | 「包管理器」面板：可升级（winget）+ 已装清单（注册表底账）+ 详情 + 拖入快捷方式认应用；**浏览到某程序的目录时栏顶给一行弱提示** | ✅ 本机真跑：可升级 11 项、底账 273 条（与独立计数一致）、详情/过滤/复制命令点过；打开面板就同时装载两份数据；**不代跑升级**（提示里也只复制 `winget upgrade --id …`）。目录归属判定层拿真注册表自洽跑过（67/67 认出自己、放弃 0 条），**但那一行提示的渲染没在真窗口看到过**，见下面「已知边界」。GitHub 上游只认出仓名，联网查询未接。**拖入的 `.lnk` 认目标**：230 个本机真件对质 Windows 自己的 oracle，176 条一致、4 条是别人机器上的件、50 条（没有 LinkInfo）明说"没有写出目标路径"而不是猜，机制见 `docs/BUG-SCAN.md` §5 |
| 设置 | 顶栏「⚙ 设置」：开机自启动开关（HKCU `Run\fmnr`，可逆）、显示隐藏项、本机卷/插件计数 | ✅ 真机往返点过：开→注册表多出 1 个值、关→回到原样且**别人的 9 个自启动项一个没动** |
| 三入口一致 | 内核能力上表：`read_file` / `write_file` / `rename` / `create_dir` / `stat` / `packages`（CLI 对应 `cat` / `put` / `rename` / `mkdir` / `stat` / `apps`） | ✅ `entry_parity.rs` 三条腿（MCP 真子进程 / HTTP 真套接字 / CLI 真二进制）逐步比对返回 JSON **整份相等**；反证打过（CLI 腿偷加 `--max-bytes 3` 立刻判红；把 CLI 的 `stat` 改指 `ls` 也立刻判红）；`stat` 真跑：只读+隐藏文件回 `["ReadOnly","Hidden"]`、普通文件回 `[]`；`apps` 真跑：本机 273 条底账、**不回显卸载命令行**；**§6.2「MCP/HTTP 接多卷」未做**，见 `docs/NEXT-BATCH.md` §11 |
| 批量入口 | `fmnr batch`：stdin 一行一条 `{"tool":…,"args":…}`，stdout 一行一条结果，**共享一次挂载扫描** | ✅ 本机量过：20 条命令 0.813 s → 一次 batch 0.045 s（**18.2 倍**）；一行失败不中断整批（改成遇错就退，测试拿到 3 行而不是 5 行）；返回体带 `line` 对齐输入物理行号，空行不回话 |
| 默认程序 | 「用默认程序打开」解析系统真值（UserChoice → Classes → shell\open\command），只读不写 | ✅ 本机 `.txt` 解析到 WindowsApps 里的 Notepad 真路径并成功起进程；`fmnr open --external` 不传 program 即走这条 |
| 重复文件 | 阶梯筛选 + **字节比对才算判定**；「磁盘占用」面板里扫、勾、**只删副本（进回收站）** | ✅ 真跑：`a/b/c.bin` 同内容扫成一组、默认留第一个、取消某项后按钮计数跟着变、删完清单重算且**保住用户其余的勾**；`b.bin` 在回收站里查到（删前 0 项）；9 条测试含"喂恒等碰撞哈希也不许把不等内容成组"；四元组指纹缓存，"不记住"是真开关（ephemeral 连落盘都拒绝） |
| 多卷寻址 | MCP/HTTP 每次调用按参数选卷（与 CLI 共用 `fmnr-core` 那一份规则），空表明确报错而不是列空 | ✅ 探针卷测试钉住"选错卷立刻显形"（第一版用两个本地卷的测试把路由摘掉照样过，已作废重写）；**独立 MCP 进程仍挂不了 zip**，那是依赖决定 |
| 子树搜索 | 归档/远程卷走 `Volume::list` 逐层遍历（本地仍走 jwalk），根读不了说成"搜不动"而不是 0 命中 | ✅ 归档卷真跑：包内 `inner/deep-readme-4471.md` 搜 `deep` 命中（1 结果 · 1 ms）；**远程卷未实测**（本机无 SSH 目标） |
| 跨卷互传 | 源卷 `read_stream` → 目标卷 `write_stream`，一块一次写、两边都不整份进内存；目标是**另一个卷**时（带 `local:` 前缀或一条裸绝对路径）走这条 | ✅ 真 CLI 进程：`fmnr cp archive:D:\tmp\e2e\pack.7z/hello.txt --to D:\tmp\e2e\out` → `done:1 · bytes:9`、盘上文件逐字节相等（修前这条报「归档卷是只读的」，四条入口都取不出包内文件）。🔴 **只搬文件**（整目录有 §21-2 那条 O(N×D) 禁区，加上本地卷的 `search` 只按"名字以 `.` 开头"跳隐藏项这一条前置），**界面里的跨栏拖放仍走单卷** |
| 特殊文件夹 | 目录树起点下挂 下载/文档/图片/桌面（走系统已知文件夹，不拼 `home\Downloads`） | ✅ 真跑点过：点「下载」→ 栏头/面包屑/状态栏三处都是 `C:\Users\lbx13\Downloads`；被重定向的文件夹也拿得到真实位置 |
| 路径串 | 面包屑/栏头与列出的内容必须指向同一个目录 | ✅ 已确诊并修：旧面包屑按 `components()` 取前 N 个重拼，Windows 上第一跳给的是 `E:`（盘相对，按该盘 CWD 解析）。现在标签与目标同源走 `ancestors()`，枚举层 `LocalVolume::resolve` 把 `E:` 补成 `E:\`（MCP/HTTP/CLI 那条腿客户端可以直接传 `"path":"E:"`）。真跑验过：`E:\tmp\adv-mut` 点「tmp」→ 栏头 `E:\tmp`、列的是 tmp 的内容。机制与实测见 `docs/UI-NEXT.md` §10 |
| CI | 五道门禁（lint / 双平台测试 / 插件行为 / 插件字节可复现 / 客户端契约） | ✅ `85f8ba9`（run 38979552946）全绿 |

本地跑门用 [`scripts/gate.sh`](scripts/gate.sh)：按 CI 的顺序跑，最后一步
把**索引**物化成干净的树再查 fmt——因为本轮真实踩过一次"本地全绿但 CI 红"，
起因是 `git add` 跑在 `cargo fmt` 之前，门判的是工作树而提交的是索引。

## 已知边界

写在这里的是**判据边界**，不是谦虚：

- 🔴 **归档卷：界面里挂得上、列得出，但只读面没测。**
  `fmnr-ui` 启动时走 `fmnr_mount::mount_plan`（与 CLI 同一套受测逻辑），
  卷下拉列的是 `VolumeRegistry` 的真实内容，「📦 挂载」开的是
  归档/WSL/SSH 三选一的对话框。**实测到的**：启动挂上本地卷、
  卷下拉显示 `local:E:\`、切卷会把两栏复位到该卷起点、
  **挂一个 zip → 顶栏变成 `archive:…\sample.zip`、两栏列出包内目录与文件**。
  **没实测的**：在归档卷里双击一个文件（预览/外置打开的语义）、
  往只读卷粘贴会不会被拒得干净、SSH 那条挂载路径。
  🔴 **7z 到"真进程"为止，没到"界面"为止**：门是拿真 7-Zip 26.00 写出的字节建的（挂上、列出、
  读成员、加密件判红），另外用 `fmnr volumes / ls / cat` 在真 CLI 进程里端到端点过一遍；
  `fmnr-ui` 这边只改了挂载对话框那句写错的标签（`tar.bz2`→`7z`，**没在真窗口拍过图**），
  挂载逻辑一行没动，**界面里从没挂过 `.7z`**。
  别把"挂上了"读成"整条链都能用"。
  **WSL 挂载这一轮补测过了**：「📦 挂载」→ WSL 页发现到 `Ubuntu-24.04`
  （列表来自 `wsl -l -q`）→ 挂上 → 顶栏变 `wsl:Ubuntu-24.04`、
  两栏与目录树都列出该发行版的根。它和"在本地卷里直接进
  `\\wsl.localhost\<distro>\…`"是同一条底层路径（UNC 本来就可 `read_dir`），
  差别是入口现在看得见。
- **跨卷互传到"真进程"为止，界面没接。** 在 `D:\tmp\e2e` 里起真 CLI 进程实测到的：
  `fmnr cp archive:D:\tmp\e2e\pack.7z/hello.txt --to D:\tmp\e2e\final` → `{done:1, bytes:9}`，
  盘上 9 字节逐字节等于 `hello 7z\n`；嵌套成员 `dir/deep.bin` → 落 10 字节 `ABCDEFGHIJ`；
  `fmnr search <那个件> deep` 吐出的那条 `archive:…!/dir/deep.bin` 原样喂给 `fmnr cat`
  读回同样 10 字节（这就是 `!/` 那一格的验收）。
  🔴 一句别读错的证据：往只读卷拷（`cp <本地文件> --to archive:…`）时 CLI 打的
  「归档卷是只读的：1 → archive:…」是**源卷自己的 `copy`** 报的（那次选卷把归档卷选成了源卷），
  不是 `copy_across` 那句「目标卷 X 是只读的」——两条消息都说清了只读，但别拿这条 CLI 输出
  当 `copy_across` 的实测证据（那一格在 `fmnr-ai/tests/cross_volume_dispatch.rs` 里测）。
  **没实测的**：WSL / SSH 卷之间的搬运（没有可连主机）、界面里的跨栏拖放（仍走单卷那条 `ops::spawn_op`）。
- **两栏只能看已挂载的那个卷。** 面包屑最远到盘根，但目录树只挂当前 home 的子树，
  「📦 挂载」只给 归档 / WSL / SSH 三项——**没有"挂另一块本地盘"**。
  所以从 `local:C:\Users\…` 出发，走到 `C:\Program Files (x86)\…` 这种同机绝对路径
  在界面上做不到（`LocalVolume::resolve` 其实收绝对路径，是**没有入口**）。
- **栏顶那条"这个目录有新版可升"的弱提示：判定层验过、像素没看过。**
  `fmnr-pkg::owner` 拿真注册表自洽跑过（本机 74 条写了安装位置、67 条目录还在，
  **67 条全部认出自己、主动放弃 0 条**），底账自动装载也在真窗口里看到过
  （打开包管理器就是 `可升级 11｜已装清单 273`）。但**提示那一行本身没渲染出来过**：
  要让它出现得走进某个"登记了安装位置又在可升级清单里"的目录，
  而这些目录要么在受保护的盘外目录、要么被上一条那个缺口挡住。别当已验收。
- **界面这一轮是"跑起来看"过的，不是只编译过。** 实测通过的路径：
  中文渲染（装 CJK 回退字体）、两栏各自导航与面包屑、行内过滤 + 退格只删字符
  不再让对栏跳上级、单击/双击行、搜索面板（按钮能打开、输入即搜、命中高亮、
  单击定位）、F2 重命名（回车提交、只改名时扩展名保留）、新建目录对话框抢焦点。
  **没测过的**：白板视图在本轮改动后的往返、拖放、冲突弹窗、复制/移动进度、
  插件预览、HTTP/MCP 面。
- **SSH/SFTP 卷没有真实验证过。** 端点解析、host key 决策（fail-closed）、
  SFTP 状态码→错误分类这三块有单测；握手、认证成功率、流式读、断连恢复**没有**。
  `ssh_live.rs` 在没有 `FMNR_SSH_TEST_HOST` 时打印跳过并通过——
  所以那一片绿灯不代表远程卷可用。
- **WSL 是真机实测过的**（开发机装有 WSL2：`Ubuntu-24.04`、`docker-desktop`）：
  两种 UNC 写法都能直接 `read_dir`，`wslpath` 双向可用，
  所以浏览 WSL **不需要协议层**，复用本地卷即可。
  仍未实测的只有「未安装 WSL」那条错误文案，因此代码里不按文案判可用性，
  只看退出码。
- **远程卷与归档卷都不做 hash 图标覆盖**（取 hash 的代价与本地不在一个量级）。
- **归档卷只读**：进卷时一次建好条目索引（zip 读中央目录；tar/tar.gz 要顺序扫过头块，gz 还要整条 inflate；
  7z 读头表——名字/大小/时间/属性都在头里，成员内容按需才解压），
  成员内容按需才读——**列目录不解压**，但建索引这一步不是懒的（实测数字见 `docs/NEXT-BATCH.md` §21-2）。
- **7z 只解主流编码**：`sevenz-rust2` 以 `default-features = false` 引入，能解 COPY/LZMA/LZMA2/BCJ/DELTA；
  AES（加密件）与 bzip2/PPMd/deflate/lz4/zstd 的特征**没开**，撞上时明确报错、不挂一个看着像空的卷。
  这一版也没有输口令的入口，所以加密件是"读不了"而不是"要密码"。
- Liquid Glass 是 egui 上的 acrylic **近似**，不是像素级多层模糊。
- 不做：音视频转码（不内嵌 FFmpeg）、文件同步、云盘挂载、系统级 Shell 扩展。

## License

MIT
