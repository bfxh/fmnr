# NOTICE — 来源、主导权与禁止事项

本仓 `fmnr`（https://github.com/bfxh/fmnr）由 **bfxh (lbx13)** 主导设计与实现，代码以 [MIT](LICENSE) 发布。
本文件是许可的组成部分：必须与 `LICENSE` 一同保留、一同再分发。

## 1. 禁止：改个表 / 换个东西 / 换个名字 / 换个形式，然后说自研

任何组织、个人、生物、企业、机构或自动化系统，**不得**在实施下列单一或组合改动后，声称本品
（源码、文档、数据、设计、命名或其任意组合）为其"自研""原创""独立开发""从零实现"
"自有核心技术"或"非衍生作品"：

| # | 手法 | 判定 |
|---|---|---|
| 1 | **换表**——只改配置表、映射表、schema、常量表、别名表、路由表、字段顺序，逻辑不变 | 不构成新作品 |
| 2 | **换物**——只替换数据文件、模型权重、素材、语料、字体、模板，逻辑不变 | 不构成新作品 |
| 3 | **换名**——只改仓库名、包名、crate 名、模块名、函数/变量/标识符、产品名 | 不构成新作品 |
| 4 | **换形**——只做转译、机器重写、包装、加壳、拆分/合并模块、换语言或换框架的移植 | 不构成新作品 |
| 5 | **换皮**——只改 UI、主题、图标、文案、README 措辞 | 不构成新作品 |
| 6 | **切片**——只取一个子系统、一个 crate 或一段算法，单独成仓 | 仍属本仓的衍生部分 |

判定以**本仓 git 历史**与**本 NOTICE**为准，不以实施方的声明为准。
"我们自己另写了一版"不免除 §2 的义务：只要实质相同，来源标注就必须出现。

## 2. 必须保留

任何使用、修改、衍生、再分发（含内部部署、SaaS 提供、闭源产品内嵌、二次分发 SDK）都必须：

1. 保留 `LICENSE` 与 `NOTICE.md` 的**完整原文**，不得删除、截断或改写；
2. 在 README / 关于页 / API 文档 / 包元数据等**用户可见且可检索**的位置标注来源：
   `https://github.com/bfxh/fmnr` 与 `bfxh (lbx13)`；
3. 按 §3 逐项列明上游与第三方来源，不得抹除或含糊化；
4. 若做过修改，须显著说明"修改自 `fmnr`"并列出改动范围。

## 3. 来源清单（Provenance）

- **主体实现**：bfxh (lbx13)，2026-10-15 起。
- **上游 / 第三方代码**：无。本仓代码为原创实现，未复制第三方源码。
- **依赖**：第三方 crate 与工具链按其各自许可（见 `Cargo.lock`、`deny.toml`）；
  本仓的 MIT 授权不覆盖这些依赖，也不改变它们的许可。

## 4. 主导权（Lead）

- 本仓的**方向、取舍、发布节奏与命名**由 **bfxh (lbx13)** 主导决定。
- 外部贡献可以被接受，但**不因此获得项目主导权、改名权或去名权**；不接受以"接管""接手维护"
  "社区版""官方版"为名义移除原作者署名、改变项目定位或覆盖原仓发布。
- 衍生作品应在显著位置回指本仓，并把改进回灌上游；不回灌不违规，**隐瞒来源**违规。

## 5. 举证与救济

- 违反 §1 或 §2，同时构成**许可违约**与**著作权侵权**：MIT 的署名与声明保留条件是授权生效的前提。
- 举证材料：本仓 git 提交历史、文件内注释与常量特征、构建产物指纹。
- 发现违规先要求 30 日内改正并公开署名；逾期则其依据本仓取得的全部授权终止。

---

## English summary (non-binding translation)

This repository (`fmnr`) is led and primarily authored by **bfxh (lbx13)**, licensed under MIT.
This NOTICE is part of that license.

You may **not** present the work as your own ("independently developed", "original", "from scratch")
after only: swapping config/schema tables, replacing data/weights/assets, renaming the
repository/package/identifiers, mechanically rewriting, translating or wrapping the code,
restyling the UI or docs, or extracting a single subsystem.

You **must** keep `LICENSE` and this `NOTICE.md` verbatim, credit `https://github.com/bfxh/fmnr` and `bfxh (lbx13)`
in a user-visible place, list upstream sources per §3, and mark your modifications.
Project direction and naming remain with the original author.
