//! 路径只是知识域的绑定，不再是知识域本身；重命名目录无需移动远程内容。

use crate::{CoreError, config::ResolvedConfig};
use moc_openviking::{PeerSource, WorkspaceIdentity};
use moc_protocol::WorkspaceBinding;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    bindings: Vec<Binding>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Binding {
    path: StoredPath,
    peer_id: String,
    url: String,
}

/// 绑定路径按字节保真存储：UTF-8 路径沿用既有字符串格式，非 UTF-8 路径存原始
/// 字节。`to_string_lossy` 会把不同路径折叠成同一字符串，使匹配永不命中并重复写盘。
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum StoredPath {
    Text(String),
    Bytes(Vec<u8>),
}

impl StoredPath {
    fn from_path(path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            if let Some(text) = path.to_str() {
                return Self::Text(text.to_owned());
            }
            Self::Bytes(path.as_os_str().as_bytes().to_vec())
        }
        #[cfg(not(unix))]
        {
            Self::Text(path.to_string_lossy().into_owned())
        }
    }

    fn as_path(&self) -> PathBuf {
        match self {
            Self::Text(text) => PathBuf::from(text),
            Self::Bytes(bytes) => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStrExt;
                    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
                }
                #[cfg(not(unix))]
                {
                    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
                }
            }
        }
    }

    /// 协议层仍以字符串表达绑定路径；这里只用于展示与用户操作回填。
    fn display(&self) -> String {
        self.as_path().to_string_lossy().into_owned()
    }

    /// 用户操作传入的是展示字符串，非 UTF-8 路径按展示形式回退匹配。
    fn matches(&self, requested: &str) -> bool {
        self.as_path() == Path::new(requested) || self.display() == requested
    }
}

/// 在注册表中查找当前根所属的绑定；更深的根优先。
fn find<'a>(registry: &'a Registry, url: &str, root: &Path) -> Option<&'a Binding> {
    registry
        .bindings
        .iter()
        .filter(|binding| binding.url == url && root.starts_with(binding.path.as_path()))
        .max_by_key(|binding| binding.path.as_path().components().count())
}

