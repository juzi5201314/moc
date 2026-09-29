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
    path: String,
    peer_id: String,
    url: String,
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
    if let Some(binding) = registry
        .bindings
        .iter()
        .filter(|binding| binding.url == url && canonical_root.starts_with(&binding.path))
        .max_by_key(|binding| binding.path.len())
    {
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
    if let Some(binding) = registry
        .bindings
        .iter()
        .filter(|binding| binding.url == url && canonical_root.starts_with(&binding.path))
        .max_by_key(|binding| binding.path.len())
    {
        return Ok(WorkspaceIdentity {
            canonical_root,
            peer_id: binding.peer_id.clone(),
            source: PeerSource::Registered,
        });
    }
    registry.bindings.push(Binding {
        path: identity.canonical_root.to_string_lossy().into_owned(),
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
            path: binding.path.clone(),
            peer_id: binding.peer_id.clone(),
            url: binding.url.clone(),
            exists: Path::new(&binding.path).is_dir(),
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
    let target = target.to_string_lossy().into_owned();
    let _guard = lock(home)?;
    let mut registry = load(home)?;
    if registry
        .bindings
        .iter()
        .any(|binding| binding.url == url && binding.path == target && binding.path != path)
    {
        return Err(CoreError::Config(
            "目标路径已有记忆绑定，请先明确处理原绑定；不会覆盖它".into(),
        ));
    }
    let binding = registry
        .bindings
        .iter_mut()
        .find(|binding| binding.url == url && binding.path == path)
        .ok_or_else(|| CoreError::Config("工作区绑定已变更，请刷新列表".into()))?;
    binding.path = target;
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
        .position(|binding| binding.url == url && binding.path == path && binding.peer_id == peer)
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
