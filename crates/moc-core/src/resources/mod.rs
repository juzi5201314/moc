//! MOC 内部资源 URI：统一解析、读取与输入补全。

mod uri;

pub use uri::{InternalUri, ResourceScheme};

use std::fs;
use std::path::{Path, PathBuf};

use crate::session::SessionTable;
use crate::session::store::SessionPaths;
use crate::skills::SkillIndex;

/// 资源解析错误；`code` 会直接投影为工具错误码。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("[{code}] {message}")]
pub struct ResourceError {
    pub code: &'static str,
    pub message: String,
}

impl ResourceError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// 资源的物理承载；`Text` 用于不应暴露物理路径的虚拟资源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceBacking {
    File(PathBuf),
    Text(String),
}

/// 读取解析后的资源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedResource {
    pub uri: String,
    pub display_name: String,
    pub read_only: bool,
    pub backing: ResourceBacking,
}

/// 输入框与资源读取共用的候选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceCandidate {
    pub uri: String,
    pub label: String,
    pub description: String,
    pub kind: String,
    pub append_space: bool,
}

/// 资源解析所需的当前会话视图。
pub struct ResourceContext<'ctx> {
    pub session_id: &'ctx str,
    pub cwd: &'ctx Path,
    pub paths: &'ctx SessionPaths,
    pub skills: &'ctx SkillIndex,
    pub sessions: &'ctx SessionTable,
    /// 仅用于 completion 展示 viking namespace；不把 provider 配置或 secret 暴露给候选。
    pub openviking_configured: bool,
}

/// artifact sidecar 的唯一 URI/路径映射实现。
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    session_id: String,
    paths: SessionPaths,
}

impl ArtifactStore {
    pub fn new(session_id: impl Into<String>, paths: SessionPaths) -> Self {
        Self {
            session_id: session_id.into(),
            paths,
        }
    }

    pub fn uri_for_path(&self, path: &Path) -> Result<String, ResourceError> {
        let canonical = fs::canonicalize(path)
            .map_err(|error| ResourceError::new("E_ARTIFACT_NOT_FOUND", error.to_string()))?;
        for (kind, root) in [
            ("jobs", self.paths.jobs()),
            ("artifacts", self.paths.artifacts()),
        ] {
            let Ok(root) = fs::canonicalize(&root) else {
                continue;
            };
            if let Ok(relative) = canonical.strip_prefix(&root)
                && relative.components().count() == 1
                && relative.file_name().is_some()
            {
                return Ok(format!(
                    "artifact://{}/{}/{}",
                    self.session_id,
                    kind,
                    relative.to_string_lossy()
                ));
            }
        }
        Err(ResourceError::new(
            "E_ARTIFACT_PATH",
            format!(
                "{} is outside this session's artifact sidecar",
                path.display()
            ),
        ))
    }

    pub fn resolve_uri(&self, input: &str) -> Result<ResolvedResource, ResourceError> {
        let uri = InternalUri::parse(input)
            .map_err(|error| ResourceError::new("E_ARTIFACT_URI", error.message))?;
        if uri.scheme() != "artifact" {
            return Err(ResourceError::new(
                "E_ARTIFACT_URI",
                "scheme must be artifact",
            ));
        }
        if uri.authority() != self.session_id {
            return Err(ResourceError::new(
                "E_ARTIFACT_SCOPE",
                "artifact belongs to a different session",
            ));
        }
        let [kind, name] = uri.two_segments("E_ARTIFACT_URI")?;
        if !matches!(kind, "jobs" | "artifacts") {
            return Err(ResourceError::new(
                "E_ARTIFACT_URI",
                "invalid artifact namespace",
            ));
        }
        if name.starts_with('.') {
            return Err(ResourceError::new(
                "E_ARTIFACT_PATH",
                "artifact hidden files are not allowed",
            ));
        }
        let root = if kind == "jobs" {
            self.paths.jobs()
        } else {
            self.paths.artifacts()
        };
        let path = checked_child(&root, name, "E_ARTIFACT_PATH", "E_ARTIFACT_NOT_FOUND")?;
        Ok(ResolvedResource {
            uri: input.to_owned(),
            display_name: input.to_owned(),
            read_only: true,
            backing: ResourceBacking::File(path),
        })
    }