fn registry_path(home: &Path) -> PathBuf {
    home.join("workspaces.json")
}
fn load(home: &Path) -> Result<Registry, CoreError> {
    match std::fs::read(registry_path(home)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| CoreError::Config(format!("工作区注册表损坏：{error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(error) => Err(error.into()),
    }
}
fn save(home: &Path, registry: &Registry) -> Result<(), CoreError> {
    use std::io::Write;
    let bytes = serde_json::to_vec_pretty(registry)
        .map_err(|error| CoreError::Config(error.to_string()))?;
    let temp = home.join(format!(".workspaces-{}.tmp", crate::ids::entry_id()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, registry_path(home))?;
        Ok::<_, CoreError>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}
fn lock(home: &Path) -> Result<crate::session::lock::SessionLockGuard, CoreError> {
    std::fs::create_dir_all(home)?;
    crate::session::lock::SessionLockGuard::acquire(home.join("workspaces.lock"), "workspaces")
}

pub fn resolve(config: &ResolvedConfig, cwd: &Path) -> Result<WorkspaceIdentity, CoreError> {
    let canonical_root = cwd.canonicalize()?;
    let url = config.openviking.url.trim_end_matches('/');
    let registry = load(&config.home)?;
    if let Some(binding) = find(&registry, url, &canonical_root) {
        return Ok(WorkspaceIdentity {
            canonical_root,
            peer_id: binding.peer_id.clone(),
            source: PeerSource::Registered,
        });
    }
    let identity = moc_openviking::resolve_peer_id(cwd)?;
    if url.is_empty() {
        return Ok(identity);
    }
    let _guard = lock(&config.home)?;
    let mut registry = load(&config.home)?;
    if let Some(binding) = find(&registry, url, &canonical_root) {
        return Ok(WorkspaceIdentity {
            canonical_root,
            peer_id: binding.peer_id.clone(),
            source: PeerSource::Registered,
        });
    }
    registry.bindings.push(Binding {
        path: StoredPath::from_path(&identity.canonical_root),
        peer_id: identity.peer_id.clone(),
        url: url.into(),
    });
    save(&config.home, &registry)?;
    Ok(identity)
}

pub fn list(home: &Path) -> Result<Vec<WorkspaceBinding>, CoreError> {
    let registry = load(home)?;
    Ok(registry
        .bindings
        .iter()
        .map(|binding| WorkspaceBinding {
            path: binding.path.display(),
            peer_id: binding.peer_id.clone(),
            url: binding.url.clone(),
            exists: binding.path.as_path().is_dir(),
            shared: registry
                .bindings
                .iter()
                .filter(|other| other.url == binding.url && other.peer_id == binding.peer_id)
                .count()
                > 1,
        })
        .collect())
}

pub fn rebind(home: &Path, url: &str, path: &str, new_path: &str) -> Result<(), CoreError> {
    let target = Path::new(new_path).canonicalize()?;
    if !target.is_dir() {
        return Err(CoreError::Config("迁移目标必须是已存在的目录".into()));
    }
    let _guard = lock(home)?;
    let mut registry = load(home)?;
    if registry.bindings.iter().any(|binding| {
        binding.url == url && binding.path.as_path() == target && !binding.path.matches(path)
    }) {
        return Err(CoreError::Config(
            "目标路径已有记忆绑定，请先明确处理原绑定；不会覆盖它".into(),
        ));
    }
    let binding = registry
        .bindings
        .iter_mut()
        .find(|binding| binding.url == url && binding.path.matches(path))
        .ok_or_else(|| CoreError::Config("工作区绑定已变更，请刷新列表".into()))?;
    binding.path = StoredPath::from_path(&target);
    save(home, &registry)
}

pub async fn delete(
    config: &ResolvedConfig,
    url: &str,
    path: &str,
    peer: &str,
) -> Result<String, CoreError> {
    if config.openviking.url.trim_end_matches('/') != url {
        return Err(CoreError::Config(
            "请先连接此绑定所属的 OpenViking 服务；不会向其他服务发送删除请求".into(),
        ));
    }
    let _guard = lock(&config.home)?;
    let mut registry = load(&config.home)?;
    let index = registry
        .bindings
        .iter()
        .position(|binding| {
            binding.url == url && binding.path.matches(path) && binding.peer_id == peer
        })
        .ok_or_else(|| CoreError::Config("绑定已变化，请重新确认删除".into()))?;
    let shared = registry
        .bindings
        .iter()
        .enumerate()
        .any(|(other_index, binding)| {
            other_index != index && binding.url == url && binding.peer_id == peer
        });
    if !shared {
        if peer.is_empty()
            || matches!(peer, "." | "..")
            || !peer
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.@-".contains(&byte))
        {
            return Err(CoreError::Config(
                "工作区 peer 不是合法路径段，拒绝删除".into(),
            ));
        }
        let client = moc_openviking::OpenVikingClient::new(config.openviking.clone())
            .map_err(|error| CoreError::Config(error.to_string()))?
            .ok_or_else(|| CoreError::Config("OpenViking 尚未启用".into()))?;
        match client
            .delete(&format!("viking://~/peers/{peer}"), Some(peer), true)
            .await
        {
            Ok(_) | Err(moc_openviking::OpenVikingError::HttpStatus { status: 404, .. }) => {}
            Err(error) => {
                return Err(CoreError::Config(format!(
                    "远程专属知识尚未删除，保留绑定：{error}"
                )));
            }
        }
    }
    registry.bindings.remove(index);
    save(&config.home, &registry)?;
    Ok(if shared {
        "已解除路径绑定；其他路径仍使用同一知识域，远程内容保留"
    } else {
        "已删除此工作区的专属知识与路径绑定；用户共享记忆、共享资源及 agent 内容未删除"
    }
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(path: &Path, url: &str, peer: &str) -> Binding {
        Binding {
            path: StoredPath::from_path(path),
            peer_id: peer.into(),
            url: url.into(),
        }
    }

    #[test]
    fn utf8_bindings_keep_the_existing_string_format() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("work");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let mut registry = Registry::default();
        registry
            .bindings
            .push(binding(&root, "https://viking", "peer1"));
        save(home.path(), &registry).unwrap();
        let raw = std::fs::read_to_string(registry_path(home.path())).unwrap();
        assert!(raw.contains(&format!("\"path\": \"{}\"", root.display())));
        assert_eq!(load(home.path()).unwrap().bindings[0].path.as_path(), root);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_bindings_round_trip_and_match_without_duplicate_writes() {
        use std::os::unix::ffi::OsStrExt;
        let home = tempfile::tempdir().unwrap();
        let raw = std::ffi::OsStr::from_bytes(b"raw-\xff-dir");
        let root = home.path().join(raw);
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        assert!(root.to_str().is_none(), "测试路径必须是非 UTF-8");
        let mut registry = Registry::default();
        registry
            .bindings
            .push(binding(&root, "https://viking", "peer1"));
        save(home.path(), &registry).unwrap();

        let loaded = load(home.path()).unwrap();
        assert_eq!(loaded.bindings[0].path.as_path(), root);
        // resolve 在追加绑定前先查既有绑定；非 UTF-8 根必须命中，否则每次查询都会写盘。
        assert_eq!(
            find(&loaded, "https://viking", &root).map(|binding| binding.peer_id.as_str()),
            Some("peer1")
        );
        // 展示层仍是 lossy 字符串，用户按展示路径发起 rebind/delete 也要匹配。
        let listing = list(home.path()).unwrap();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].path, root.to_string_lossy());
        assert!(listing[0].exists);
        assert!(loaded.bindings[0].path.matches(&root.to_string_lossy()));
    }
}
