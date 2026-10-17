# IS 贡献方案：scan + format conversion acceleration + small features

> 目标：把一批增量功能收进一次 is 提交，而不是拆成多轮 PR。
> 仓库名仍保留 fmnr；产品名称为「终」。
> 这是设计与实施方案，不是最终代码提交。落库后按「提交清单」逐文件实现。

---

## 0. 前置判断：仓库命名

推荐策略：
产品名：终
仓库名：fmnr

不建议在本次 is 提交里做全量重命名。
理由：
- fmnr 是已发表仓库名，README、GitHub Pages、MCP 协议注册表、客户端契约、CI、安装包名、锁等全部指向 fmnr。
- 改名会造成多个入口的连锁废弃。
- 改名本身不是功能，不适合和「scan + format conversion acceleration + small features」混在一起。

落地方式：
- README.md 顶部加一行：产品名：终 / 仓库名：fmnr。
- 发布物、安装名、客户端显示名可以叫「终」。
- 后续单独做发布名/包名/展示名升级.

---

## 1. 提交主题

is: scan + format conversion acceleration + small features

本次提交包含三大主题：
1. is 扫描：增量目录扫描、目录扫描缓存、搜索管线统一
2. 格式转换加速：归档卷预索引、归档成员预读取、减少重复解析和重复统计
3. 小功能合集：文件锁、磁盘占用实时增量、拖放复制进度、插件预览增强、协议注册表回滚强化

---

## 2. 变更文件

### 2.1 新建文件

- crates/fmnr-core/src/is_scan.rs：增量目录扫描
- crates/fmnr-core/src/scan_cache.rs：目录扫描缓存
- crates/fmnr-core/src/scan_opts.rs：统一扫描选项
- crates/fmnr-fs/src/scan_cache.rs：文件系统级目录缓存
- crates/fmnr-archive/src/archive_index.rs：归档预索引
- crates/fmnr-bench/src/bin/is_scan_micro.rs：is-scan 微基准
- crates/fmnr-bench/src/bin/archive_index_micro.rs：归档预索引微基准
- tests/platform/is_scan_test.rs：is-scan 集成测试
- tests/platform/archive_index_test.rs：归档索引测试
- tests/platform/scan_cache_test.rs：扫描缓存测试
- docs/IS-CONTRIBUTION.md：本文件

### 2.2 修改文件

- crates/fmnr-core/src/lib.rs：暴露 is_scan、scan_cache、scan_opts
- crates/fmnr-core/src/search.rs：统一搜索判定入口
- crates/fmnr-fs/src/ops.rs：增量扫描、文件锁、冲突命名
- crates/fmnr-fs/src/dupes.rs：重复文件缓存记住
- crates/fmnr-archive/src/archive_volume.rs：归档预索引
- crates/fmnr-ui/src/space.rs：磁盘占用实时增量
- crates/fmnr-cli/src/main.rs：--enumerate、批处理增强
- crates/fmnr-plugins/src/host.rs：插件预览、文件大小插件
- crates/fmnr-mount/src/lib.rs：协议登记清单
- crates/fmnr-ai/src/tools.rs：MCP 目录扫描与归档工具
- README.md：产品名、功能表

---

## 3. 待决策项到代码映射

下面每个条目都给出：问题、方案 A、方案 B、推荐方案、影响文件、下一步代码产物。

### 3.1 缓存是否持久化？

#### 3.1.1 问题
缓存是否落到磁盘，还是只在进程内短期保存？

#### 3.1.2 方案 A：进程内短期缓存
适合：
- 解决同一进程内重复扫描
- 不引入磁盘持久化
- 避免跨进程一致性问题

影响：
- crates/fmnr-core/src/scan_cache.rs
- crates/fmnr-fs/src/scan_cache.rs
- crates/fmnr-archive/src/archive_index.rs

下一步产物：
- 进程内缓存结构
- 缓存键
- 缓存值
- 命中/失效/清空 API
- 测试

缺点：
- 关闭进程后丢失
- CLI/MCP/GUI 之间不共享.

#### 3.1.3 方案 B：磁盘持久化
适合：
- 跨进程共享缓存
- 大包归档预索引复用
- 不同入口一致看到更快状态

影响：
- 额外新增缓存文件格式
- 缓存目录/文件大小
- 缓存版本
- 并发写
- 过期与清理