    pub async fn archive_text(&self, text: &str) -> Result<String, ResourceError> {
        let directory = self.paths.artifacts();
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(|error| ResourceError::new("E_ARTIFACT_PATH", error.to_string()))?;
        let name = format!("{}-output.txt", crate::ids::entry_id());
        let path = directory.join(name);
        tokio::fs::write(&path, text.as_bytes())
            .await
            .map_err(|error| ResourceError::new("E_ARTIFACT_PATH", error.to_string()))?;
        self.uri_for_path(&path)
    }

    fn recent_candidates(&self) -> Vec<ResourceCandidate> {
        let mut files = Vec::new();
        for (kind, root) in [
            ("jobs", self.paths.jobs()),
            ("artifacts", self.paths.artifacts()),
        ] {
            let Ok(entries) = fs::read_dir(root) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_file() || entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                let uri = format!(
                    "artifact://{}/{}/{}",
                    self.session_id,
                    kind,
                    entry.file_name().to_string_lossy()
                );
                files.push((modified, uri));
            }
        }
        files.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        files
            .into_iter()
            .take(20)
            .map(|(_, uri)| ResourceCandidate {
                label: uri.clone(),
                uri,
                description: "read-only artifact for the current session".into(),
                kind: "file".into(),
                append_space: true,
            })
            .collect()
    }
}

/// 所有 MOC-native URI 的统一入口。
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceRouter;

impl ResourceRouter {
    pub fn resolve_read(
        ctx: &crate::tools::ToolCtx,
        input: &str,
    ) -> Result<Option<ResolvedResource>, ResourceError> {
        let Some((scheme, _)) = input.split_once("://") else {
            return Ok(None);
        };
        if matches!(scheme, "ov" | "viking") {
            return Ok(None);
        }
        let context = ctx.resource_context();
        Self::resolve_context(&context, input)
    }

    pub fn write(input: &str) -> Result<(), ResourceError> {
        let Some((scheme, _)) = input.split_once("://") else {
            return Ok(());
        };
        if matches!(scheme, "ov" | "viking") {
            return Ok(());
        }
        let uri = InternalUri::parse(input)
            .map_err(|error| ResourceError::new(error.code, error.message))?;
        if matches!(uri.scheme(), "artifact" | "skill" | "local" | "history") {
            return Err(ResourceError::new(
                "E_URI_READ_ONLY",
                "MOC internal URIs are read-only and cannot be write targets",
            ));
        }
        Err(ResourceError::new(
            "E_URI_SCHEME",
            "unsupported resource scheme",
        ))
    }

    pub fn complete(
        context: &ResourceContext<'_>,
        prefix: &str,
    ) -> Result<Vec<ResourceCandidate>, ResourceError> {
        let mut candidates = if prefix.is_empty() || prefix == "//" {
            scheme_candidates(context)
        } else if let Some(scheme) = prefix.strip_suffix(":/") {
            normalized_scheme_candidates(context, scheme)
        } else if prefix.starts_with("artifact://") {
            context_artifact_candidates(context, prefix)
        } else if prefix.starts_with("skill://") {
            skill_candidates(context, prefix)
        } else if prefix.starts_with("local://") {
            local_candidates(context, prefix)
        } else if prefix.starts_with("history://") {
            history_candidates(context, prefix)
        } else {
            Vec::new()
        };
        candidates.sort_by(|left, right| {
            left.uri
                .as_bytes()
                .cmp(right.uri.as_bytes())
                .then_with(|| left.label.as_bytes().cmp(right.label.as_bytes()))
        });
        candidates.dedup_by(|left, right| left.uri == right.uri);
        candidates.truncate(20);
        Ok(candidates)
    }

