//! 空间白板（DESIGN §4.6 / §7.1）：布局层，不是 Volume。
//!
//! 白板记录「哪个文件放在画布哪个位置」，持久化到 `board.json`。
//! 与列表视图的根本差别是**位置即语义**：Data Mountain 那篇论文的结论是
//! 用户靠空间记忆找东西比靠列表/树更快、更满意。因此本模块有一条硬约束——
//!
//! > **绝不自动排布。** 加载时位置必须与保存时逐一相等。
//!
//! 任何「自动整理」「对齐网格」「避免重叠」的「改进」都会破坏用户已经建立
//! 的空间记忆，把白板退化成一张难看且更慢的列表。坐标是用户数据，不是
//! 渲染细节。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 画布坐标。f32 因为 egui 是 f32 画布，且缩放后需要小数。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pos {
    pub x: f32,
    pub y: f32,
}

impl Pos {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// 白板上的一个节点：一个文件/目录的引用 + 画布位置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardNode {
    /// 节点稳定标识。用引用路径的字符串形式，重命名会丢，但白板不做重命名跟踪。
    pub path: PathBuf,
    /// 画布坐标。**用户数据**，加载后不得被自动改写。
    pub pos: Pos,
    /// 卡片标题（默认取文件名，用户可改）。
    pub label: String,
    /// 上次加载时该路径是否还存在。不存在时节点**保留**（只标记），
    /// 因为文件可能是临时拔掉的盘/网络卷——静默丢弃会永久毁掉用户的布局。
    #[serde(default)]
    pub missing: bool,
}

impl BoardNode {
    pub fn new(path: PathBuf, pos: Pos) -> Self {
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        Self {
            path,
            pos,
            label,
            missing: false,
        }
    }
}

/// 一条连线：`from` → `to` 是节点在 [`Board::nodes`] 里的下标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub from: usize,
    pub to: usize,
}

/// 一块白板。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Board {
    pub name: String,
    pub nodes: Vec<BoardNode>,
    #[serde(default)]
    pub links: Vec<Link>,
}

#[derive(Debug, thiserror::Error)]
pub enum BoardError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("board.json 解析失败：{0}")]
    Parse(String),
}

impl Board {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            nodes: Vec::new(),
            links: Vec::new(),
        }
    }

    /// 添加节点。返回其下标。
    pub fn add(&mut self, path: PathBuf, pos: Pos) -> usize {
        self.nodes.push(BoardNode::new(path, pos));
        self.nodes.len() - 1
    }

    /// 移动节点。位置改动只从这里进，方便日后加撤销。
    pub fn move_node(&mut self, idx: usize, pos: Pos) {
        if let Some(n) = self.nodes.get_mut(idx) {
            n.pos = pos;
        }
    }

    pub fn remove(&mut self, idx: usize) -> Option<BoardNode> {
        if idx >= self.nodes.len() {
            return None;
        }
        let removed = self.nodes.remove(idx);
        // 下标移位：删掉 idx 后，所有指向它的连线失效；指向 >idx 的要减一。
        self.links.retain(|l| l.from != idx && l.to != idx);
        for l in &mut self.links {
            if l.from > idx {
                l.from -= 1;
            }
            if l.to > idx {
                l.to -= 1;
            }
        }
        Some(removed)
    }

    pub fn connect(&mut self, from: usize, to: usize) -> Result<(), BoardError> {
        if from >= self.nodes.len() || to >= self.nodes.len() {
            return Err(BoardError::Parse(format!(
                "连线端点越界：{from}->{to}，节点数 {}",
                self.nodes.len()
            )));
        }
        if !self.links.iter().any(|l| l.from == from && l.to == to) {
            self.links.push(Link { from, to });
        }
        Ok(())
    }

    /// 刷新每个节点的 `missing` 标记。**只标记，不删除**。
    pub fn refresh_missing(&mut self) {
        for n in &mut self.nodes {
            n.missing = !n.path.exists();
        }
    }

    /// 序列化成 JSON 文本（人类可读，DESIGN §113「配置即数据」）。
    pub fn to_json(&self) -> Result<String, BoardError> {
        serde_json::to_string_pretty(self).map_err(|e| BoardError::Parse(e.to_string()))
    }

    pub fn from_json(text: &str) -> Result<Self, BoardError> {
        let mut b: Board =
            serde_json::from_str(text).map_err(|e| BoardError::Parse(e.to_string()))?;
        b.refresh_missing();
        Ok(b)
    }

    /// 白板文件路径：`<config>/fmnr/boards/<name>.json`。
    pub fn default_path(name: &str) -> PathBuf {
        boards_dir().join(format!("{name}.json"))
    }

    pub fn save(&self, path: &Path) -> Result<(), BoardError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_json()?)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, BoardError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_json(&text)
    }
}

/// 白板目录。与 icons/plugins 同级，都在 `<config>/fmnr` 下。
fn boards_dir() -> PathBuf {
    dirs_config().join("fmnr").join("boards")
}