下一步产物：
- 缓存文件格式
- 缓存目录管理
- 读写 API
- 容量上限
- 老化策略
- 测试

缺点：
- 实现复杂
- 需要处理磁盘空间和并发.

#### 3.1.4 推荐方案
第一阶段做进程内短期缓存。

原因是：当前提交核心目标是减少同一进程内重复全量扫描、避免 UI 阻塞、让 CLI/目录枚举更快。不要一开始引入复杂磁盘缓存。

后续如果需要，再独立做持久化层.

---

### 3.2 文件锁语义：共享读 / 独占写 / 关闭后自动释放？

#### 3.2.1 问题
文件锁要做成多强？

#### 3.2.2 方案 A：短生命周期、非阻塞式
适合：
- 文件管理器
- 预览、复制、移动、重命名
- 避免用户等待
- 不阻塞 UI

行为：
- 锁只在操作短命
- 外部写返回可写/不可写/锁定状态
- 取消立即释放

影响：
- crates/fmnr-fs/src/ops.rs
- crates/fmnr-ui/src/preview.rs
- crates/fmnr-ui/src/space.rs
- crates/fmnr-cli/src/main.rs

下一步产物：
- 短生命周期锁 API
- 锁状态语义
- 取消释放
- 测试

缺点：
- 不强制阻止外部写，要靠策略和提示.

#### 3.2.3 方案 B：强连接锁
适合：
- 需要严格防止并发编辑
- 特定业务场景

影响：
- 更复杂的平台 API
- 更多生命周期管理
- 可能误伤同源进程

缺点：
- 复杂度高
- 用户体验风险大.

#### 3.2.4 推荐方案
第一阶段做短生命周期、非阻塞式文件锁。

含义：
- 预览不锁整个文件
- 复制/移动/重命名短暂锁定元数据或目标目录
- 外部写返回明确状态，而不是静默阻塞.

---

### 3.3 --enumerate 是否开放给 MCP/CLI？

#### 3.3.1 问题
是否要把递增目录枚举暴露给 MCP？

#### 3.3.2 方案 A：只做 CLI 命令
当前最小可交付：
fmnr enumerate --root <dir> --depth 3 --dir-only

优点：
- 实现简单
- 直接验证
- 减少 MCP 契约变更

影响：
- crates/fmnr-cli/src/main.rs
- crates/fmnr-core/src/is_scan.rs

缺点：
- 顶层命令可用，但 AI 客户端未直接接入.

#### 3.3.3 方案 B：同时开放 MCP
需要新增 MCP 工具：enumerate / scan / list_dir

优点：
- AI 客户端可直接用

影响：
- crates/fmnr-ai/src/tools.rs
- crates/fmnr-core/src/lib.rs
- 更多契约测试

缺点：
- 需要防止大目录返回超大数据
- 需要输出裁剪和缓存控制.

#### 3.3.4 推荐方案
第一阶段做 CLI + 核心库暴露。先保证 fmnr enumerate --root <dir> --depth N --dir-only 能稳定输出。后续根据使用反馈再决定 MCP 工具是否接入.

---

### 3.4 大文件预览阈值？

#### 3.4.1 问题
多大文件才进入分层预览？

#### 3.4.2 推荐数值
建议：small_file_limit = 4 MiB

含义：
- 小文件：可以完整预览
- 大文件：返回元数据、预览部分、或禁止预览

影响：
- crates/fmnr-ui/src/preview.rs
- crates/fmnr-archive/src/archive_volume.rs
- crates/fmnr-fs/src/ops.rs
- crates/fmnr-plugins/src/host.rs

#### 3.4.3 预览行为
- 文本文件：超限时只读前固定字节
- 图片文件：缩放预览，不解码全图
- 大二进制文件：只显示元数据，禁止预览
- 归档内文本：防止解压全包

---

### 3.5 归档写入方式？

#### 3.5.1 问题
归档卷要不要支持写入？

#### 3.5.2 推荐方案
第一阶段做：
- 归档只读
- 归档内写入失败
- 需要导出时导出到临时目录

影响：
- crates/fmnr-archive/src/archive_volume.rs
- crates/fmnr-plugins/src/host.rs
- crates/fmnr-ui/src/preview.rs
- crates/fmnr-cli/src/main.rs