    fn resolve_context(
        context: &ResourceContext<'_>,
        input: &str,
    ) -> Result<Option<ResolvedResource>, ResourceError> {
        let uri = InternalUri::parse(input)
            .map_err(|error| ResourceError::new(error.code, error.message))?;
        match uri.scheme() {
            "artifact" => ArtifactStore::new(context.session_id, context.paths.clone())
                .resolve_uri(input)
                .map(Some),
            "skill" => resolve_skill(context, &uri, input).map(Some),
            "local" => resolve_local(context, &uri, input).map(Some),
            "history" => resolve_history(context, &uri, input).map(Some),
            _ => Err(ResourceError::new(
                "E_URI_SCHEME",
                format!("unsupported resource scheme `{}`", uri.scheme()),
            )),
        }
    }
}

fn scheme_candidates(_context: &ResourceContext<'_>) -> Vec<ResourceCandidate> {
    vec![
        namespace("artifact://", "current session tool output", "artifact"),
        namespace("history://", "current session transcript", "history"),
        namespace("local://", "current session plan files", "local"),
        namespace("skill://", "discovered agent skill", "skill"),
        namespace("viking://", "remote OpenViking resource", "provider"),
    ]
}

fn normalized_scheme_candidates(
    context: &ResourceContext<'_>,
    scheme: &str,
) -> Vec<ResourceCandidate> {
    if !matches!(
        scheme,
        "artifact" | "history" | "local" | "skill" | "viking"
    ) {
        return Vec::new();
    }
    if scheme == "local" {
        return local_candidates(context, "local://");
    }
    if scheme == "viking" && !context.openviking_configured {
        return Vec::new();
    }
    vec![namespace(
        &format!("{scheme}://"),
        "standard resource URI prefix",
        "namespace",
    )]
}

fn namespace(uri: &str, description: &str, kind: &str) -> ResourceCandidate {
    ResourceCandidate {
        uri: uri.into(),
        label: uri.into(),
        description: description.into(),
        kind: kind.into(),
        append_space: false,
    }
}

fn context_artifact_candidates(
    context: &ResourceContext<'_>,
    prefix: &str,
) -> Vec<ResourceCandidate> {
    let store = ArtifactStore::new(context.session_id, context.paths.clone());
    store
        .recent_candidates()
        .into_iter()
        .filter(|candidate| candidate.uri.starts_with(prefix))
        .collect()
}

fn skill_candidates(context: &ResourceContext<'_>, prefix: &str) -> Vec<ResourceCandidate> {
    let mut values = Vec::new();
    for skill in context.skills.list() {
        let root = format!("skill://{}", skill.name);
        if root.starts_with(prefix) || prefix == "skill://" {
            values.push(ResourceCandidate {
                uri: root.clone(),
                label: skill.name.clone(),
                description: skill.description.clone(),
                kind: "skill".into(),
                append_space: true,
            });
        }
        let resource_prefix = format!("{root}/");
        if prefix == resource_prefix {
            for name in list_skill_resources(&skill.base_dir) {
                values.push(ResourceCandidate {
                    uri: format!("{resource_prefix}{name}"),
                    label: name,
                    description: "skill-bundled resource".into(),
                    kind: "file".into(),
                    append_space: true,
                });
            }
        }
    }
    values
}

fn local_candidates(context: &ResourceContext<'_>, prefix: &str) -> Vec<ResourceCandidate> {
    let root = format!("local://{}/plans/", context.session_id);
    if prefix == "local://" || prefix == root || root.starts_with(prefix) {
        let mut values = vec![ResourceCandidate {
            uri: root.clone(),
            label: "plans/".into(),
            description: "current session plan directory".into(),
            kind: "namespace".into(),
            append_space: false,
        }];
        if prefix == root
            && let Ok(entries) = fs::read_dir(context.paths.plans())
        {
            values.extend(entries.flatten().filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                (entry.file_type().ok()?.is_file() && name.ends_with(".md")).then(|| {
                    let uri = format!("{root}{name}");
                    ResourceCandidate {
                        uri: uri.clone(),
                        label: name,
                        description: "read-only plan for the current session".into(),
                        kind: "file".into(),
                        append_space: true,
                    }
                })
            }));
        }
        return values;
    }
    Vec::new()
}