/// `dirs` crate 不在 fmnr-core 的依赖里（核心库要轻）。这里只用环境变量，
/// 取不到就退到当前目录——白板存不下比编译不过好。
fn dirs_config() -> PathBuf {
    if let Ok(p) = std::env::var("APPDATA") {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("HOME") {
        return PathBuf::from(p).join(".config");
    }
    PathBuf::from(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fmnr-board-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn add_and_move_records_positions() {
        let mut b = Board::new("test");
        let i = b.add(PathBuf::from("/a.txt"), Pos::new(10.0, 20.0));
        let j = b.add(PathBuf::from("/b.txt"), Pos::new(300.5, 40.25));
        assert_eq!(i, 0);
        assert_eq!(j, 1);
        assert_eq!(b.nodes[i].pos, Pos::new(10.0, 20.0));

        b.move_node(i, Pos::new(11.0, 21.0));
        assert_eq!(b.nodes[i].pos, Pos::new(11.0, 21.0));
    }

    /// M6 验收核心：布置 → 保存 → 恢复，位置必须**逐一相等**。
    /// 任何自动排布都会让这条断言失败——这正是它存在的意义。
    #[test]
    fn positions_survive_a_save_load_roundtrip() {
        let dir = tmp("roundtrip");
        let file = dir.join("board.json");

        let mut b = Board::new("我的白板");
        // 故意用不整齐、不对齐的坐标：自动排布会把这些规整掉。
        b.add(dir.join("a.txt"), Pos::new(13.5, 7.25));
        b.add(dir.join("b.txt"), Pos::new(401.0, 88.125));
        b.add(dir.join("c.txt"), Pos::new(-30.75, 512.5));
        b.connect(0, 1).unwrap();
        b.connect(1, 2).unwrap();

        b.save(&file).unwrap();
        let back = Board::load(&file).unwrap();

        assert_eq!(back.name, "我的白板");
        assert_eq!(back.nodes.len(), 3);
        for (orig, got) in b.nodes.iter().zip(back.nodes.iter()) {
            assert_eq!(orig.pos, got.pos, "位置必须逐一相等：{}", orig.label);
            assert_eq!(orig.path, got.path);
            assert_eq!(orig.label, got.label);
        }
        assert_eq!(back.links, b.links);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 文件不在了：节点要**保留**并标记 missing，不能静默消失。
    /// 拔掉的盘/断网的卷会让一堆文件「不存在」，丢弃等于毁掉用户布局。
    #[test]
    fn missing_files_are_flagged_not_dropped() {
        let dir = tmp("missing");
        let mut b = Board::new("m");
        b.add(dir.join("real.txt"), Pos::new(0.0, 0.0));
        std::fs::write(dir.join("real.txt"), b"x").unwrap();
        b.add(dir.join("ghost.txt"), Pos::new(100.0, 0.0));

        b.refresh_missing();
        assert!(!b.nodes[0].missing, "存在的文件不该标记 missing");
        assert!(b.nodes[1].missing, "不存在的文件应标记 missing");
        assert_eq!(b.nodes.len(), 2, "标记不是删除：节点数不变");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removing_a_node_drops_its_links_and_shifts_indices() {
        let mut b = Board::new("l");
        b.add(PathBuf::from("/0"), Pos::new(0.0, 0.0));
        b.add(PathBuf::from("/1"), Pos::new(1.0, 0.0));
        b.add(PathBuf::from("/2"), Pos::new(2.0, 0.0));
        b.connect(0, 1).unwrap();
        b.connect(1, 2).unwrap();

        // 删掉 0：连到 0 的 (0,1) 应消失；剩下的 (1,2) 下标各减一变成 (0,1)。
        b.remove(0);
        assert_eq!(b.nodes.len(), 2);
        assert_eq!(b.nodes[0].path, PathBuf::from("/1"));
        assert_eq!(b.links, vec![Link { from: 0, to: 1 }]);
    }

    #[test]
    fn connect_rejects_out_of_range_endpoints() {
        let mut b = Board::new("e");
        b.add(PathBuf::from("/only"), Pos::new(0.0, 0.0));
        assert!(b.connect(0, 5).is_err(), "越界端点应报错而不是 panic");
        assert!(b.connect(0, 0).is_ok(), "自环允许（表示自我关联）");
        // 重复连线不重复添加。
        b.connect(0, 0).unwrap();
        assert_eq!(b.links.len(), 1);
    }

    #[test]
    fn unknown_field_in_board_json_is_rejected() {
        let bad = r#"{"name":"x","nodes":[],"links":[],"zoom":2.0}"#;
        let e = Board::from_json(bad).unwrap_err();
        assert!(matches!(e, BoardError::Parse(_)), "未知字段应报错：{e}");
    }

    #[test]
    fn label_defaults_to_file_name() {
        let n = BoardNode::new(PathBuf::from("/deep/dir/notes.md"), Pos::new(0.0, 0.0));
        assert_eq!(n.label, "notes.md");
    }
}