后续可扩展：
- 虚拟文件系统写入
- 归档内编辑
- 归档变更写入.

---

## 4. 核心设计

### 4.1 is-scan：增量目录扫描

#### 4.1.1 现状问题
- 全量 read_dir + metadata，大目录一致阻塞。
- 无缓存时每次都会重新扫描。
- 不同入口（GUI、CLI、MCP）各自实现搜索/扫描，结果不一致。
- 归档、WSL、远程、本地之间扫描策略不统一。

#### 4.1.2 设计
统一扫描选项 VolumeScanOpts：
- depth：目录深度
- include_hidden / include_system / include_ignored：过滤开关
- max_entries：最大条目数
- progress：进度回调

目录缓存 ScanCacheKey：
- volume_id + path + flags + depth + 三个过滤开关

ScanCacheValue：
- entries / bytes / file_count / dir_count / started_at / finished_at

缓存失效：
- 缓存命中直接返回，未命中才执行 IO
- 目录变化时失效
- scan_opts 变化时失效
- ephemeral 不持久化；persistent 可写入磁盘缓存

语义要求：
- dir_stats() 的 (bytes, files) 语义不变
- 缓存不能隐藏目录内容变化
- 深度限制必须生效
- 取消扫描必须释放资源

---

### 4.2 格式转换加速：归档预索引

#### 4.2.1 目标
不是把归档全量解压，而是让归档列表、stat、预览更快。

#### 4.2.2 核心类型
ArchiveIndex：
- archive_path / member_count / total_bytes / index_version / entries

ArchiveEntryIndex：
- name / is_dir / size / offset / compressed_size / compression_type / modified

行为：
- 首次打开归档时建立索引
- 只索引元数据，不解压内容
- 后续 list_dir、stat、preview 从缓存读取
- 归档只读，不引入外部 7z 解压
- 归档内写操作失败或改为导出到临时目录

---

### 4.3 搜索管线统一

统一判定入口：
- name_matches(name, needle, case_sensitive)
- path_matches(path, needle, case_sensitive)
- extension_matches(extension, needle, case_sensitive)

统一卷入口：LocalVolume / ArchiveVolume / RemoteVolume / WslVolume

统一输出：entries / total_bytes / file_count / dir_count / metadata

---

## 5. 小功能

### 5.1 文件锁
短生命周期、非阻塞式。语义：短锁、超时释放、取消释放、外部写可拒绝/警告/协商、不阻塞读取。

### 5.2 磁盘占用实时增量
升级现有磁盘占用视图：支持增量扫描、缓存未命中自动刷新、大目录不阻塞 UI、可取消、数据不丢失。

### 5.3 拖放复制进度
统一复用 batch 的进度抽象：单文件复制、多文件复制、移动、删除、拖放、归档加载。
必须有进度、取消、失败后不中断整个批次的语义。

### 5.4 插件预览增强
新增：file_size 插件、目录大纲插件增强、归档内预览插件。
插件能力表 PluginCapability { name, version, capabilities }。
插件边界：只能访问文件名、扩展名、属性、路径、尺寸；不直接获取进程内文件句柄；预览只读；升级需版本校验。

### 5.5 协议注册表回滚
fmnr:// 注册在 HKCU 下，建立可逆清单：注册项、当前值、源值、取消顺序、失败处理。
每条注册项带 id 与 source；取消一个项不影响其他项；失败不静默丢弃。

---

## 6. 验证门禁

### 6.1 提交前必跑
    cargo fmt --check
    python -X utf8 scripts/god_gate.py --root .
    python -X utf8 scripts/args_gate.py
    cargo test --workspace

### 6.2 增量相关
    cargo test -p fmnr-core --test is_scan_test
    cargo test -p fmnr-fs --test scan_cache_test
    cargo test -p fmnr-archive --test archive_index_test

### 6.3 插件
    cargo test -p fmnr-plugins --test real_plugin_e2e

### 6.4 微基准
    cargo run -p fmnr-bench --bin is_scan_micro
    cargo run -p fmnr-bench --bin archive_index_micro

---

## 7. 风险清单

### 7.1 缓存复杂度
风险：缓存键不完整、目录变化后没失效、不同入口拿到不同缓存、大目录缓存过大。
防御：缓存键包含 volume_id/path/flags/depth/三个过滤开关；目录结构、父目录、权限、文件变更都要失效；增加回归测试。

