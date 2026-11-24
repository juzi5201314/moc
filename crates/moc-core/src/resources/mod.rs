//! MOC 内部资源 URI：统一解析、读取与输入补全。

pub(crate) mod history;
mod uri;

pub use uri::{InternalUri, ResourceScheme};

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    /// 按当前会话生成的只读文档，不落盘，也不参与文件编辑锚定。
    Text(String),
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
    /// `label` 中被查询命中的字符下标（Unicode 标量序号、升序），供前端高亮。
    pub matches: Vec<u32>,
}

/// 资源解析所需的当前会话视图。
pub struct ResourceContext<'ctx> {
    pub session_id: &'ctx str,
    pub cwd: &'ctx Path,
    pub paths: &'ctx SessionPaths,
    pub skills: &'ctx SkillIndex,
    pub sessions: &'ctx SessionTable,
    pub tools: Option<crate::tools::documentation::ToolDocumentation>,
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
                matches: Vec::new(),
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
    pub async fn resolve_read(
        ctx: &crate::tools::ToolCtx,
        input: &str,
    ) -> Result<Option<ResolvedResource>, ResourceError> {
        // 未注册 scheme 的 `://` 字符串是合法本地路径，必须交给文件系统解析。
        let Some(scheme) = resource_scheme(input) else {
            return Ok(None);
        };
        let context = ctx.resource_context().await;
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

    /// 按字符串 URI 解析；scheme 未注册时报错。供 `resource/read` 这类
    /// 没有 `ToolCtx` 的调用方使用，history 走 full sandbox（与
    /// [`Self::resolve_context`] 一致）。
    pub fn resolve(
        context: &ResourceContext<'_>,
        input: &str,
    ) -> Result<ResolvedResource, ResourceError> {
        let scheme = resource_scheme(input)
            .ok_or_else(|| ResourceError::new("E_URI_INVALID", "unknown resource URI"))?;
        Self::resolve_context(context, scheme, input)?
            .ok_or_else(|| ResourceError::new("E_URI_INVALID", "unknown resource URI"))
    }

    /// `kind` 只约束 `@` 工作区补全（`file` / `dir`），URI 补全忽略它。
    pub fn complete(
        context: &ResourceContext<'_>,
        prefix: &str,
        kind: Option<&str>,
    ) -> Result<Vec<ResourceCandidate>, ResourceError> {
        let mut candidates = if prefix.starts_with('@') {
            // `@` 附件引用有独立的模糊排序，不走后面的字典序排序。
            let candidates = attach::file_candidates(context, prefix, kind);
            return Ok(candidates);
        } else if prefix.is_empty() || prefix == "//" {
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
        } else if prefix.starts_with("tool://") {
            tool_candidates(context, prefix)
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
            ResourceScheme::Tool => Ok(Some(ResolvedResource {
                uri: input.to_owned(),
                backing: ResourceBacking::Text(
                    tool_documentation(context)?
                        .read(input)
                        .map_err(|error| ResourceError::new(error.code, error.message))?,
                ),
            })),
        }
    }
}

/// `@` 附件引用：补全与注入块标记（`wrap_attachment`/`parse_attachment` 供前端转写复用）。
pub mod attach;
mod resolution;
use resolution::resolve_history;
use resolution::resolve_local;
use resolution::resolve_skill;
mod completion;
use completion::context_artifact_candidates;
use completion::history_candidates;
use completion::local_candidates;
use completion::normalized_scheme_candidates;
use completion::scheme_candidates;
use completion::skill_candidates;
use completion::tool_candidates;
use completion::tool_documentation;
mod listing;
use listing::checked_child;
use listing::directory_rows;
use listing::file_count;
pub use listing::list;
use listing::render_listing;
use listing::to_ms;

/// artifact URI 的合法形状；错误信息直接带上它，模型据此一次改对。
fn artifact_shape() -> String {
    "artifact URI must be artifact://jobs/<file> or artifact://artifacts/<file>; copy artifact URIs from notices or `job` op=list".to_owned()
}

/// 目录里的文件条目（非隐藏普通文件）：mtime 降序、同名按 URI 升序。
/// 文本 listing 与 `resource/list` 的结构化列举共用这一条事实来源。
struct DirRow {
    name: String,
    uri: String,
    size: u64,
    modified: SystemTime,
}

/// `resource/list` 的一次列举结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceListing {
    /// 回显请求的 URI（根为 `""`）。
    pub uri: String,
    pub entries: Vec<ListedEntry>,
    /// 条目超过 [`moc_protocol::FS_LIST_DIR_MAX_ENTRIES`] 被截断。
    pub truncated: bool,
}

/// `resource/list` 的单条：`is_dir` 可继续列举，`!is_dir` 可 `resource/read`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedEntry {
    pub uri: String,
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified_ms: Option<u64>,
    pub description: Option<String>,
}

impl ListedEntry {
    fn namespace(uri: &str) -> Self {
        Self {
            uri: uri.to_owned(),
            name: uri.to_owned(),
            is_dir: true,
            size: None,
            modified_ms: None,
            description: None,
        }
    }
}

impl From<DirRow> for ListedEntry {
    fn from(row: DirRow) -> Self {
        Self {
            uri: row.uri,
            name: row.name,
            is_dir: false,
            size: Some(row.size),
            modified_ms: to_ms(row.modified),
            description: None,
        }
    }
}

#[cfg(test)]
mod tests;
