mod support;

use moc_protocol::{FsCreateDir, FsCreateDirParams, FsListDir, FsListDirParams};

#[tokio::test]
async fn fs_create_dir_makes_one_level_directory_and_relist_shows_it() {
    let harness = support::Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    let work = std::fs::canonicalize(harness.root.path().join("work")).unwrap();

    let created = harness
        .client
        .call::<FsCreateDir>(FsCreateDirParams {
            parent: work.to_string_lossy().into_owned(),
            name: " 新项目 ".into(),
        })
        .await
        .expect("fs/create_dir 成功");
    assert_eq!(created.path, work.join("新项目").to_string_lossy());
    assert!(work.join("新项目").is_dir());

    let listed = harness
        .client
        .call::<FsListDir>(FsListDirParams {
            path: Some(work.to_string_lossy().into_owned()),
            show_hidden: false,
        })
        .await
        .expect("fs/list_dir 成功");
    assert!(
        listed.entries.iter().any(|entry| entry.name == "新项目"),
        "新建的目录应出现在列表里"
    );
}

#[tokio::test]
async fn fs_create_dir_rejects_bad_names_as_invalid_params() {
    let harness = support::Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    let work = std::fs::canonicalize(harness.root.path().join("work")).unwrap();
    for name in ["", "..", "a/b"] {
        let error = harness
            .client
            .call::<FsCreateDir>(FsCreateDirParams {
                parent: work.to_string_lossy().into_owned(),
                name: name.into(),
            })
            .await
            .expect_err("非法目录名应被拒绝");
        assert_eq!(error.code(), Some(-32602), "{name:?}: {error:?}");
    }
    assert!(!work.join("a").exists(), "不得创建出非法路径");
}

#[tokio::test]
async fn fs_create_dir_rejects_existing_directory() {
    let harness = support::Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    let work = std::fs::canonicalize(harness.root.path().join("work")).unwrap();
    std::fs::create_dir(work.join("dup")).unwrap();
    let error = harness
        .client
        .call::<FsCreateDir>(FsCreateDirParams {
            parent: work.to_string_lossy().into_owned(),
            name: "dup".into(),
        })
        .await
        .expect_err("已存在的目录应报错");
    assert_eq!(error.code(), Some(-32602), "{error:?}");
    assert!(error.to_string().contains("目录已存在"), "{error}");
}
