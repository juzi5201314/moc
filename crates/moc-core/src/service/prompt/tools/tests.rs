use super::*;
use crate::service::prompt::tools::authorization::write_preview;
use crate::service::prompt::tools::authorization::write_targets;

fn workspace(directory: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = directory.path().join("work");
    let outside = directory.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    (
        root.canonicalize().unwrap(),
        outside.canonicalize().unwrap(),
    )
}

fn outside_workspace_targets(
    cwd: &std::path::Path,
    tool_name: &str,
    arguments: &Value,
) -> Result<Vec<std::path::PathBuf>, crate::tools::ToolError> {
    Ok(write_targets(cwd, tool_name, arguments)?
        .into_iter()
        .filter(|path| crate::tools::workspace_escape(cwd, path).is_some())
        .collect())
}

#[test]
fn writes_inside_the_workspace_need_no_approval() {
    let directory = tempfile::tempdir().unwrap();
    let (root, _) = workspace(&directory);
    for arguments in [
        serde_json::json!({"path":"src/main.rs","content":"x"}),
        serde_json::json!({"path":root.join("nested/new.txt").display().to_string(),"content":"x"}),
    ] {
        assert!(
            outside_workspace_targets(&root, "write", &arguments)
                .unwrap()
                .is_empty(),
            "{arguments}"
        );
    }
    let edit = serde_json::json!({
        "path":"src/main.rs",
        "tag":"0000",
        "edits":[{"op":"replace","from":1,"to":1,"text":"change"}]
    });
    assert!(
        outside_workspace_targets(&root, "edit", &edit)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn write_and_move_targets_outside_the_workspace_require_approval() {
    let directory = tempfile::tempdir().unwrap();
    let (root, outside) = workspace(&directory);
    let write = serde_json::json!({
        "path": outside.join("result.txt").display().to_string(),
        "content": "x"
    });
    assert_eq!(
        outside_workspace_targets(&root, "write", &write).unwrap(),
        vec![outside.join("result.txt")]
    );
    // move 的 target_path 与源文件一起授权，越出工作区的目标也必须检查出来。
    let edit = serde_json::json!({
        "path": "src/main.rs",
        "tag": "0000",
        "edits": [{"op":"replace","from":1,"to":1,"text":"change"}],
        "move": {"from":2,"to":2,"target_path": outside.join("moved.txt").display().to_string(),"target_tag":"0000"}
    });
    assert_eq!(
        outside_workspace_targets(&root, "edit", &edit).unwrap(),
        vec![outside.join("moved.txt")]
    );
}

#[test]
fn write_preview_carries_content_and_marks_truncation() {
    let body = write_preview("first\nsecond");
    assert!(body.contains("first\nsecond"));
    let long = (0..WRITE_PREVIEW_LINES + 5)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let preview = write_preview(&long);
    assert!(preview.contains("line 0") && !preview.contains("line 44"));
    assert!(preview.ends_with("[...]"));
    let wide = "x".repeat(WRITE_PREVIEW_BYTES + 1);
    assert!(write_preview(&wide).ends_with("[...]"));
}

#[test]
fn malformed_write_arguments_surface_the_tool_error() {
    let directory = tempfile::tempdir().unwrap();
    let (root, _) = workspace(&directory);
    let error =
        outside_workspace_targets(&root, "edit", &serde_json::json!({"files":[]})).unwrap_err();
    assert_eq!(error.code, "E_BAD_PAYLOAD");
}
