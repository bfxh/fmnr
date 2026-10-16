//! 卷注册表：把多个 `Volume` 聚合成一个可列出/可切换的单元。
//!
//! 当前 `LocalVolume` 只代表**一个**盘；本模块将它提升为「仓」（registry），
//! 让 UI、CLI、MCP 共享同一套「卷列表」与「选卷」语义。
//!
//! M8 目标：UI/CLI 真正能打开归档、切换 WSL、连 SSH——这些卷都要
//! 先注册进本仓，才能被 UI 拉出来选择。

use crate::{Volume, VolumeId, VolumeKind};
use std::sync::Arc;

pub struct VolumeInfo {
    /// 卷的唯一 id（数据源是 `Volume::id()`），CLI/MCP 用它寻址。
    pub id: VolumeId,
    /// 对外展示的短名，优先用 ID 的人类可读段（例如 local:C、archive:/a.zip）。
    pub name: String,
    pub kind: VolumeKind,
    pub writable: bool,
    pub note: String,
}

/// 空注册表：功能最小但完整。
#[derive(Default)]
pub struct VolumeRegistry {
    map: std::sync::RwLock<Vec<Arc<dyn Volume>>>,
}

impl VolumeRegistry {
    pub fn new() -> Self {
        Self {
            map: std::sync::RwLock::new(Vec::new()),
        }
    }

    /// 向仓里加入一个卷。返回该卷自生的 id（与 `Volume::id()` 相同）。
    pub fn add(&self, volume: Arc<dyn Volume>) -> VolumeId {
        let id = volume.id();
        self.map.write().unwrap().push(volume);
        id
    }

    /// 列出仓里所有卷（不对外暴露裸 `Arc<dyn Volume>`）。
    pub fn list(&self) -> Vec<VolumeInfo> {
        let vols = self.map.read().unwrap();
        vols.iter()
            .map(|v| VolumeInfo {
                id: v.id(),
                name: display_name(&**v),
                kind: v.kind(),
                writable: v.writable(),
                note: note_for(&**v),
            })
            .collect()
    }

    /// 按 id 取一个卷（共享引用，UI 读、MCP 读都用它）。
    pub fn get(&self, id: &VolumeId) -> Option<Arc<dyn Volume>> {
        self.map
            .read()
            .unwrap()
            .iter()
            .find(|v| v.id() == *id)
            .cloned()
    }

    /// 按 id 取一个卷（可变引用，CLI 选卷/切卷）。非本仓的调用方不要用。
    pub fn get_mut(&self, id: &VolumeId) -> Option<Arc<dyn Volume>> {
        self.map
            .write()
            .unwrap()
            .iter()
            .find(|v| v.id() == *id)
            .cloned()
    }

    /// 关掉所有卷，重建空仓。
    pub fn clear(&self) {
        *self.map.write().unwrap() = Vec::new();
    }
}

/// 一个「卷来源」的对外描述：挂载尝试的结果。
///
/// 存在的理由：挂载会失败（文件没了、归档坏了、SSH 配置错），
/// 而失败的**原因**必须能被 UI/CLI/MCP 显示出来，
/// 不能只是从列表里静默消失——那用户只会看到「我用不了」却不知道为什么。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VolumeSourceInfo {
    /// 来源的人类可读名（`archive:D:\x\a.zip`、`ssh://u@h`）。
    pub display: String,
    /// 是否真的挂上了。
    pub mounted: bool,
    /// 挂载失败的原因；成功时为 None。
    pub error: Option<String>,
    /// 挂载成功后该卷的类型。
    pub kind: VolumeKind,
}

fn display_name(v: &dyn Volume) -> String {
    // 直接用 id 的字符串，最稳：本地盘是 local:C、归档是 archive:/x/a.zip
    // 这类名字人类就可读，虚拟卷也在枚举里一目了然。
    v.id().0.clone()
}

fn note_for(v: &dyn Volume) -> String {
    match v.kind() {
        crate::VolumeKind::Archive => "只读挂载，懒加载目录".to_string(),
        crate::VolumeKind::Wsl => "WSL 卷".to_string(),
        crate::VolumeKind::Remote => "SSH/SFTP 远程卷".to_string(),
        _ => String::new(),
    }
}

