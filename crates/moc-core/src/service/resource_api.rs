//! 资源与图表协议操作。

use super::Core;
use crate::service::invalid_params;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::RpcErrorCode;
use moc_protocol::methods::ChartAnalyzeParams;
use moc_protocol::methods::ChartAnalyzeResult;
use moc_protocol::methods::FsEntryKind;
use moc_protocol::methods::FsFileEncoding;
use moc_protocol::methods::ResourceCompleteParams;
use moc_protocol::methods::ResourceCompleteResult;
use moc_protocol::methods::ResourceListParams;
use moc_protocol::methods::ResourceListResult;
use moc_protocol::methods::ResourceReadParams;
use moc_protocol::methods::ResourceReadResult;
use serde_json::json;

pub(super) async fn resource_complete(
    core: &Core,
    ctx: RequestContext,
    params: ResourceCompleteParams,
) -> Result<ResourceCompleteResult, RpcError> {
    let (record, cwd, skills) = core.resource_context(&params.session_id).await?;
    let tools = core.resource_tools(&ctx, &record, &params.prefix).await?;
    let sessions = core.inner.sessions.clone();
    let prefix = params.prefix.clone();
    let session_id = params.session_id.clone();
    // `@` 补全要遍历工作区，放到阻塞线程避免卡住执行器。
    let candidates = tokio::task::spawn_blocking(move || {
        let context = crate::resources::ResourceContext {
            session_id: &session_id,
            cwd: &cwd,
            paths: &record.paths,
            skills: &skills,
            sessions: &sessions,
            tools,
        };
        crate::resources::ResourceRouter::complete(&context, &prefix)
    })
    .await
    .map_err(|error| RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()))?
    .map_err(|error| RpcError::new(RpcErrorCode::InvalidParams.code(), error.to_string()))?
    .into_iter()
    .map(|candidate| moc_protocol::ResourceCandidate {
        uri: candidate.uri,
        label: candidate.label,
        description: candidate.description,
        kind: candidate.kind,
        append_space: candidate.append_space,
    })
    .collect();
    Ok(ResourceCompleteResult { candidates })
}

pub(super) async fn resource_list(
    core: &Core,
    ctx: RequestContext,
    params: ResourceListParams,
) -> Result<ResourceListResult, RpcError> {
    let (record, cwd, skills) = core.resource_context(&params.session_id).await?;
    let tools = core.resource_tools(&ctx, &record, &params.uri).await?;
    let sessions = core.inner.sessions.clone();
    let listing = tokio::task::spawn_blocking(move || {
        let context = crate::resources::ResourceContext {
            session_id: &params.session_id,
            cwd: &cwd,
            paths: &record.paths,
            skills: &skills,
            sessions: &sessions,
            tools,
        };
        crate::resources::list(&context, &params.uri)
    })
    .await
    .map_err(|error| RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()))?
    .map_err(|error| invalid_params(error.to_string()))?;
    Ok(ResourceListResult {
        uri: listing.uri,
        truncated: listing.truncated,
        entries: listing
            .entries
            .into_iter()
            .map(|entry| moc_protocol::ResourceListEntry {
                uri: entry.uri,
                name: entry.name,
                kind: if entry.is_dir {
                    FsEntryKind::Dir
                } else {
                    FsEntryKind::File
                },
                size: entry.size,
                modified_ms: entry.modified_ms,
                description: entry.description,
            })
            .collect(),
    })
}

