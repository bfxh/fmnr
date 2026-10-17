//! 目录树模型。
//!
//! 刻意与渲染无关：侧栏长什么样、放在哪、宽度多少，都不影响这里。
//! 四条契约（见 `docs/UI-NEXT.md` §8 第 1 条）：
//! 只显示目录、懒加载、展开态按绝对路径为键、读失败要留可见标记而不是静默变空。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// 树里的一层列举结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Level {
    /// 这一层的子目录。**文件不在里面**（见 `level_of`）。
    pub dirs: Vec<TreeDir>,
    /// 读这一层失败的原因（权限、卷掉了、路径已不存在）。
    ///
    /// 有错时 `dirs` 为空，但节点**必须留在树上**并显示标记：
    /// 把"我读不了"渲染成"这里没有东西"是假答案。
    pub error: Option<String>,
}

/// 从卷报的条目里挑出子目录。
///
/// 排序在这里做而不在 UI 里做：树的顺序必须**稳定**（不跟随两栏的名称/大小/时间
/// 排序切换），否则用户刚要点开的目录会因为改了排序而跳走。
pub fn level_of(parent: &Path, entries: &[fmnr_core::Entry]) -> Level {
    let mut dirs: Vec<TreeDir> = entries
        .iter()
        .filter(|e| e.is_dir)
        .map(|e| TreeDir {
            path: parent.join(&e.name),
            name: e.name.clone(),
        })
        .collect();
    dirs.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    Level { dirs, error: None }
}

/// 把「特殊文件夹」并进某一层的目录列表前面。
///
/// 用于把 下载/文档/图片/桌面 挂在起点目录下面：它们可能被系统重定向
/// （实测常见的是被挪进 OneDrive），真实子目录名和稳定标签不是一回事，
/// 所以标签由调用方给，这里只负责**按路径去重**——
/// 同一个目录既出现在真实列举里、又出现在特殊清单里时，只留一份，
/// 留着带标签的那份（用户认的是"下载"，不是 `OneDrive\文档` 这种路径）。
pub fn merge_specials(mut level: Level, specials: &[TreeDir]) -> Level {
    // 列失败的层原样退回：那里已经有一个红色标记要说"我读不到"，
    // 再并四个指得准不准都不知道的节点进去，等于把错误盖成一份看起来正常的清单。
    if level.error.is_some() {
        return level;
    }
    // 带标签的那份排在最前，**保持调用方给的顺序**；同一个目录若在真实
    // 列举里也出现（多数情况就是如此），从它原来的字母位摘掉。
    // 留在字母位等于没挂——实测这个 home 下有一百多个点目录，`下载` 会被埋到看不见。
    let mut dirs: Vec<TreeDir> = specials.to_vec();
    dirs.extend(
        level
            .dirs
            .into_iter()
            .filter(|d| !specials.iter().any(|s| s.path == d.path)),
    );
    level.dirs = dirs;
    level
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeDir {
    pub path: PathBuf,
    pub name: String,
}

/// 渲染用的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub dir: TreeDir,
    pub depth: usize,
    pub expanded: bool,
    /// 已加载过且确实没有子目录 ⇒ 可以不再显示展开箭头。
    /// 与"还没加载过"必须区分开，否则用户分不清"空"和"没点开过"。
    pub known_leaf: bool,
    /// 这一层读失败的原因（挂在父节点上，让父节点自己标红）。
    pub error: Option<String>,
}

/// 防止自引用符号链接把可见行拉成无限：一条链上最多这么多层。
/// 32 足够深（真实路径很少超过 15 层），又远低于会让 UI 卡死的量级。
pub const MAX_DEPTH: usize = 32;

#[derive(Debug, Default)]
pub struct Tree {
    roots: Vec<TreeDir>,
    expanded: HashSet<PathBuf>,
    /// 已加载的层。缺键 = 还没加载（懒加载的核心状态）。
    levels: HashMap<PathBuf, Level>,
    selected: Option<PathBuf>,
}

impl Tree {
    pub fn set_roots(&mut self, roots: Vec<TreeDir>) {
        self.roots = roots;
        // 根换了，选中态若不在新根下就作废：留着一个看不见的选中项
        // 会让"回车/删除作用于哪个文件"变成猜谜。
        if let Some(sel) = &self.selected {
            if !self.roots.iter().any(|r| sel.starts_with(&r.path)) {
                self.selected = None;
            }
        }
    }