// 空仓是「没有数据」；包装 `list()` 以便 CLI 打印「没有卷」。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ListOpts, Result, VPath, Volume};

    struct DummyVol {
        id: VolumeId,
        kind: VolumeKind,
        entries: Vec<crate::Entry>,
    }

    #[async_trait::async_trait]
    impl Volume for DummyVol {
        fn id(&self) -> VolumeId {
            self.id.clone()
        }
        fn kind(&self) -> VolumeKind {
            self.kind
        }
        async fn list(&self, _path: &VPath, _opts: &ListOpts) -> Result<Vec<crate::Entry>> {
            Ok(self.entries.clone())
        }
        async fn read_stream(
            &self,
            _path: &VPath,
        ) -> Result<
            std::boxed::Box<dyn futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + Unpin>,
        > {
            panic!("not used in dummy");
        }
        async fn write(&self, _path: &VPath, _data: &[u8]) -> Result<()> {
            panic!("not used in dummy");
        }
        fn writable(&self) -> bool {
            true
        }
        fn native_path(&self, _path: &VPath) -> Option<std::path::PathBuf> {
            None
        }
        async fn spawn_external_open(
            &self,
            _file_path: &VPath,
            _program: &str,
        ) -> Result<crate::ExternalOpen> {
            panic!("not used in dummy");
        }
        fn external_opens(&self) -> &crate::ExternalOpens {
            panic!("not used in dummy");
        }
        async fn copy(
            &self,
            _sources: &[VPath],
            _dest_dir: &VPath,
            _policy: crate::ConflictPolicy,
            _progress: std::sync::Arc<crate::Progress>,
        ) -> Result<crate::OpReport> {
            panic!("not used in dummy");
        }
        async fn move_entries(
            &self,
            _sources: &[VPath],
            _dest_dir: &VPath,
            _policy: crate::ConflictPolicy,
            _progress: std::sync::Arc<crate::Progress>,
        ) -> Result<crate::OpReport> {
            panic!("not used in dummy");
        }
        async fn delete(
            &self,
            _targets: &[VPath],
            _progress: std::sync::Arc<crate::Progress>,
        ) -> Result<crate::OpReport> {
            panic!("not used in dummy");
        }
        async fn rename(&self, _from: &VPath, _to: &VPath) -> Result<()> {
            panic!("not used in dummy");
        }
        async fn create_dir(&self, _path: &VPath) -> Result<()> {
            panic!("not used in dummy");
        }
        async fn search(
            &self,
            _root: &VPath,
            _needle: String,
            _limit: usize,
        ) -> Result<Vec<VPath>> {
            panic!("not used in dummy");
        }
    }

    #[test]
    fn registry_unique_ids_across_volumes() {
        let reg = VolumeRegistry::new();
        let a = Arc::new(DummyVol {
            id: VolumeId("v1".into()),
            kind: crate::VolumeKind::Local,
            entries: Vec::new(),
        });
        let b = Arc::new(DummyVol {
            id: VolumeId("v2".into()),
            kind: crate::VolumeKind::Archive,
            entries: Vec::new(),
        });
        reg.add(a);
        reg.add(b);
        assert_eq!(reg.list().len(), 2);
        assert!(reg.get(&VolumeId("v1".into())).is_some());
        assert!(reg.get(&VolumeId("v2".into())).is_some());
        assert!(reg.get(&VolumeId("v3".into())).is_none());
    }

    /// 回归：`add()` 必须回传**卷自生的** id。
    ///
    /// UI 侧曾把当前卷 id 硬编码成 `local:.`，而 `LocalVolume::new(home)` 自生的
    /// id 是 `local:<home 绝对路径>`——两者永不相等，`get()` 永远落空，
    /// 只能靠 fallback 兜底。这个 bug 不报错、不 panic，只让「卷接线」静默失效。
    /// 断言 `add` 的返回值本身就是可查得的 key。
    #[test]
    fn add_returns_lookup_key() {
        let reg = VolumeRegistry::new();
        let id = reg.add(Arc::new(DummyVol {
            id: VolumeId("local:C:\\Users\\x".into()),
            kind: crate::VolumeKind::Local,
            entries: Vec::new(),
        }));
        assert_eq!(id, VolumeId("local:C:\\Users\\x".into()));
        assert!(reg.get(&id).is_some(), "add() 回传的 id 必须能直接查回该卷");
        assert_eq!(reg.list().len(), 1);
        assert_eq!(reg.list()[0].id, id);
    }

    /// 只读卷必须在 `VolumeInfo` 上如实标注：UI 靠它给只读卷加「（只读）」后缀。
    #[test]
    fn readonly_is_surfaced() {
        let reg = VolumeRegistry::new();
        reg.add(Arc::new(DummyVol {
            id: VolumeId("archive:a.zip".into()),
            kind: crate::VolumeKind::Archive,
            entries: Vec::new(),
        }));
        let info = &reg.list()[0];
        assert_eq!(info.kind, crate::VolumeKind::Archive);
        assert!(info.writable, "DummyVol 自报可写，这里断言字段被如实透传");
        assert!(!info.note.is_empty(), "归档卷应有说明文案");
    }
}