pub(super) async fn resource_read(
    core: &Core,
    ctx: RequestContext,
    params: ResourceReadParams,
) -> Result<ResourceReadResult, RpcError> {
    let (record, cwd, skills) = core.resource_context(&params.session_id).await?;
    let tools = core.resource_tools(&ctx, &record, &params.uri).await?;
    let sessions = core.inner.sessions.clone();
    let uri = params.uri.clone();
    tokio::task::spawn_blocking(move || {
        let context = crate::resources::ResourceContext {
            session_id: &params.session_id,
            cwd: &cwd,
            paths: &record.paths,
            skills: &skills,
            sessions: &sessions,
            tools,
        };
        let resolved = crate::resources::ResourceRouter::resolve(&context, &params.uri)
            .map_err(|error| invalid_params(error.to_string()))?;
        let path = match resolved.backing {
            crate::resources::ResourceBacking::File(path) => path,
            crate::resources::ResourceBacking::Text(mut text) => {
                let size = text.len() as u64;
                let limit = params
                    .max_bytes
                    .unwrap_or(moc_protocol::FS_READ_FILE_DEFAULT_MAX_BYTES)
                    .min(moc_protocol::FS_READ_FILE_MAX_BYTES) as usize;
                let mut end = text.len().min(limit);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                return Ok(ResourceReadResult {
                    uri,
                    size,
                    modified_ms: None,
                    encoding: FsFileEncoding::Utf8,
                    content: Some(text),
                    truncated: size > limit as u64,
                });
            }
            crate::resources::ResourceBacking::Namespace { .. } => {
                return Err(invalid_params(
                    moc_i18n::t!("core.err.uri_is_namespace").into_owned(),
                ));
            }
        };
        let read = crate::fs::read_path(&path, params.max_bytes)
            .map_err(|error| invalid_params(error.to_string()))?;
        Ok(ResourceReadResult {
            uri,
            size: read.size,
            modified_ms: read.modified_ms,
            encoding: if read.binary {
                FsFileEncoding::Binary
            } else {
                FsFileEncoding::Utf8
            },
            content: read.content,
            truncated: read.truncated,
        })
    })
    .await
    .map_err(|error| RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()))?
}

pub(super) async fn chart_analyze(
    _ctx: RequestContext,
    params: ChartAnalyzeParams,
) -> Result<ChartAnalyzeResult, RpcError> {
    let table = crate::chart::analyze(&crate::chart::RawTable {
        header: params.header,
        rows: params.rows,
    });
    let Some(plan) = crate::chart::plan(&table) else {
        return Ok(ChartAnalyzeResult { spec: None });
    };
    let spec = crate::chart::build(&table, &plan);
    let kind = match spec.kind {
        crate::chart::ChartKind::Bar => "Bar",
        crate::chart::ChartKind::Grouped => "Grouped",
        crate::chart::ChartKind::Paired => "Paired",
        crate::chart::ChartKind::Line => "Line",
        crate::chart::ChartKind::Stacked => "Stacked",
        crate::chart::ChartKind::Progress => "Progress",
        crate::chart::ChartKind::Heatmap => "Heatmap",
        crate::chart::ChartKind::Scatter => "Scatter",
        crate::chart::ChartKind::Panels => "Panels",
    };
    let series = spec
        .series
        .into_iter()
        .map(|item| {
            let dim = match item.dim {
                crate::chart::Dimension::Count => "Count",
                crate::chart::Dimension::Percent => "Percent",
                crate::chart::Dimension::Duration => "Duration",
                crate::chart::Dimension::Bytes => "Bytes",
                crate::chart::Dimension::Currency => "Currency",
                crate::chart::Dimension::Ratio => "Ratio",
                crate::chart::Dimension::Fraction => "Fraction",
                crate::chart::Dimension::Rate => "Rate",
            };
            let polarity = item.polarity.map(|value| match value {
                crate::chart::Polarity::Higher => "Higher",
                crate::chart::Polarity::Lower => "Lower",
            });
            let points: Vec<_> = item
                .points
                .into_iter()
                .map(|point| {
                    point.map(|point| {
                        json!({
                            "value": point.value,
                            "text": point.text,
                            "upper": point.upper,
                            "total": point.total,
                        })
                    })
                })
                .collect();
            json!({
                "name": item.name,
                "dim": dim,
                "unit": item.unit,
                "factor": item.factor,
                "polarity": polarity,
                "points": points,
            })
        })
        .collect::<Vec<_>>();
    Ok(ChartAnalyzeResult {
        spec: Some(json!({
            "kind": kind,
            "axis": spec.axis,
            "categories": spec.categories,
            "series": series,
            "normalized": spec.normalized,
            "diverging": spec.diverging,
            "notes": spec.notes,
        })),
    })
}
