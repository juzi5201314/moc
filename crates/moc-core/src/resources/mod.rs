//! MOC 内部资源 URI：统一解析、读取与输入补全。

mod uri;

pub use uri::{InternalUri, ResourceScheme};

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// 资源的物理承载；`Namespace` 是命名空间/根，自身没有单一路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceBacking {
    File(PathBuf),
    /// 命名空间/根：`listing` 是 read 展示的列表正文；`entries` 是
    /// （URI 前缀, 真实目录）对，grep 逐目录扫描后按前缀拼出条目 URI。
    Namespace {
        listing: String,
        entries: Vec<(String, PathBuf)>,
    },
}

/// 读取解析后的资源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedResource {
    pub uri: String,
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
}

/// 命名空间列表里最多列出的条目数；超出部分只给计数。
const LISTING_LIMIT: usize = 20;

/// 空命名空间的正文；与 `read` 的空目录文案同形。
const EMPTY_NAMESPACE: &str = "(empty namespace)";

/// artifact sidecar 的唯一 URI/路径映射实现。
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    paths: SessionPaths,
}

impl ArtifactStore {
    pub fn new(paths: SessionPaths) -> Self {
        Self { paths }
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
                return Ok(format!("artifact://{kind}/{}", relative.to_string_lossy()));
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
        if input == "artifact://" {
            let listing = [
                ("artifact://jobs", self.paths.jobs()),
                ("artifact://artifacts", self.paths.artifacts()),
            ]
            .into_iter()
            .map(|(uri, directory)| format!("{uri}  {} files", file_count(&directory)))
            .collect::<Vec<_>>()
            .join("\n");
            return Ok(ResolvedResource {
                uri: input.to_owned(),
                backing: ResourceBacking::Namespace {
                    listing,
                    entries: vec![
                        ("artifact://jobs/".to_owned(), self.paths.jobs()),
                        ("artifact://artifacts/".to_owned(), self.paths.artifacts()),
                    ],
                },
            });
        }
        let uri = InternalUri::parse(input)
            .map_err(|error| ResourceError::new("E_ARTIFACT_URI", error.message))?;
        let (kind, root) = match uri.authority() {
            "jobs" => ("jobs", self.paths.jobs()),
            "artifacts" => ("artifacts", self.paths.artifacts()),
            _ => return Err(ResourceError::new("E_ARTIFACT_URI", artifact_shape())),
        };
        let prefix = format!("artifact://{kind}/");
        match uri.segments() {
            [] => Ok(ResolvedResource {
                uri: input.to_owned(),
                backing: ResourceBacking::Namespace {
                    listing: render_listing(Vec::new(), directory_rows(&prefix, &root)),
                    entries: vec![(prefix, root)],
                },
            }),
            [name] => {
                if name.starts_with('.') {
                    return Err(ResourceError::new(
                        "E_ARTIFACT_PATH",
                        "artifact hidden files are not allowed",
                    ));
                }
                let path = checked_child(&root, name, "E_ARTIFACT_PATH", "E_ARTIFACT_NOT_FOUND")?;
                Ok(ResolvedResource {
                    uri: input.to_owned(),
                    backing: ResourceBacking::File(path),
                })
            }
            _ => Err(ResourceError::new("E_ARTIFACT_URI", artifact_shape())),
        }
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
                let uri = format!("artifact://{kind}/{}", entry.file_name().to_string_lossy());
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

/// 是否为 MOC 资源 URI。
///
/// 只有注册过的 scheme 才算资源 URI；含 `://` 的其它字符串是合法本地路径，
/// 工具必须按文件系统路径处理，不能按 URI 报错。
pub fn is_resource_uri(input: &str) -> bool {
    resource_scheme(input).is_some()
}

/// 读取输入前缀里的已注册 scheme；不是资源 URI 时返回 `None`。
fn resource_scheme(input: &str) -> Option<ResourceScheme> {
    let (scheme, _) = input.split_once("://")?;
    ResourceScheme::parse(scheme)
}

/// 所有 MOC-native URI 的统一入口。
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceRouter;

impl ResourceRouter {
    pub fn resolve_read(
        ctx: &crate::tools::ToolCtx,
        input: &str,
    ) -> Result<Option<ResolvedResource>, ResourceError> {
        // 未注册 scheme 的 `://` 字符串是合法本地路径，必须交给文件系统解析。
        let Some(scheme) = resource_scheme(input) else {
            return Ok(None);
        };
        let context = ctx.resource_context();
        if scheme == ResourceScheme::History {
            return resolve_history(&context, input, &ctx.sandbox_policy()).map(Some);
        }
        Self::resolve_context(&context, scheme, input)
    }

    pub fn write(input: &str) -> Result<(), ResourceError> {
        match resource_scheme(input) {
            // 未注册 scheme 的 `://` 字符串按本地路径写入；内部 URI 只读。
            None => Ok(()),
            Some(_) => Err(ResourceError::new(
                "E_URI_READ_ONLY",
                "session resource URIs are read-only; plans are saved with the plan tool",
            )),
        }
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
        scheme: ResourceScheme,
        input: &str,
    ) -> Result<Option<ResolvedResource>, ResourceError> {
        match scheme {
            ResourceScheme::Artifact => ArtifactStore::new(context.paths.clone())
                .resolve_uri(input)
                .map(Some),
            ResourceScheme::Skill => resolve_skill(context, input).map(Some),
            ResourceScheme::Local => resolve_local(context, input).map(Some),
            ResourceScheme::History => resolve_history(
                context,
                input,
                &crate::sandbox::SandboxPolicy::full(context.cwd.to_owned()),
            )
            .map(Some),
        }
    }
}

fn scheme_candidates(_context: &ResourceContext<'_>) -> Vec<ResourceCandidate> {
    vec![
        namespace("artifact://", "current session tool output", "artifact"),
        namespace("history://", "current session transcript", "history"),
        namespace("local://", "current session plan files", "local"),
        namespace("skill://", "discovered agent skill", "skill"),
    ]
}

fn normalized_scheme_candidates(
    context: &ResourceContext<'_>,
    scheme: &str,
) -> Vec<ResourceCandidate> {
    if !matches!(scheme, "artifact" | "history" | "local" | "skill") {
        return Vec::new();
    }
    if scheme == "local" {
        return local_candidates(context, "local://");
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
    let mut values = Vec::new();
    if prefix == "artifact://" {
        values.push(namespace(
            "artifact://jobs/",
            "current session job logs",
            "namespace",
        ));
        values.push(namespace(
            "artifact://artifacts/",
            "current session archived tool output",
            "namespace",
        ));
    }
    let store = ArtifactStore::new(context.paths.clone());
    values.extend(
        store
            .recent_candidates()
            .into_iter()
            .filter(|candidate| candidate.uri.starts_with(prefix)),
    );
    values
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
    let root = "local://";
    let mut values = Vec::new();
    if root.starts_with(prefix) {
        values.push(ResourceCandidate {
            uri: root.to_owned(),
            label: "plans/".into(),
            description: "current session plan directory".into(),
            kind: "namespace".into(),
            append_space: false,
        });
    }
    let Ok(entries) = fs::read_dir(context.paths.plans()) else {
        return values;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !name.ends_with(".md") {
            continue;
        }
        if !entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
            continue;
        }
        let uri = format!("local://{name}");
        if !uri.starts_with(prefix) {
            continue;
        }
        values.push(ResourceCandidate {
            uri,
            label: name,
            description: "read-only plan for the current session".into(),
            kind: "file".into(),
            append_space: true,
        });
    }
    values
}

fn history_candidates(context: &ResourceContext<'_>, prefix: &str) -> Vec<ResourceCandidate> {
    let transcript = "history://transcript";
    let mut values = Vec::new();
    if transcript.starts_with(prefix) {
        values.push(ResourceCandidate {
            uri: transcript.into(),
            label: "transcript".into(),
            description: "current session JSONL transcript".into(),
            kind: "file".into(),
            append_space: true,
        });
    }
    let index = "history://compaction/INDEX.md";
    if context.paths.compaction().join("INDEX.md").is_file() && index.starts_with(prefix) {
        values.push(ResourceCandidate {
            uri: index.into(),
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
    input: &str,
) -> Result<ResolvedResource, ResourceError> {
    if input == "skill://" {
        let mut entries = Vec::new();
        let mut lines = Vec::new();
        for skill in context.skills.list() {
            entries.push((format!("skill://{}/", skill.name), skill.base_dir.clone()));
            lines.push(format!("skill://{}  {}", skill.name, skill.description));
        }
        return Ok(ResolvedResource {
            uri: input.into(),
            backing: ResourceBacking::Namespace {
                listing: render_listing(lines, Vec::new()),
                entries,
            },
        });
    }
    let uri =
        InternalUri::parse(input).map_err(|error| ResourceError::new(error.code, error.message))?;
    let skill = context.skills.get(uri.authority()).ok_or_else(|| {
        // 名字猜错时直接给出可用的名字：让模型一次改对，而不是逐个试错。
        let known = context
            .skills
            .list()
            .take(12)
            .map(|meta| meta.name.as_str())
            .collect::<Vec<_>>();
        ResourceError::new(
            "E_SKILL_NOT_FOUND",
            if known.is_empty() {
                "skill was not found; this workspace discovers no skills".to_owned()
            } else {
                format!(
                    "skill was not found; known skills read as skill://<name>/SKILL.md: {}",
                    known.join(", ")
                )
            },
        )
    })?;
    let relative = if uri.segments().is_empty() {
        PathBuf::from("SKILL.md")
    } else {
        let mut path = PathBuf::new();
        for segment in uri.segments() {
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
        backing: ResourceBacking::File(path),
    })
}

fn resolve_local(
    context: &ResourceContext<'_>,
    input: &str,
) -> Result<ResolvedResource, ResourceError> {
    let directory = context.paths.plans();
    if input == "local://" {
        return Ok(ResolvedResource {
            uri: input.into(),
            backing: ResourceBacking::Namespace {
                listing: render_listing(Vec::new(), directory_rows("local://", &directory)),
                entries: vec![("local://".to_owned(), directory)],
            },
        });
    }
    let uri =
        InternalUri::parse(input).map_err(|error| ResourceError::new(error.code, error.message))?;
    let name = uri.authority();
    if !uri.segments().is_empty() || !name.ends_with(".md") {
        return Err(ResourceError::new(
            "E_LOCAL_URI",
            "local supports only local://<id>.md",
        ));
    }
    let path = checked_child(&directory, name, "E_LOCAL_URI", "E_LOCAL_NOT_FOUND")?;
    Ok(ResolvedResource {
        uri: input.into(),
        backing: ResourceBacking::File(path),
    })
}

fn resolve_history(
    context: &ResourceContext<'_>,
    input: &str,
    policy: &crate::sandbox::SandboxPolicy,
) -> Result<ResolvedResource, ResourceError> {
    let compaction = context.paths.compaction();
    if input == "history://" {
        let lead = history_transcript_row(context).into_iter().collect();
        return Ok(history_namespace(input, lead, &compaction));
    }
    let uri =
        InternalUri::parse(input).map_err(|error| ResourceError::new(error.code, error.message))?;
    let path = match (uri.authority(), uri.segments()) {
        ("transcript", []) => {
            if policy.denies_read(&context.paths.transcript)
                || policy.denies_read(&crate::session::log::history_view_path(context.paths))
            {
                return Err(ResourceError::new(
                    "E_SANDBOX_DENIED",
                    "transcript 源文件或镜像被禁止读取",
                ));
            }
            let exists = crate::session::log::refresh_history_view(context.paths)
                .map_err(|error| ResourceError::new("E_HISTORY_READ", error.to_string()))?;
            if !exists {
                return Err(ResourceError::new(
                    "E_HISTORY_NOT_FOUND",
                    "transcript 尚未落盘",
                ));
            }
            fs::canonicalize(crate::session::log::history_view_path(context.paths))
                .map_err(|error| ResourceError::new("E_HISTORY_NOT_FOUND", error.to_string()))?
        }
        ("compaction", []) => return Ok(history_namespace(input, Vec::new(), &compaction)),
        ("compaction", [name]) if name == "INDEX.md" || name.starts_with("segment-") => {
            let candidate =
                checked_child(&compaction, name, "E_HISTORY_URI", "E_HISTORY_NOT_FOUND")?;
            fs::canonicalize(candidate)
                .map_err(|error| ResourceError::new("E_HISTORY_NOT_FOUND", error.to_string()))?
        }
        _ => {
            return Err(ResourceError::new(
                "E_HISTORY_URI",
                "history supports only history://transcript or history://compaction/INDEX.md",
            ));
        }
    };
    Ok(ResolvedResource {
        uri: input.into(),
        backing: ResourceBacking::File(path),
    })
}

/// `history://` 与 `history://compaction` 的命名空间视图；两者都扫 compaction 目录。
fn history_namespace(uri: &str, lead: Vec<String>, compaction: &Path) -> ResolvedResource {
    ResolvedResource {
        uri: uri.to_owned(),
        backing: ResourceBacking::Namespace {
            listing: render_listing(lead, directory_rows("history://compaction/", compaction)),
            entries: vec![("history://compaction/".to_owned(), compaction.to_owned())],
        },
    }
}

/// transcript 在列表里的行；文件尚未落盘时为 `None`。
fn history_transcript_row(context: &ResourceContext<'_>) -> Option<String> {
    let metadata = fs::metadata(&context.paths.transcript).ok()?;
    Some(format!(
        "history://transcript  {} B  {}s ago",
        metadata.len(),
        age_secs(metadata.modified().unwrap_or(UNIX_EPOCH))
    ))
}

/// artifact URI 的合法形状；错误信息直接带上它，模型据此一次改对。
fn artifact_shape() -> String {
    "artifact URI must be artifact://jobs/<file> or artifact://artifacts/<file>; copy artifact URIs from notices or `job` op=list".to_owned()
}

/// 目录里可见文件的数量；目录缺失或不可读记 0。
fn file_count(directory: &Path) -> usize {
    let Ok(entries) = fs::read_dir(directory) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            !entry.file_name().to_string_lossy().starts_with('.')
                && entry.metadata().is_ok_and(|metadata| metadata.is_file())
        })
        .count()
}

/// 目录里的文件条目：`{uri}  {size} B  {age}s ago`，按 mtime 降序、同名按 URI 升序。
fn directory_rows(prefix: &str, directory: &Path) -> Vec<(SystemTime, String)> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        rows.push((
            modified,
            format!(
                "{prefix}{name}  {} B  {}s ago",
                metadata.len(),
                age_secs(modified)
            ),
        ));
    }
    rows.sort_unstable_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    rows
}

/// 命名空间正文：`lead` 是固定行，其后接目录行；总条数超过上限只给计数。
fn render_listing(lead: Vec<String>, rows: Vec<(SystemTime, String)>) -> String {
    let total = lead.len() + rows.len();
    if total == 0 {
        return EMPTY_NAMESPACE.to_owned();
    }
    let mut lines = lead;
    lines.extend(
        rows.into_iter()
            .take(LISTING_LIMIT.saturating_sub(lines.len()))
            .map(|(_, line)| line),
    );
    let mut text = lines.join("\n");
    if total > lines.len() {
        text.push_str(&format!("\n[… {} more]", total - lines.len()));
    }
    text
}

fn age_secs(modified: SystemTime) -> u64 {
    modified.elapsed().unwrap_or_default().as_secs()
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

    fn test_paths(root: &Path) -> SessionPaths {
        SessionPaths {
            transcript: root.join("transcript.jsonl"),
            sidecar: root.join("sidecar"),
        }
    }

    fn context_of<'a>(
        paths: &'a SessionPaths,
        cwd: &'a Path,
        skills: &'a SkillIndex,
        sessions: &'a SessionTable,
    ) -> ResourceContext<'a> {
        ResourceContext {
            session_id: "ses_1",
            cwd,
            paths,
            skills,
            sessions,
        }
    }

    #[test]
    fn artifact_uri_round_trips_without_a_session_segment() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        fs::create_dir_all(paths.jobs()).unwrap();
        fs::create_dir_all(paths.artifacts()).unwrap();
        let path = paths.artifacts().join("out.txt");
        fs::write(&path, "payload").unwrap();
        let store = ArtifactStore::new(paths.clone());
        let uri = store.uri_for_path(&path).unwrap();
        assert_eq!(uri, "artifact://artifacts/out.txt");
        assert_eq!(
            store.resolve_uri(&uri).unwrap().backing,
            ResourceBacking::File(path.canonicalize().unwrap())
        );
        // 猜错形状时必须一次说清正确形状与 URI 的来源，模型不能靠试错收敛。
        let wrong_kind = store.resolve_uri("artifact://tmp/out.txt").unwrap_err();
        assert_eq!(wrong_kind.code, "E_ARTIFACT_URI");
        assert!(wrong_kind.message.contains("artifact://jobs/<file>"));
        assert!(wrong_kind.message.contains("`job` op=list"));
        // 旧形状（会话段）同样落在这条错误上：一条消息就说明新形状。
        let old_shape = store
            .resolve_uri("artifact://ses_1/jobs/job_1.log")
            .unwrap_err();
        assert_eq!(old_shape.code, "E_ARTIFACT_URI");
        assert!(old_shape.message.contains("artifact://jobs/<file>"));
        let extra_segment = store.resolve_uri("artifact://jobs/a/b.log").unwrap_err();
        assert_eq!(extra_segment.code, "E_ARTIFACT_URI");
        let hidden = store.resolve_uri("artifact://jobs/.job_1.log").unwrap_err();
        assert_eq!(hidden.code, "E_ARTIFACT_PATH");
        let missing = store.resolve_uri("artifact://jobs/job_9.log").unwrap_err();
        assert_eq!(missing.code, "E_ARTIFACT_NOT_FOUND");
    }

    /// 两个 kind 目录都能作为命名空间列出：列表给 read，entries 给 grep 扫描。
    #[test]
    fn artifact_namespaces_list_files_and_expose_scan_entries() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        fs::create_dir_all(paths.jobs()).unwrap();
        fs::create_dir_all(paths.artifacts()).unwrap();
        fs::write(paths.jobs().join("job_1.log"), "alpha\n").unwrap();
        fs::write(paths.jobs().join(".hidden.log"), "noise\n").unwrap();
        fs::write(paths.artifacts().join("out.txt"), "payload").unwrap();
        let store = ArtifactStore::new(paths.clone());

        let root_ns = store.resolve_uri("artifact://").unwrap();
        let ResourceBacking::Namespace { listing, entries } = &root_ns.backing else {
            panic!("artifact:// 必须是命名空间：{:?}", root_ns.backing);
        };
        assert_eq!(
            listing,
            "artifact://jobs  1 files\nartifact://artifacts  1 files"
        );
        assert_eq!(
            entries,
            &vec![
                ("artifact://jobs/".to_owned(), paths.jobs()),
                ("artifact://artifacts/".to_owned(), paths.artifacts()),
            ]
        );

        let jobs = store.resolve_uri("artifact://jobs").unwrap();
        let ResourceBacking::Namespace { listing, entries } = &jobs.backing else {
            panic!("artifact://jobs 必须是命名空间：{:?}", jobs.backing);
        };
        assert!(
            listing.starts_with("artifact://jobs/job_1.log  6 B  "),
            "{listing}"
        );
        assert!(!listing.contains(".hidden.log"), "{listing}");
        assert_eq!(
            entries,
            &vec![("artifact://jobs/".to_owned(), paths.jobs())]
        );
    }

    /// 目录为空或不存在时列表必须自解释，而不是留一片空白。
    #[test]
    fn empty_artifact_namespace_reports_itself() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        let store = ArtifactStore::new(paths.clone());
        let empty = store.resolve_uri("artifact://artifacts").unwrap();
        let ResourceBacking::Namespace { listing, .. } = &empty.backing else {
            panic!("artifact://artifacts 必须是命名空间");
        };
        assert_eq!(listing, "(empty namespace)");
    }

    #[test]
    fn completion_exposes_only_registered_schemes_and_normalizes_local_prefix() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = test_paths(root.path());
        fs::create_dir_all(paths.jobs()).expect("jobs 目录");
        fs::write(paths.jobs().join("job_1.log"), "alpha\n").expect("job 日志");
        let sessions = SessionTable::new(root.path().join("sessions"), false);
        let skills = SkillIndex::empty();
        let context = context_of(&paths, root.path(), &skills, &sessions);

        let root_candidates = ResourceRouter::complete(&context, "//").expect("根候选");
        assert!(
            !root_candidates
                .iter()
                .any(|candidate| candidate.uri == "proc://")
        );
        assert!(root_candidates.len() <= 20);
        let local = ResourceRouter::complete(&context, "local:/").expect("local 前缀");
        assert!(local.iter().any(|candidate| candidate.uri == "local://"));
        let artifacts = ResourceRouter::complete(&context, "artifact://").expect("artifact 前缀");
        assert!(
            artifacts
                .iter()
                .any(|candidate| candidate.uri == "artifact://jobs/")
        );
        assert!(
            artifacts
                .iter()
                .any(|candidate| candidate.uri == "artifact://artifacts/")
        );
        assert!(
            artifacts
                .iter()
                .any(|candidate| candidate.uri == "artifact://jobs/job_1.log")
        );
        let history = ResourceRouter::complete(&context, "history://").expect("history 前缀");
        assert!(
            history
                .iter()
                .any(|candidate| candidate.uri == "history://transcript")
        );
    }

    /// 根 URI 直接列出内容：模型不抄 id、不猜形状，一次 read 就看到全部候选。
    #[test]
    fn local_root_lists_plans_and_plan_uri_resolves() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = test_paths(root.path());
        fs::create_dir_all(paths.plans()).expect("plans 目录");
        fs::write(paths.plans().join("plan_1.md"), "# plan\n").expect("计划");
        let sessions = SessionTable::new(root.path().join("sessions"), false);
        let skills = SkillIndex::empty();
        let context = context_of(&paths, root.path(), &skills, &sessions);

        let listing = ResourceRouter::resolve_context(&context, ResourceScheme::Local, "local://")
            .expect("local 根")
            .expect("local 是资源");
        let ResourceBacking::Namespace { listing, entries } = &listing.backing else {
            panic!("local:// 必须是命名空间：{:?}", listing.backing);
        };
        assert!(listing.starts_with("local://plan_1.md  7 B  "), "{listing}");
        assert_eq!(entries, &vec![("local://".to_owned(), paths.plans())]);

        let file =
            ResourceRouter::resolve_context(&context, ResourceScheme::Local, "local://plan_1.md")
                .expect("计划 URI")
                .expect("计划是资源");
        assert_eq!(
            file.backing,
            ResourceBacking::File(paths.plans().join("plan_1.md").canonicalize().unwrap())
        );

        let wrong = ResourceRouter::resolve_context(
            &context,
            ResourceScheme::Local,
            "local://ses_1/plans/plan_1.md",
        )
        .unwrap_err();
        assert_eq!(wrong.code, "E_LOCAL_URI");
        assert!(
            wrong.message.contains("local://<id>.md"),
            "{}",
            wrong.message
        );
    }

    #[test]
    fn history_root_lists_transcript_and_compaction_segments() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = test_paths(root.path());
        fs::create_dir_all(paths.compaction()).expect("compaction 目录");
        fs::write(&paths.transcript, "{}\n").expect("transcript");
        fs::write(paths.compaction().join("INDEX.md"), "# index\n").expect("索引");
        let sessions = SessionTable::new(root.path().join("sessions"), false);
        let skills = SkillIndex::empty();
        let context = context_of(&paths, root.path(), &skills, &sessions);

        let history =
            ResourceRouter::resolve_context(&context, ResourceScheme::History, "history://")
                .expect("history 根")
                .expect("history 是资源");
        let ResourceBacking::Namespace { listing, entries } = &history.backing else {
            panic!("history:// 必须是命名空间：{:?}", history.backing);
        };
        assert!(
            listing.starts_with("history://transcript  3 B  "),
            "{listing}"
        );
        assert!(
            listing.contains("history://compaction/INDEX.md  8 B  "),
            "{listing}"
        );
        assert_eq!(
            entries,
            &vec![("history://compaction/".to_owned(), paths.compaction())]
        );

        let transcript = ResourceRouter::resolve_context(
            &context,
            ResourceScheme::History,
            "history://transcript",
        )
        .expect("transcript URI")
        .expect("transcript 是资源");
        assert_eq!(
            transcript.backing,
            ResourceBacking::File(paths.transcript.canonicalize().unwrap())
        );

        let wrong = ResourceRouter::resolve_context(
            &context,
            ResourceScheme::History,
            "history://ses_1/transcript",
        )
        .unwrap_err();
        assert_eq!(wrong.code, "E_HISTORY_URI");
        assert!(
            wrong.message.contains("history://transcript"),
            "{}",
            wrong.message
        );
    }

    #[test]
    fn history_source_denials_block_decoding_but_keep_namespaces() {
        use crate::sandbox::{SandboxGrants, SandboxPolicy};
        use moc_protocol::SessionPermission;

        let root = tempfile::tempdir().expect("临时目录");
        let mut paths = test_paths(root.path());
        paths.transcript = root.path().join("transcript.jsonl.zst");
        fs::write(
            &paths.transcript,
            crate::session::log::compress_frame(b"{\"a\":1}\n").unwrap(),
        )
        .unwrap();
        fs::create_dir_all(paths.compaction()).unwrap();
        fs::write(paths.compaction().join("INDEX.md"), "索引").unwrap();
        let sessions = SessionTable::new(root.path().join("sessions"), false);
        let skills = SkillIndex::empty();
        let context = context_of(&paths, root.path(), &skills, &sessions);
        let mut config = crate::ResolvedConfig::load(&crate::CoreConfig {
            home: root.path().join("home"),
            cwd: root.path().to_owned(),
            ..Default::default()
        })
        .unwrap()
        .sandbox;
        config.deny_read_paths = vec![paths.transcript.clone()];
        let policy = SandboxPolicy::new(
            SessionPermission::Workspace,
            root.path().to_owned(),
            &config,
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            None,
        );
        for uri in ["history://transcript", "history://%74ranscript"] {
            assert_eq!(
                resolve_history(&context, uri, &policy).unwrap_err().code,
                "E_SANDBOX_DENIED"
            );
        }
        assert!(!crate::session::log::history_view_path(&paths).exists());
        assert!(resolve_history(&context, "history://", &policy).is_ok());
        assert!(resolve_history(&context, "history://compaction/INDEX.md", &policy).is_ok());
        assert_eq!(
            resolve_history(&context, "history://transcript/invalid", &policy)
                .unwrap_err()
                .code,
            "E_HISTORY_URI"
        );
        assert!(
            resolve_history(
                &context,
                "history://transcript",
                &SandboxPolicy::full(root.path().to_owned())
            )
            .is_ok()
        );
    }

    #[test]
    fn compressed_history_transcript_is_decoded_into_a_mirror() {
        let root = tempfile::tempdir().expect("临时目录");
        let mut paths = test_paths(root.path());
        paths.transcript = root.path().join("transcript.jsonl.zst");
        let mut bytes = crate::session::log::compress_frame(b"{\"a\":1}\n").expect("压缩第一帧");
        bytes.extend(crate::session::log::compress_frame(b"{\"a\":2}\n").expect("压缩第二帧"));
        fs::write(&paths.transcript, bytes).expect("压缩 transcript");
        let sessions = SessionTable::new(root.path().join("sessions"), false);
        let skills = SkillIndex::empty();
        let context = context_of(&paths, root.path(), &skills, &sessions);

        let transcript = ResourceRouter::resolve_context(
            &context,
            ResourceScheme::History,
            "history://transcript",
        )
        .expect("transcript URI")
        .expect("transcript 是资源");
        let mirror = paths.sidecar.join("history/transcript.jsonl");
        assert_eq!(
            transcript.backing,
            ResourceBacking::File(mirror.canonicalize().unwrap())
        );
        assert_eq!(
            fs::read_to_string(&mirror).expect("读取镜像"),
            "{\"a\":1}\n{\"a\":2}\n",
            "压缩会话对外暴露完整解码后的日志"
        );
    }

    /// 列表超过上限时只给计数：正文保持有界，模型仍知道还有多少条。
    #[test]
    fn namespace_listing_truncates_and_counts_the_rest() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = test_paths(root.path());
        fs::create_dir_all(paths.jobs()).expect("jobs 目录");
        for index in 0..25 {
            fs::write(paths.jobs().join(format!("job_{index}.log")), "x").expect("job 日志");
        }
        let store = ArtifactStore::new(paths.clone());
        let jobs = store.resolve_uri("artifact://jobs").expect("jobs 命名空间");
        let ResourceBacking::Namespace { listing, .. } = &jobs.backing else {
            panic!("artifact://jobs 必须是命名空间");
        };
        assert_eq!(listing.lines().count(), 21, "{listing}");
        assert!(listing.ends_with("[… 5 more]"), "{listing}");
    }

    /// 名字猜错时错误里必须带上可用名字：模型一次改对，不再逐个试错。
    #[test]
    fn unknown_skill_error_lists_known_names() {
        let root = tempfile::tempdir().expect("临时目录");
        let cwd = root.path().join("repo");
        let home = root.path().join("home");
        fs::create_dir_all(cwd.join(".agents/skills/demo")).expect("skill 目录");
        fs::write(
            cwd.join(".agents/skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: demo skill\n---\nbody",
        )
        .expect("skill 正文");
        let skills =
            crate::skills::discovery::discover_index(&cwd, &home, Some(&home), &Default::default());
        let paths = test_paths(root.path());
        let sessions = SessionTable::new(root.path().join("sessions"), false);
        let context = context_of(&paths, &cwd, &skills, &sessions);

        let error = ResourceRouter::resolve_context(
            &context,
            ResourceScheme::Skill,
            "skill://missing/SKILL.md",
        )
        .unwrap_err();
        assert_eq!(error.code, "E_SKILL_NOT_FOUND");
        assert!(
            error.message.contains("skill://<name>/SKILL.md") && error.message.contains("demo"),
            "{}",
            error.message
        );

        let root_listing =
            ResourceRouter::resolve_context(&context, ResourceScheme::Skill, "skill://")
                .expect("skill 根")
                .expect("skill 是资源");
        let ResourceBacking::Namespace { listing, entries } = &root_listing.backing else {
            panic!("skill:// 必须是命名空间：{:?}", root_listing.backing);
        };
        assert_eq!(listing, "skill://demo  demo skill");
        assert_eq!(
            entries,
            &vec![(
                "skill://demo/".to_owned(),
                cwd.join(".agents/skills/demo").canonicalize().unwrap()
            )]
        );

        let skill =
            ResourceRouter::resolve_context(&context, ResourceScheme::Skill, "skill://demo")
                .expect("skill URI")
                .expect("skill 是资源");
        assert_eq!(
            skill.backing,
            ResourceBacking::File(
                cwd.join(".agents/skills/demo/SKILL.md")
                    .canonicalize()
                    .unwrap()
            )
        );
    }

    #[test]
    fn resource_uri_predicate_covers_registered_schemes_only() {
        for uri in [
            "artifact://jobs/job.log",
            "skill://demo/SKILL.md",
            "local://plan_1.md",
            "history://transcript",
        ] {
            assert!(is_resource_uri(uri), "{uri}");
        }
        for path in ["plain://dir/file", "/tmp/report://draft", "src/main.rs", ""] {
            assert!(!is_resource_uri(path), "{path}");
        }
    }

    #[test]
    fn write_rejects_native_resources() {
        for input in [
            "skill://demo",
            "artifact://jobs/a.log",
            "local://plan_1.md",
            "history://transcript",
        ] {
            assert_eq!(
                ResourceRouter::write(input).unwrap_err().code,
                "E_URI_READ_ONLY",
                "{input}"
            );
        }
    }

    #[test]
    fn local_paths_with_scheme_separators_are_not_resource_uris() {
        for path in ["plain://dir/file", "/tmp/report://draft", "unknown://value"] {
            assert!(ResourceRouter::write(path).is_ok(), "{path}");
            assert!(!is_resource_uri(path), "{path}");
        }
    }
}