### 7.2 归档预索引成本
风险：首次打开变慢、大包卡顿、索引全量解压。
防御：只索引元数据不解压内容；索引失败不阻塞后续列表；区分首次索引与缓存命中；对大包做基准测试。

### 7.3 文件锁
风险：锁太久、锁阻塞 UI、锁误伤同源进程。
防御：锁短生命周期；取消立即释放；外部写给明确状态；不做成强连接锁除非必要。

### 7.4 网络卷未实测
风险：SSH/SFTP 接口通过但真实主机不满足、错误分类不准、弱连接下流式读取失败。
防御：新代码做单测；真机验证标记为 unverified；不把接口通过等同于真实可用。

---

## 8. 待决策项（汇总）

1. 缓存是否持久化？→ 第一阶段进程内短期缓存；后续若需跨进程共享再单独加持久化层。
2. 文件锁语义？→ 第一阶段短生命周期、非阻塞式；业务需要再扩展强连接锁。
3. --enumerate 是否开放 MCP/CLI？→ 第一阶段 CLI + 核心库；后续再决定 MCP 工具接入。
4. 大文件预览阈值？→ 默认 4 MiB；超大文件给元数据、部分预览或禁止预览。
5. 归档写入方式？→ 第一阶段只读，需导出时导出到临时目录；后续再扩展虚拟写入。
6. 产品名「终」是发布名还是安装名？→ 当前只在 README 标注产品名，不改代码包名，发布策略变化时另起 PR。

---

## 9. 提交清单

### 新建
    crates/fmnr-core/src/is_scan.rs
    crates/fmnr-core/src/scan_cache.rs
    crates/fmnr-core/src/scan_opts.rs
    crates/fmnr-fs/src/scan_cache.rs
    crates/fmnr-archive/src/archive_index.rs
    crates/fmnr-bench/src/bin/is_scan_micro.rs
    crates/fmnr-bench/src/bin/archive_index_micro.rs
    tests/platform/is_scan_test.rs
    tests/platform/archive_index_test.rs
    tests/platform/scan_cache_test.rs

### 修改
    crates/fmnr-core/src/lib.rs
    crates/fmnr-core/src/search.rs
    crates/fmnr-fs/src/ops.rs
    crates/fmnr-fs/src/dupes.rs
    crates/fmnr-archive/src/archive_volume.rs
    crates/fmnr-ui/src/space.rs
    crates/fmnr-cli/src/main.rs
    crates/fmnr-plugins/src/host.rs
    crates/fmnr-mount/src/lib.rs
    crates/fmnr-ai/src/tools.rs
    README.md

### 文档
    docs/DESIGN.md
    docs/IS-CONTRIBUTION.md
    README.md

---

## 10. 提交消息建议

主标题：is: scan + format conversion acceleration + small features
副标题：Incremental directory scan, archive pre-indexing, and small features.

---

## 11. 推荐工作流

    git checkout -b feat/is-scan-format-accel
    cargo fmt --check
    python -X utf8 scripts/god_gate.py --root . --fast
    python -X utf8 scripts/args_gate.py --fast
    cargo test --workspace
    cargo run -p fmnr-bench --bin is_scan_micro
    cargo run -p fmnr-bench --bin archive_index_micro

---

## 12. 后续扩展（非本提交必须）

1. 目录树缓存 + 版本化
2. 搜索结果跨卷编号
3. 归档卷内容预览
4. 磁盘占用面板增量
5. MCP 目录扫描能力
6. 慢路径指标
7. 插件市场和插件修复通道

---

## 13. 证据来源

- README.md：里程碑、CLI、MCP、功能表
- docs/DESIGN.md：唯一事实源
- docs/UI-NEXT.md：缺口清单
- docs/NEXT-BATCH.md：未来方向与未做项
- git log：近期提交历史和子系统趋势
- crates/：12 个 crate
- crates/fmnr-fs/src/ops.rs：dir_stats()、冲突命名器
- crates/fmnr-core/src/search.rs：name_matches()
- crates/fmnr-archive/src：归档只读挂载
- crates/fmnr-ai/src：MCP stdio + HTTP
- crates/fmnr-cli：CLI 入口
- crates/fmnr-ui：egui GUI

---

*文档完善时间：2026-10-17*