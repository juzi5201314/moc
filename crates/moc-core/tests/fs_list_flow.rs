mod support;

use moc_protocol::{FsListDir, FsListDirParams};

#[tokio::test]
async fn fs_list_dir_lists_sorted_subdirectories_with_git_marks() {
    let harness = support::Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    let work = harness.root.path().join("work");
    std::fs::create_dir_all(work.join("b")).unwrap();
    std::fs::create_dir_all(work.join("A/.git")).unwrap();
    std::fs::create_dir_all(work.join(".hidden")).unwrap();
    std::fs::write(work.join("file.txt"), "x").unwrap();

    let result = harness
        .client
        .call::<FsListDir>(FsListDirParams {
            path: Some(work.to_string_lossy().into_owned()),
            show_hidden: false,
        })
        .await
        .expect("fs/list_dir 成功");

    let names: Vec<_> = result
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["A", "b"]);
    assert!(result.entries[0].git);
    assert!(!result.entries[1].git);
    let root = std::fs::canonicalize(harness.root.path()).unwrap();
    assert_eq!(
        result.parent.as_deref(),
        Some(root.to_string_lossy().as_ref())
    );
}

#[tokio::test]
async fn fs_list_dir_rejects_relative_paths_as_invalid_params() {
    let harness = support::Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    let error = harness
        .client
        .call::<FsListDir>(FsListDirParams {
            path: Some("relative/dir".into()),
            show_hidden: false,
        })
        .await
        .expect_err("相对路径应被拒绝");
    assert_eq!(error.code(), Some(-32602), "{error:?}");
}