    pub fn roots(&self) -> &[TreeDir] {
        &self.roots
    }

    pub fn selected(&self) -> Option<&Path> {
        self.selected.as_deref()
    }

    pub fn select(&mut self, path: Option<PathBuf>) {
        self.selected = path;
    }

    /// 展开/收起。返回 `true` 表示**需要去列这一层**（调用方据此起一次异步列举）。
    pub fn toggle(&mut self, path: &Path) -> bool {
        if self.expanded.remove(path) {
            return false;
        }
        self.expanded.insert(path.to_path_buf());
        self.needs_load(path)
    }

    /// 展开了但还没有缓存 ⇒ 要列。
    pub fn needs_load(&self, path: &Path) -> bool {
        self.expanded.contains(path) && !self.levels.contains_key(path)
    }

    /// 喂进一层的列举结果。
    ///
    /// 只接受"当前正在展开"的路径：收起后晚到的结果直接丢，
    /// 否则一次慢列举返回时会在用户已经收起的节点下悄悄长出子树。
    pub fn put_level(&mut self, path: &Path, level: Level) {
        if self.expanded.contains(path) {
            self.levels.insert(path.to_path_buf(), level);
        }
    }

    /// 丢掉某个路径之下（含自身）的缓存层。重扫/卷变化时用。
    ///
    /// 展开态**故意不清**：路径还在原地，用户不该因为一次刷新就被迫重新点开。
    pub fn invalidate_below(&mut self, path: &Path) {
        self.levels.retain(|k, _| !k.starts_with(path));
    }

    /// 当前**可见**且还没列出来的目录。渲染前拿这个去补数据。
    ///
    /// 只看可见行：收起的父节点下面可能留着几十个"展开但未加载"的旧路径，
    /// 全去列一遍等于替用户点了他看不见的东西。
    pub fn pending_loads(&self) -> Vec<PathBuf> {
        self.rows()
            .into_iter()
            .map(|r| r.dir.path)
            .filter(|p| self.needs_load(p))
            .collect()
    }

