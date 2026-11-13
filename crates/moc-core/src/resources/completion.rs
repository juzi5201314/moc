use super::ArtifactStore;
use super::ResourceCandidate;
use super::ResourceContext;
use super::ResourceError;
use super::listing::list_skill_resources;
use std::fs;

pub(super) fn scheme_candidates(_context: &ResourceContext<'_>) -> Vec<ResourceCandidate> {
    vec![
        namespace("artifact://", "current session tool output", "artifact"),
        namespace("history://", "current session transcript", "history"),
        namespace("local://", "current session plan files", "local"),
        namespace("skill://", "discovered agent skill", "skill"),
        namespace("tool://", "on-demand tool documentation", "tool"),
    ]
}

pub(super) fn normalized_scheme_candidates(
    context: &ResourceContext<'_>,
    scheme: &str,
) -> Vec<ResourceCandidate> {
    if !matches!(scheme, "artifact" | "history" | "local" | "skill" | "tool") {
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

pub(super) fn namespace(uri: &str, description: &str, kind: &str) -> ResourceCandidate {
    ResourceCandidate {
        uri: uri.into(),
        label: uri.into(),
        description: description.into(),
        kind: kind.into(),
        append_space: false,
    }
}

pub(super) fn tool_documentation<'ctx>(
    context: &'ctx ResourceContext<'_>,
) -> Result<&'ctx crate::tools::documentation::ToolDocumentation, ResourceError> {
    context.tools.as_ref().ok_or_else(|| {
        ResourceError::new(
            "E_TOOL_DOC",
            "Tool documentation requires a session tool context",
        )
    })
}

pub(super) fn tool_candidates(
    context: &ResourceContext<'_>,
    prefix: &str,
) -> Vec<ResourceCandidate> {
    let Some(tools) = context.tools.as_ref() else {
        return Vec::new();
    };
    tools
        .topics()
        .map(|(name, description)| {
            let uri = format!("tool://{name}");
            ResourceCandidate {
                label: name.to_owned(),
                uri,
                description: description.to_owned(),
                kind: "tool".into(),
                append_space: true,
            }
        })
        .filter(|candidate| candidate.uri.starts_with(prefix))
        .collect()
}

pub(super) fn context_artifact_candidates(
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

pub(super) fn skill_candidates(
    context: &ResourceContext<'_>,
    prefix: &str,
) -> Vec<ResourceCandidate> {
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

pub(super) fn local_candidates(
    context: &ResourceContext<'_>,
    prefix: &str,
) -> Vec<ResourceCandidate> {
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

pub(super) fn history_candidates(
    context: &ResourceContext<'_>,
    prefix: &str,
) -> Vec<ResourceCandidate> {
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
