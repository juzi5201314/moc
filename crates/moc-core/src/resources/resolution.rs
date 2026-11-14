use super::ResolvedResource;
use super::ResourceBacking;
use super::ResourceContext;
use super::ResourceError;
use super::listing::age_secs;
use super::listing::checked_child;
use super::listing::directory_rows;
use super::listing::render_listing;
use crate::resources::InternalUri;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

pub(super) fn resolve_skill(
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

pub(super) fn resolve_local(
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

pub(super) fn resolve_history(
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
        ("transcript", _) | ("raw", []) => {
            let selection = if uri.authority() == "transcript" {
                Some(super::history::Selection::parse(&uri)?)
            } else {
                None
            };
            let target = selection.as_ref().map_or_else(
                || crate::session::log::history_view_path(context.paths),
                |selection| selection.path(context.paths),
            );
            if policy.denies_read(&context.paths.transcript)
                || policy.denies_read(&crate::session::log::history_view_path(context.paths))
                || policy.denies_read(&target)
            {
                return Err(ResourceError::new(
                    "E_SANDBOX_DENIED",
                    "transcript 源文件或镜像被禁止读取",
                ));
            }
            let exists = match selection {
                Some(selection) => selection.refresh(context.paths),
                None => crate::session::log::refresh_history_view(context.paths),
            }
            .map_err(|error| ResourceError::new("E_HISTORY_READ", error.to_string()))?;
            if !exists {
                return Err(ResourceError::new(
                    "E_HISTORY_NOT_FOUND",
                    "transcript 尚未落盘",
                ));
            }
            fs::canonicalize(target)
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
                "Use history://transcript, history://transcript/role/user, history://transcript/tool/<name>, history://transcript/entry/<id>, history://raw, or history://compaction/INDEX.md",
            ));
        }
    };
    Ok(ResolvedResource {
        uri: input.into(),
        backing: ResourceBacking::File(path),
    })
}

/// `history://` 与 `history://compaction` 的命名空间视图；两者都扫 compaction 目录。
pub(super) fn history_namespace(
    uri: &str,
    lead: Vec<String>,
    compaction: &Path,
) -> ResolvedResource {
    ResolvedResource {
        uri: uri.to_owned(),
        backing: ResourceBacking::Namespace {
            listing: render_listing(lead, directory_rows("history://compaction/", compaction)),
            entries: vec![("history://compaction/".to_owned(), compaction.to_owned())],
        },
    }
}

/// transcript 在列表里的行；文件尚未落盘时为 `None`。
pub(super) fn history_transcript_row(context: &ResourceContext<'_>) -> Option<String> {
    let metadata = fs::metadata(&context.paths.transcript).ok()?;
    Some(format!(
        "history://transcript  {} B  {}s ago",
        metadata.len(),
        age_secs(metadata.modified().unwrap_or(UNIX_EPOCH))
    ))
}