fn history_candidates(context: &ResourceContext<'_>, prefix: &str) -> Vec<ResourceCandidate> {
    let transcript = format!("history://{}/transcript", context.session_id);
    let mut values = Vec::new();
    if transcript.starts_with(prefix) || prefix == "history://" {
        values.push(ResourceCandidate {
            uri: transcript,
            label: "transcript".into(),
            description: "current session JSONL transcript".into(),
            kind: "file".into(),
            append_space: true,
        });
    }
    let index = format!("history://{}/compaction/INDEX.md", context.session_id);
    if context.paths.compaction().join("INDEX.md").is_file()
        && (index.starts_with(prefix) || prefix == "history://")
    {
        values.push(ResourceCandidate {
            uri: index,
            label: "compaction index".into(),
            description: "pre-compaction history segment index".into(),
            kind: "file".into(),
            append_space: true,
        });
    }
    values
}

fn resolve_skill(
    context: &ResourceContext<'_>,
    uri: &InternalUri,
    input: &str,
) -> Result<ResolvedResource, ResourceError> {
    let skill = context
        .skills
        .get(uri.authority())
        .ok_or_else(|| ResourceError::new("E_SKILL_NOT_FOUND", "skill was not found"))?;
    let relative = if uri.path_segments().is_empty() {
        PathBuf::from("SKILL.md")
    } else {
        let mut path = PathBuf::new();
        for segment in uri.path_segments() {
            if segment.starts_with('.') || segment == ".." {
                return Err(ResourceError::new(
                    "E_SKILL_PATH",
                    "skill path contains an invalid segment",
                ));
            }
            path.push(segment);
        }
        path
    };
    let path = checked_child(
        &skill.base_dir,
        &relative,
        "E_SKILL_PATH",
        "E_SKILL_NOT_FOUND",
    )?;
    Ok(ResolvedResource {
        uri: input.into(),
        display_name: input.into(),
        read_only: true,
        backing: ResourceBacking::File(path),
    })
}

fn resolve_local(
    context: &ResourceContext<'_>,
    uri: &InternalUri,
    input: &str,
) -> Result<ResolvedResource, ResourceError> {
    if uri.authority() != context.session_id {
        return Err(ResourceError::new(
            "E_LOCAL_SCOPE",
            "local resource belongs to a different session",
        ));
    }
    let [namespace, name] = uri.two_segments("E_LOCAL_URI")?;
    if namespace != "plans" || !name.ends_with(".md") {
        return Err(ResourceError::new(
            "E_LOCAL_URI",
            "local supports only plans/*.md",
        ));
    }
    let path = checked_child(
        &context.paths.plans(),
        name,
        "E_LOCAL_URI",
        "E_LOCAL_NOT_FOUND",
    )?;
    Ok(ResolvedResource {
        uri: input.into(),
        display_name: input.into(),
        read_only: true,
        backing: ResourceBacking::File(path),
    })
}

fn resolve_history(
    context: &ResourceContext<'_>,
    uri: &InternalUri,
    input: &str,
) -> Result<ResolvedResource, ResourceError> {
    if uri.authority() != context.session_id {
        return Err(ResourceError::new(
            "E_HISTORY_SCOPE",
            "history resource belongs to a different session",
        ));
    }
    let segments = uri.path_segments();
    let path = if segments.len() == 1 && segments[0] == "transcript" {
        fs::canonicalize(&context.paths.transcript)
            .map_err(|error| ResourceError::new("E_HISTORY_NOT_FOUND", error.to_string()))?
    } else if segments.len() == 2
        && segments[0] == "compaction"
        && (segments[1] == "INDEX.md" || segments[1].starts_with("segment-"))
    {
        let candidate = checked_child(
            &context.paths.compaction(),
            &segments[1],
            "E_HISTORY_URI",
            "E_HISTORY_NOT_FOUND",
        )?;
        fs::canonicalize(candidate)
            .map_err(|error| ResourceError::new("E_HISTORY_NOT_FOUND", error.to_string()))?
    } else {
        return Err(ResourceError::new(
            "E_HISTORY_URI",
            "history supports only transcript or compaction segments",
        ));
    };
    Ok(ResolvedResource {
        uri: input.into(),
        display_name: input.into(),
        read_only: true,
        backing: ResourceBacking::File(path),
    })
}