    /// 当前可见行（深度优先，按根的顺序）。
    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for root in &self.roots {
            out.push(self.row_of(root, 0));
            self.walk(root, 0, &mut out);
        }
        out
    }

    fn walk(&self, dir: &TreeDir, depth: usize, out: &mut Vec<Row>) {
        if depth >= MAX_DEPTH || !self.expanded.contains(&dir.path) {
            return;
        }
        let Some(level) = self.levels.get(&dir.path) else {
            return;
        };
        for child in &level.dirs {
            out.push(self.row_of(child, depth + 1));
            // 自引用符号链接会造出 A/l/A/l/… 这样**互不相同**的路径，
            // 光靠"已展开集合"挡不住无限下降，所以深度上限是硬闸。
            self.walk(child, depth + 1, out);
        }
    }

    fn row_of(&self, dir: &TreeDir, depth: usize) -> Row {
        let expanded = self.expanded.contains(&dir.path);
        let level = self.levels.get(&dir.path);
        Row {
            dir: dir.clone(),
            depth,
            expanded,
            known_leaf: matches!(level, Some(l) if l.error.is_none() && l.dirs.is_empty()),
            error: level.and_then(|l| l.error.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> fmnr_core::Entry {
        fmnr_core::Entry {
            name: name.to_string(),
            is_dir,
            size: if is_dir { None } else { Some(1) },
            modified: None,
            icon_key: if is_dir {
                fmnr_core::IconKey::Directory
            } else {
                fmnr_core::IconKey::Generic
            },
            attrs: Vec::new(),
        }
    }

    fn dir(path: &str) -> TreeDir {
        TreeDir {
            path: PathBuf::from(path),
            name: Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string()),
        }
    }

    /// 特殊文件夹并进起点那一层：一律插到最前，且同一目录不许出现两次。
    #[test]
    fn specials_are_prepended_but_never_duplicated() {
        let level = Level {
            dirs: vec![
                dir("R:/a"),
                TreeDir {
                    path: PathBuf::from("R:/od"),
                    name: "od".into(),
                },
            ],
            error: None,
        };
        let merged = merge_specials(
            level,
            &[
                TreeDir {
                    path: PathBuf::from("R:/od"),
                    name: "文档".into(),
                },
                TreeDir {
                    path: PathBuf::from("R:/dl"),
                    name: "下载".into(),
                },
            ],
        );
        let got: Vec<(String, String)> = merged
            .dirs
            .iter()
            .map(|d| (d.name.clone(), d.path.to_string_lossy().to_string()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("文档".to_string(), "R:/od".to_string()),
                ("下载".to_string(), "R:/dl".to_string()),
                ("a".to_string(), "R:/a".to_string()),
            ],
            "实得 {got:?}"
        );
        // 同一个路径在结果里只能出现一次
        let mut paths: Vec<_> = got.iter().map(|(_, p)| p.clone()).collect();
        let n = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), n, "并进来了重复目录");
    }

    /// 起点目录列不出来（权限/卷掉了）时不许长出假节点：错误标记必须原样留着。
    #[test]
    fn merge_keeps_an_error_level_empty() {
        let level = Level {
            dirs: Vec::new(),
            error: Some("拒绝访问".into()),
        };
        let merged = merge_specials(level, &[dir("R:/dl")]);
        assert_eq!(merged.error.as_deref(), Some("拒绝访问"));
        assert!(
            merged.dirs.is_empty(),
            "列失败的层不该被并进节点：{:?}",
            merged.dirs
        );
    }

    #[test]
    fn files_never_appear_in_the_tree() {
        let level = level_of(
            Path::new("R:"),
            &[
                entry("afile.txt", false),
                entry("Zdir", true),
                entry("another.md", false),
                entry("adir", true),
            ],
        );
        let names: Vec<&str> = level.dirs.iter().map(|d| d.name.as_str()).collect();
        // 目录按名称稳定排序，与文件混在一起的排序不参与。
        assert_eq!(names, vec!["adir", "Zdir"]);
    }

    #[test]
    fn toggle_asks_for_a_listing_only_the_first_time() {
        let mut t = Tree::default();
        let d = dir("R:/a");
        t.set_roots(vec![d.clone()]);

        assert!(t.toggle(&d.path), "首次展开要去列这一层");
        assert!(t.needs_load(&d.path));
        t.put_level(&d.path, Level::default());
        assert!(!t.needs_load(&d.path), "已有缓存就不该再列");
        assert!(!t.toggle(&d.path), "收起时不需要列举");
        assert!(
            !t.toggle(&d.path),
            "收起后再展开要秒开缓存，不该重列——重列是 invalidate_below 的职责"
        );
    }

    #[test]
    fn a_level_arriving_after_collapse_is_dropped() {
        // 慢列举返回时用户已经收起了节点：不能在他眼皮底下长出子树。
        let mut t = Tree::default();
        let d = dir("R:/a");
        t.set_roots(vec![d.clone()]);
        t.toggle(&d.path);
        t.toggle(&d.path);
        t.put_level(
            &d.path,
            Level {
                dirs: vec![dir("R:/a/child")],
                error: None,
            },
        );
        assert_eq!(t.rows().len(), 1, "只剩根自己");
    }

    #[test]
    fn expansion_survives_a_refresh_because_it_is_keyed_by_path() {
        // 借 disktree 不变量 5：位置键会被重排弄失效，路径键不会。
        let mut t = Tree::default();
        let root = dir("R:");
        t.set_roots(vec![root.clone()]);
        t.toggle(&root.path);
        t.put_level(
            &root.path,
            Level {
                dirs: vec![dir("R:/b"), dir("R:/a")],
                error: None,
            },
        );
        let a = dir("R:/a");
        t.toggle(&a.path);
        t.put_level(&a.path, Level::default());
        assert_eq!(t.rows().len(), 3, "根 + 两个子；a 已展开但它是空的");

        // 刷新根层（比如新出现一个目录），a 的展开态必须还在。
        t.put_level(
            &root.path,
            Level {
                dirs: vec![dir("R:/a"), dir("R:/b"), dir("R:/c")],
                error: None,
            },
        );
        let a_row = t
            .rows()
            .into_iter()
            .find(|r| r.dir.path == a.path)
            .expect("a 该在可见行里");
        assert!(a_row.expanded, "换根层数据不该把 a 的展开态弄丢");
        assert_eq!(t.rows().len(), 4, "根 + 三个子");
    }

    #[test]
    fn read_error_stays_visible_and_is_not_reported_as_empty() {
        let mut t = Tree::default();
        let root = dir("R:");
        t.set_roots(vec![root.clone()]);
        t.toggle(&root.path);
        t.put_level(
            &root.path,
            Level {
                dirs: Vec::new(),
                error: Some("拒绝访问".into()),
            },
        );
        let rows = t.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].error.as_deref(), Some("拒绝访问"));
        assert!(
            !rows[0].known_leaf,
            "读失败不能被当成'这个目录是空的'——两者要长得不一样"
        );
    }

    #[test]
    fn loaded_but_childless_is_marked_as_a_known_leaf() {
        let mut t = Tree::default();
        let root = dir("R:");
        t.set_roots(vec![root.clone()]);
        t.toggle(&root.path);
        t.put_level(&root.path, Level::default());
        let rows = t.rows();
        assert!(rows[0].known_leaf, "列过且确实没有子目录 ⇒ 可以不再给箭头");
        assert!(rows[0].error.is_none());
    }

    #[test]
    fn self_referential_chain_is_capped_not_infinite() {
        // 符号链接指回祖先会造出 A/l/A/l/… 互不相同的路径，
        // 展开集合挡不住，只能靠深度硬闸。
        let mut t = Tree::default();
        let root = dir("R:/a");
        t.set_roots(vec![root.clone()]);
        let mut path = PathBuf::from("R:/a");
        for _ in 0..(MAX_DEPTH + 10) {
            t.toggle(&path);
            let child = path.join("l");
            t.put_level(
                &path,
                Level {
                    dirs: vec![TreeDir {
                        name: "l".into(),
                        path: child.clone(),
                    }],
                    error: None,
                },
            );
            path = child;
        }
        let rows = t.rows();
        assert_eq!(
            rows.len(),
            MAX_DEPTH + 1,
            "根(0) 到最深一行(MAX_DEPTH)，必须停在深度上限而不是爆掉"
        );
        assert_eq!(rows.last().map(|r| r.depth), Some(MAX_DEPTH));
    }

    #[test]
    fn invalidate_below_drops_cache_but_keeps_expansion() {
        let mut t = Tree::default();
        let root = dir("R:");
        t.set_roots(vec![root.clone()]);
        t.toggle(&root.path);
        t.put_level(
            &root.path,
            Level {
                dirs: vec![dir("R:/a")],
                error: None,
            },
        );
        let a = dir("R:/a");
        t.toggle(&a.path);
        t.put_level(
            &a.path,
            Level {
                dirs: vec![dir("R:/a/deep")],
                error: None,
            },
        );

        t.invalidate_below(Path::new("R:/a"));
        assert!(
            t.rows().iter().any(|r| r.dir.path == a.path && r.expanded),
            "刷新不该把用户点开的位置抹掉"
        );
        assert!(t.needs_load(&a.path), "但缓存要作废，重列时会拿新数据");
        assert_eq!(t.rows().len(), 2, "没缓存的展开节点只剩自己一行");
    }

    #[test]
    fn pending_loads_only_covers_visible_nodes() {
        let mut t = Tree::default();
        let root = dir("R:");
        t.set_roots(vec![root.clone()]);
        t.toggle(&root.path);
        assert_eq!(t.pending_loads(), vec![PathBuf::from("R:")]);
        t.put_level(
            &root.path,
            Level {
                dirs: vec![dir("R:/a")],
                error: None,
            },
        );
        assert!(t.pending_loads().is_empty(), "已加载的不该再列");

        // 收起根之后，根下面那个"展开但未加载"的节点不可见 ⇒ 不该出现在待列里。
        let a = dir("R:/a");
        t.toggle(&a.path);
        t.toggle(&root.path);
        assert!(
            t.needs_load(&a.path),
            "收起父节点不该顺手抹掉子节点的展开态（needs_load 仍为真 = 还记着它被展开过）"
        );
        assert!(t.pending_loads().is_empty(), "看不见的节点不该被替用户点开");
    }

    #[test]
    fn selection_outside_the_new_roots_is_dropped() {
        // 留一个看不见的选中项，等于让回车/删除去动用户没看到的东西。
        let mut t = Tree::default();
        t.set_roots(vec![dir("C:")]);
        t.select(Some(PathBuf::from("C:/Users/lbx13")));
        assert_eq!(t.selected(), Some(Path::new("C:/Users/lbx13")));
        t.set_roots(vec![dir("D:")]);
        assert_eq!(t.selected(), None);
    }
}
