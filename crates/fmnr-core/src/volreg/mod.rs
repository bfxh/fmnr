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

    /// 按「参数里出现的路径」挑卷：谁的 id 前缀匹配得最长就用谁。
    ///
    /// 这条规则原来长在 CLI 里。提到这里是因为它必须**三个入口共用一份**：
    /// 每个客户端各写一遍，迟早出现"CLI 能读包内文件、MCP 说找不到"，
    /// 而那种分叉看起来像某个入口坏了，实际是两份规则漂移了。
    /// 匹配不到返回 None，由调用方回退默认卷（行为与单卷时代一致）。
    pub fn route_for(&self, samples: &[String]) -> Option<Arc<dyn Volume>> {
        let ids: Vec<VolumeId> = self.list().into_iter().map(|v| v.id).collect();
        let mut best: Option<VolumeId> = None;
        for s in samples {
            for id in &ids {
                if s.starts_with(&id.0) && best.as_ref().is_none_or(|b| id.0.len() > b.0.len()) {
                    best = Some(id.clone());
                }
            }
        }
        best.and_then(|id| self.get(&id))
    }

    /// 直接从工具参数 JSON 路由（字段名单一见 [`path_samples`]）。
    pub fn route_for_args(&self, args: &serde_json::Value) -> Option<Arc<dyn Volume>> {
        self.route_for(&path_samples(args))
    }
}

/// 工具参数里"哪些字段是路径"的**唯一**清单。
///
/// 只认已知字段名，不递归猜：拼错字段（`srcs` 写成 `source`）时应当
/// 回退默认卷并让调用方看见，而不是被当成路径样本路由到莫名其妙的卷上。
pub fn path_samples(args: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    for k in ["path", "root", "dest_dir", "to", "from"] {
        if let Some(s) = args.get(k).and_then(|v| v.as_str()) {
            out.push(s.to_string());
        }
    }
    if let Some(arr) = args.get("sources").and_then(|v| v.as_array()) {
        out.extend(arr.iter().filter_map(|v| v.as_str().map(str::to_string)));
    }
    out
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

    fn dummy(id: &str) -> Arc<DummyVol> {
        Arc::new(DummyVol {
            id: VolumeId(id.into()),
            kind: crate::VolumeKind::Local,
            entries: Vec::new(),
        })
    }

    /// 卷路由的两个易错点之一：**最长 id 前缀**优先。
    /// 选错卷不报错，只会静默走错（表现为莫名的 NotFound），所以钉死。
    #[test]
    fn route_picks_the_longest_matching_volume_id() {
        let reg = VolumeRegistry::new();
        reg.add(dummy("archive:D:/x.zip"));
        reg.add(dummy("archive:D:/x.zip/deep.zip"));
        reg.add(dummy("local:C:"));
        let got = reg
            .route_for(&["archive:D:/x.zip/deep.zip/a/b".to_string()])
            .expect("应当命中某个卷");
        assert_eq!(got.id().0, "archive:D:/x.zip/deep.zip");
        let got2 = reg
            .route_for(&["archive:D:/x.zip/top.txt".to_string()])
            .expect("应当命中某个卷");
        assert_eq!(got2.id().0, "archive:D:/x.zip");
    }

    /// 谁都不匹配时返回 None，让调用方回退默认卷——
    /// 直接报错会让"没挂归档卷"的普通用法变红。
    #[test]
    fn route_returns_none_when_nothing_matches() {
        let reg = VolumeRegistry::new();
        reg.add(dummy("local:C:"));
        assert!(reg.route_for(&["D:/elsewhere".to_string()]).is_none());
        assert!(reg.route_for(&[]).is_none());
    }

    /// 另一个易错点：**只认已知路径字段**。
    /// 搜索词混进样本会让一个跟卷无关的 query 意外决定走哪个卷。
    #[test]
    fn path_samples_reads_only_known_fields() {
        let args = serde_json::json!({
            "path": "archive:D:/x.zip/a",
            "root": "archive:D:/x.zip",
            "from": "archive:D:/x.zip/b",
            "sources": ["a.txt", "archive:D:/x.zip/c"],
            "query": "archive:D:/x.zip",
            "note": "archive:D:/x.zip",
        });
        let got = path_samples(&args);
        for want in [
            "archive:D:/x.zip/a",
            "archive:D:/x.zip",
            "archive:D:/x.zip/b",
            "archive:D:/x.zip/c",
            "a.txt",
        ] {
            assert!(
                got.iter().any(|s| s == want),
                "{want} 应当是样本，实得 {got:?}"
            );
        }
        // query/note 的值换成不像路径的串再断一次：
        // 上面那条 "archive:D:/x.zip" 恰好与 root 同值，单独看它判不出有没有混进来。
        let noisy = serde_json::json!({"query": "hello world", "note": "hello world", "path": "p"});
        assert!(
            !path_samples(&noisy).iter().any(|s| s == "hello world"),
            "非路径字段不该进样本：{noisy:?}"
        );
    }
}