fn checked_child(
    root: &Path,
    relative: impl AsRef<Path>,
    path_code: &'static str,
    not_found_code: &'static str,
) -> Result<PathBuf, ResourceError> {
    let relative = relative.as_ref();
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
    {
        return Err(ResourceError::new(
            path_code,
            "resource path escapes the allowed directory",
        ));
    }
    let root =
        fs::canonicalize(root).map_err(|error| ResourceError::new(path_code, error.to_string()))?;
    let path = root.join(relative);
    let canonical = fs::canonicalize(&path)
        .map_err(|error| ResourceError::new(not_found_code, error.to_string()))?;
    if !canonical.starts_with(&root) || canonical == root {
        return Err(ResourceError::new(
            path_code,
            "resource path escapes the allowed directory",
        ));
    }
    if !canonical.is_file() {
        return Err(ResourceError::new(not_found_code, "resource is not a file"));
    }
    Ok(canonical)
}

fn list_skill_resources(base: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    let mut names = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "SKILL.md" || name.starts_with('.') {
                return None;
            }
            let is_dir = entry.file_type().ok()?.is_dir();
            Some(if is_dir { format!("{name}/") } else { name })
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_uri_is_session_scoped_and_canonical() {
        let root = tempfile::tempdir().unwrap();
        let paths = SessionPaths {
            transcript: root.path().join("transcript.jsonl"),
            sidecar: root.path().join("sidecar"),
        };
        fs::create_dir_all(paths.jobs()).unwrap();
        fs::create_dir_all(paths.artifacts()).unwrap();
        let path = paths.artifacts().join("out.txt");
        fs::write(&path, "payload").unwrap();
        let store = ArtifactStore::new("ses_1", paths.clone());
        let uri = store.uri_for_path(&path).unwrap();
        assert_eq!(uri, "artifact://ses_1/artifacts/out.txt");
        assert!(store.resolve_uri(&uri).unwrap().read_only);
        assert_eq!(
            store
                .resolve_uri("artifact://ses_2/artifacts/out.txt")
                .unwrap_err()
                .code,
            "E_ARTIFACT_SCOPE"
        );
    }

    #[test]
    fn completion_exposes_only_registered_schemes_and_normalizes_local_prefix() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = SessionPaths {
            transcript: root.path().join("transcript.jsonl"),
            sidecar: root.path().join("sidecar"),
        };
        let sessions = SessionTable::new(root.path().join("sessions"), false);
        let skills = SkillIndex::empty();
        let context = ResourceContext {
            session_id: "ses_1",
            cwd: root.path(),
            paths: &paths,
            skills: &skills,
            sessions: &sessions,
            openviking_configured: false,
        };
        let root_candidates = ResourceRouter::complete(&context, "//").expect("根候选");
        assert!(
            root_candidates
                .iter()
                .any(|candidate| candidate.uri == "viking://")
        );
        assert!(
            !root_candidates
                .iter()
                .any(|candidate| candidate.uri == "proc://")
        );
        assert!(root_candidates.len() <= 20);
        let local = ResourceRouter::complete(&context, "local:/").expect("local 前缀");
        assert!(
            local
                .iter()
                .any(|candidate| candidate.uri == "local://ses_1/plans/")
        );
    }

    #[test]
    fn write_rejects_native_resources() {
        assert_eq!(
            ResourceRouter::write("skill://demo").unwrap_err().code,
            "E_URI_READ_ONLY"
        );
        assert_eq!(
            ResourceRouter::write("unknown://value").unwrap_err().code,
            "E_URI_SCHEME"
        );
    }
}
