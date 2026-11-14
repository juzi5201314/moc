use super::DirRow;
use super::EMPTY_NAMESPACE;
use super::LISTING_LIMIT;
use super::ListedEntry;
use super::ResourceContext;
use super::ResourceError;
use super::ResourceListing;
use super::artifact_shape;
use super::completion::tool_documentation;
use super::resource_scheme;
use crate::resources::InternalUri;
use crate::resources::ResourceScheme;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// `resource/list`：把内部 URI 展开成结构化的一层条目。
///
/// URI→目录/文件的映射与 [`ResourceRouter`] 的 resolve 系列同一份规则；
/// 文件 URI 不是命名空间，报错（该用 `resource/read`）。
pub fn list(context: &ResourceContext<'_>, input: &str) -> Result<ResourceListing, ResourceError> {
    if input.is_empty() {
        return Ok(ResourceListing {
            uri: input.to_owned(),
            entries: [
                "local://",
                "artifact://jobs",
                "artifact://artifacts",
                "history://",
                "skill://",
                "tool://",
            ]
            .into_iter()
            .map(ListedEntry::namespace)
            .collect(),
            truncated: false,
        });
    }
    let mut entries = match resource_scheme(input) {
        Some(ResourceScheme::Artifact) => list_artifact(context, input)?,
        Some(ResourceScheme::Local) => list_local(context, input)?,
        Some(ResourceScheme::History) => list_history(context, input)?,
        Some(ResourceScheme::Skill) => list_skill(context, input)?,
        Some(ResourceScheme::Tool) => {
            if input != "tool://" {
                return Err(ResourceError::new(
                    "E_TOOL_DOC",
                    "Tool documents are not directories; use resource/read",
                ));
            }
            tool_documentation(context)?
                .topics()
                .map(|(name, description)| ListedEntry {
                    uri: format!("tool://{name}"),
                    name: name.to_owned(),
                    is_dir: false,
                    size: None,
                    modified_ms: None,
                    description: Some(description.to_owned()),
                })
                .collect()
        }
        None => {
            return Err(ResourceError::new("E_URI_INVALID", "unknown resource URI"));
        }
    };
    // 目录在前、文件在后，各自按名称不区分大小写排序。
    entries.sort_by(|left, right| {
        (!left.is_dir)
            .cmp(&!right.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    let truncated = entries.len() > moc_protocol::FS_LIST_DIR_MAX_ENTRIES;
    entries.truncate(moc_protocol::FS_LIST_DIR_MAX_ENTRIES);
    Ok(ResourceListing {
        uri: input.to_owned(),
        entries,
        truncated,
    })
}

pub(super) fn list_artifact(
    context: &ResourceContext<'_>,
    input: &str,
) -> Result<Vec<ListedEntry>, ResourceError> {
    if input == "artifact://" {
        return Ok(vec![
            ListedEntry::namespace("artifact://jobs"),
            ListedEntry::namespace("artifact://artifacts"),
        ]);
    }
    let uri = InternalUri::parse(input)
        .map_err(|error| ResourceError::new("E_ARTIFACT_URI", error.message))?;
    let (prefix, root) = match uri.authority() {
        "jobs" => ("artifact://jobs/", context.paths.jobs()),
        "artifacts" => ("artifact://artifacts/", context.paths.artifacts()),
        _ => return Err(ResourceError::new("E_ARTIFACT_URI", artifact_shape())),
    };
    if !uri.segments().is_empty() {
        return Err(ResourceError::new("E_ARTIFACT_URI", artifact_shape()));
    }
    Ok(directory_entries(prefix, &root)
        .into_iter()
        .map(ListedEntry::from)
        .collect())
}

pub(super) fn list_local(
    context: &ResourceContext<'_>,
    input: &str,
) -> Result<Vec<ListedEntry>, ResourceError> {
    if input != "local://" {
        return Err(ResourceError::new(
            "E_LOCAL_URI",
            "local supports only local:// listing",
        ));
    }
    // 与 resolve_local 同一份规则：只列 `*.md` 计划文件。
    Ok(directory_entries("local://", &context.paths.plans())
        .into_iter()
        .filter(|row| row.name.ends_with(".md"))
        .map(ListedEntry::from)
        .collect())
}

/// 与 resolve_history 的命名空间一致：`history://` 列出 transcript 与
/// compaction 下的条目；`history://compaction` 只列后者。
pub(super) fn list_history(
    context: &ResourceContext<'_>,
    input: &str,
) -> Result<Vec<ListedEntry>, ResourceError> {
    if !matches!(input, "history://" | "history://compaction") {
        return Err(ResourceError::new(
            "E_HISTORY_URI",
            "history supports only history:// or history://compaction listing",
        ));
    }
    let mut entries = Vec::new();
    if input == "history://"
        && let Ok(metadata) = fs::metadata(&context.paths.transcript)
    {
        entries.push(ListedEntry {
            uri: "history://transcript".to_owned(),
            name: "transcript".to_owned(),
            is_dir: false,
            size: None,
            modified_ms: metadata.modified().ok().and_then(to_ms),
            description: Some(
                "Readable history index; /role/user, /tool/<name>, /entry/<id> select records"
                    .into(),
            ),
        });
        entries.push(ListedEntry {
            uri: "history://raw".into(),
            name: "raw".into(),
            is_dir: false,
            size: Some(metadata.len()),
            modified_ms: metadata.modified().ok().and_then(to_ms),
            description: Some("Original session JSONL".into()),
        });
    }
    entries.extend(
        directory_entries("history://compaction/", &context.paths.compaction())
            .into_iter()
            .map(ListedEntry::from),
    );
    Ok(entries)
}

/// `skill://` → 每个技能一个 dir；`skill://<name>[/sub...]` → 该技能
/// base_dir 下对应层。与 resolve_skill 同一份段规则（拒绝 `.` 开头与 `..` 段）。
pub(super) fn list_skill(
    context: &ResourceContext<'_>,
    input: &str,
) -> Result<Vec<ListedEntry>, ResourceError> {
    if input == "skill://" {
        return Ok(context
            .skills
            .list()
            .map(|skill| ListedEntry {
                uri: format!("skill://{}", skill.name),
                name: skill.name.clone(),
                is_dir: true,
                size: None,
                modified_ms: None,
                description: Some(skill.description.clone()),
            })
            .collect());
    }
    let uri =
        InternalUri::parse(input).map_err(|error| ResourceError::new(error.code, error.message))?;
    let skill = context
        .skills
        .get(uri.authority())
        .ok_or_else(|| ResourceError::new("E_SKILL_NOT_FOUND", "skill was not found"))?;
    let mut relative = PathBuf::new();
    for segment in uri.segments() {
        if segment.starts_with('.') {
            return Err(ResourceError::new(
                "E_SKILL_PATH",
                "skill path contains an invalid segment",
            ));
        }
        relative.push(segment);
    }
    let base = fs::canonicalize(&skill.base_dir)
        .map_err(|error| ResourceError::new("E_SKILL_PATH", error.to_string()))?;
    let directory = fs::canonicalize(base.join(&relative))
        .map_err(|error| ResourceError::new("E_SKILL_NOT_FOUND", error.to_string()))?;
    if !directory.starts_with(&base) || !directory.is_dir() {
        return Err(ResourceError::new(
            "E_SKILL_PATH",
            "skill path escapes the skill directory or is not a directory",
        ));
    }
    let prefix = format!(
        "skill://{}",
        std::iter::once(uri.authority())
            .chain(uri.segments().iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("/")
    );
    let mut entries = Vec::new();
    let reader = fs::read_dir(&directory)
        .map_err(|error| ResourceError::new("E_SKILL_PATH", error.to_string()))?;
    for entry in reader.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let child_uri = format!("{prefix}/{name}");
        if file_type.is_dir() {
            entries.push(ListedEntry {
                uri: child_uri,
                name,
                is_dir: true,
                size: None,
                modified_ms: None,
                description: None,
            });
        } else if file_type.is_file()
            && let Ok(metadata) = entry.metadata()
        {
            entries.push(ListedEntry {
                uri: child_uri,
                name,
                is_dir: false,
                size: Some(metadata.len()),
                modified_ms: metadata.modified().ok().and_then(to_ms),
                description: None,
            });
        }
    }
    Ok(entries)
}

/// 命名空间正文：`lead` 是固定行，其后接目录行；总条数超过上限只给计数。
pub(super) fn render_listing(lead: Vec<String>, rows: Vec<(SystemTime, String)>) -> String {
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

pub(super) fn age_secs(modified: SystemTime) -> u64 {
    modified.elapsed().unwrap_or_default().as_secs()
}

pub(super) fn checked_child(
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

pub(super) fn list_skill_resources(base: &Path) -> Vec<String> {
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

pub(super) fn directory_entries(prefix: &str, directory: &Path) -> Vec<DirRow> {
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
        rows.push(DirRow {
            uri: format!("{prefix}{name}"),
            name,
            size: metadata.len(),
            modified,
        });
    }
    rows.sort_unstable_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.uri.cmp(&right.uri))
    });
    rows
}

/// 目录里的文件条目：`{uri}  {size} B  {age}s ago`，按 mtime 降序、同名按 URI 升序。
pub(super) fn directory_rows(prefix: &str, directory: &Path) -> Vec<(SystemTime, String)> {
    directory_entries(prefix, directory)
        .into_iter()
        .map(|row| {
            (
                row.modified,
                format!(
                    "{}  {} B  {}s ago",
                    row.uri,
                    row.size,
                    age_secs(row.modified)
                ),
            )
        })
        .collect()
}

pub(super) fn to_ms(time: SystemTime) -> Option<u64> {
    u64::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

/// 目录里可见文件的数量；目录缺失或不可读记 0。
pub(super) fn file_count(directory: &Path) -> usize {
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
