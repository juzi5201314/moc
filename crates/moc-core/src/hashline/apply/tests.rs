use super::*;
use crate::hashline::{AnchorStore, ServedFile};

/// 写文件、整份展示，返回快照、served 状态和 tag 文本。
fn served_snapshot(path: &Path, text: &str) -> (FileSnapshot, ServedFile, String) {
    std::fs::write(path, text).expect("写测试文件");
    let snapshot = read_snapshot(path, 1_000_000).expect("读取快照");
    let mut served = ServedFile::new();
    let tag = served.serve(
        snapshot.epoch,
        &snapshot.lines,
        std::slice::from_ref(&(0..snapshot.lines.len())),
    );
    (snapshot, served, tag_text(tag))
}

fn request(path: &str, tag: &str, edits: Vec<EditOp>) -> EditRequest {
    EditRequest {
        path: path.into(),
        tag: tag.into(),
        edits,
        move_lines: None,
    }
}

fn replace(from: usize, to: usize, text: &str) -> EditOp {
    EditOp::Replace {
        from,
        to,
        text: text.into(),
    }
}

fn insert(after: usize, text: &str) -> EditOp {
    EditOp::Insert {
        after,
        text: text.into(),
    }
}

fn delete(from: usize, to: usize) -> EditOp {
    EditOp::Delete { from, to }
}

#[tokio::test]
async fn applies_single_line_replacement_and_preserves_crlf_bom() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("file.txt");
    std::fs::write(&path, b"\xef\xbb\xbffirst\r\nsecond\r\nthird\r\n").expect("写文件");
    let snapshot = read_snapshot(&path, 4096).expect("读取");
    let mut served = ServedFile::new();
    let tag = tag_text(served.serve(
        snapshot.epoch,
        &snapshot.lines,
        std::slice::from_ref(&(0..3)),
    ));
    apply_edit(
        dir.path(),
        &request("file.txt", &tag, vec![replace(2, 2, "replacement")]),
        &served,
        None,
        &AnchorStore::new(),
        32,
        4096,
    )
    .await
    .expect("编辑成功");
    assert_eq!(
        std::fs::read(&path).expect("读取结果"),
        b"\xef\xbb\xbffirst\r\nreplacement\r\nthird\r\n"
    );
}

#[tokio::test]
async fn empty_insert_after_unterminated_last_line_remains_editable() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("file.txt");
    let (_, served, tag) = served_snapshot(&path, "one");
    let path = path.canonicalize().unwrap();
    let store = AnchorStore::new();
    let output = apply_edit(
        dir.path(),
        &request("file.txt", &tag, vec![insert(1, "")]),
        &served,
        None,
        &store,
        32,
        4096,
    )
    .await
    .expect("插入空行成功");
    assert!(output.content.contains("+ 2│"), "{}", output.content);
    let inserted_bytes = std::fs::read(&path).expect("读取结果");

    let served = store.served(&path).expect("插入后 served");
    let tag = tag_text(served.current_tag().expect("插入后的 tag"));
    apply_edit(
        dir.path(),
        &request("file.txt", &tag, vec![replace(2, 2, "two")]),
        &served,
        None,
        &store,
        32,
        4096,
    )
    .await
    .expect("替换新插入的空行成功");

    assert_eq!(inserted_bytes, b"one\n\n");
    assert_eq!(std::fs::read(&path).expect("读取结果"), b"one\ntwo\n");

    for (original, edit) in [("", insert(0, "")), ("one", replace(1, 1, ""))] {
        let (_, served, tag) = served_snapshot(&path, original);
        let store = AnchorStore::new();
        let output = apply_edit(
            dir.path(),
            &request("file.txt", &tag, vec![edit]),
            &served,
            None,
            &store,
            32,
            4096,
        )
        .await
        .expect("得到单个空行");
        assert!(output.content.contains("+ 1│"), "{}", output.content);
        assert_eq!(std::fs::read(&path).expect("读取结果"), b"\n");

        let header = output.content.lines().next().expect("diff 头部");
        let served = store.served(&path).expect("编辑后 served");
        apply_edit(
            dir.path(),
            &request("file.txt", header, vec![replace(1, 1, "two")]),
            &served,
            None,
            &store,
            32,
            4096,
        )
        .await
        .expect("替换唯一的空行成功");
        assert_eq!(std::fs::read(&path).expect("读取结果"), b"two\n");
    }
}

#[tokio::test]
async fn older_tag_remaps_untouched_lines_and_rejects_changed_ones() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("file.txt");
    let (_snapshot, served, t0) = served_snapshot(&path, "a\nb\nc\n");
    // 锚点按规范化路径登记（macOS /var→/private/var），查找用同一形态。
    let path = path.canonicalize().unwrap();
    let store = AnchorStore::new();
    let output = apply_edit(
        dir.path(),
        &request("file.txt", &t0, vec![insert(0, "top")]),
        &served,
        None,
        &store,
        32,
        4096,
    )
    .await
    .expect("顶部插入成功");
    let header = output.content.lines().next().expect("diff 头部");
    assert!(
        header.ends_with(']') && !header.ends_with(&format!("#{t0}]")),
        "{header}"
    );
    assert!(output.content.contains("+ 1│top"), "{}", output.content);
    assert!(output.content.contains("  2│a"), "{}", output.content);

    let served = store.served(&path).expect("插入后 served");
    apply_edit(
        dir.path(),
        &request("file.txt", &t0, vec![replace(3, 3, "C")]),
        &served,
        None,
        &store,
        32,
        4096,
    )
    .await
    .expect("旧 tag 的未改动行可重映射");
    assert_eq!(
        std::fs::read_to_string(&path).expect("读取结果"),
        "top\na\nb\nC\n"
    );

    let served = store.served(&path).expect("替换后 served");
    let error = apply_edit(
        dir.path(),
        &request("file.txt", &t0, vec![replace(3, 3, "X")]),
        &served,
        None,
        &store,
        32,
        4096,
    )
    .await
    .expect_err("旧 tag 的已改动行必须拒绝");
    assert!(error.to_string().starts_with("[E_STALE_LINE]"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&path).expect("读取结果"),
        "top\na\nb\nC\n"
    );
}

#[tokio::test]
async fn same_gap_inserts_keep_request_order_before_replacement() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("file.txt");
    let (_snapshot, served, tag) = served_snapshot(&path, "a\nb\n");
    apply_edit(
        dir.path(),
        &request(
            "file.txt",
            &tag,
            vec![insert(1, "x"), insert(1, "y"), replace(2, 2, "B")],
        ),
        &served,
        None,
        &AnchorStore::new(),
        32,
        4096,
    )
    .await
    .expect("编辑成功");
    assert_eq!(
        std::fs::read_to_string(&path).expect("读取结果"),
        "a\nx\ny\nB\n"
    );
}

#[tokio::test]
async fn reread_expands_the_served_window() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("file.txt");
    let (snapshot, _served, tag) = served_snapshot(&path, "one\ntwo\nthree\n");
    let mut partial = ServedFile::new();
    partial.serve(
        snapshot.epoch,
        &snapshot.lines,
        std::slice::from_ref(&(0..1)),
    );
    let store = AnchorStore::new();
    let edit = request("file.txt", &tag, vec![replace(1, 3, "rewritten")]);
    let error = apply_edit(dir.path(), &edit, &partial, None, &store, 32, 4096)
        .await
        .expect_err("未展示的行必须拒绝");
    assert!(error.to_string().contains("E_UNSEEN"), "{error}");
    partial.serve(
        snapshot.epoch,
        &snapshot.lines,
        std::slice::from_ref(&(0..3)),
    );
    apply_edit(dir.path(), &edit, &partial, None, &store, 32, 4096)
        .await
        .expect("重读后范围可编辑");
}

#[tokio::test]
async fn rejects_unseen_and_overlapping_edits_without_mutating_files() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("file.txt");
    let (snapshot, served, tag) = served_snapshot(&path, "one\ntwo\nthree\n");
    let before = std::fs::read(&path).expect("读取原文件");
    let mut partial = ServedFile::new();
    partial.serve(snapshot.epoch, &snapshot.lines, &[0..1, 2..3]);
    let store = AnchorStore::new();
    let error = apply_edit(
        dir.path(),
        &request("file.txt", &tag, vec![replace(1, 2, "x")]),
        &partial,
        None,
        &store,
        32,
        4096,
    )
    .await
    .expect_err("未展示的中间行必须拒绝");
    assert!(error.to_string().contains("E_UNSEEN"), "{error}");
    for edits in [
        vec![replace(1, 2, "x"), replace(2, 3, "y")],
        vec![replace(1, 1, "one"), replace(1, 1, "two")],
        vec![replace(1, 3, "x"), insert(1, "y")],
    ] {
        let error = apply_edit(
            dir.path(),
            &request("file.txt", &tag, edits),
            &served,
            None,
            &store,
            32,
            4096,
        )
        .await
        .expect_err("重叠范围必须拒绝");
        assert!(error.to_string().contains("E_OVERLAP"), "{error}");
    }
    let error = apply_edit(
        dir.path(),
        &request("file.txt", &tag, vec![replace(3, 1, "x")]),
        &served,
        None,
        &store,
        32,
        4096,
    )
    .await
    .expect_err("反向范围必须拒绝");
    assert!(error.to_string().contains("E_BAD_PAYLOAD"), "{error}");
    assert_eq!(std::fs::read(&path).expect("读取原文件"), before);
}

#[tokio::test]
async fn preview_matches_applied_content_and_reports_insert_delete_lines() {
    use moc_protocol::DiffLineKind;

    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("file.txt");
    let (_snapshot, served, tag) = served_snapshot(&path, "alpha\nbeta\ngamma\n");
    let store = AnchorStore::new();
    let edit = request("file.txt", &tag, vec![insert(2, "delta\nbeta")]);
    let preview = preview_edit(dir.path(), &edit, &served, None, 32, 4096).expect("预览成功");
    assert!(!preview.truncated);
    assert_eq!(preview.files.len(), 1);
    let kinds: Vec<_> = preview.files[0]
        .hunks
        .iter()
        .flat_map(|hunk| {
            hunk.lines
                .iter()
                .map(|line| (line.kind, line.text.as_str()))
        })
        .collect();
    assert!(kinds.contains(&(DiffLineKind::Insert, "delta")));
    assert!(kinds.contains(&(DiffLineKind::Insert, "beta")));
    assert!(!kinds.iter().any(|(kind, _)| *kind == DiffLineKind::Delete));
    let output = apply_edit(dir.path(), &edit, &served, None, &store, 32, 4096)
        .await
        .expect("编辑成功");
    assert_eq!(
        std::fs::read_to_string(&path).expect("读取结果"),
        "alpha\nbeta\ndelta\nbeta\ngamma\n"
    );
    let applied = output.diff.expect("成功编辑携带 diff");
    let inserts: Vec<&str> = applied.files[0]
        .hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .filter(|line| line.kind == DiffLineKind::Insert)
        .map(|line| line.text.as_str())
        .collect();
    assert_eq!(inserts.len(), 2);
    assert!(inserts.contains(&"delta"));
    assert!(inserts.contains(&"beta"));
}

#[tokio::test]
async fn move_commits_both_files_and_advances_both_served_states() {
    let dir = tempfile::tempdir().expect("临时目录");
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    std::fs::write(&source, "a\nb\nc\nd\ne\nf\n").expect("写源文件");
    std::fs::write(&target, "t1\nt2\nt3\nt4\nt5\nt6\nt7\nt8\n").expect("写目标文件");
    // 输出头与锚点都按规范化路径生成（macOS /var→/private/var）。
    let source = source.canonicalize().unwrap();
    let target = target.canonicalize().unwrap();
    let source_snapshot = read_snapshot(&source, 4096).expect("读取源文件");
    let target_snapshot = read_snapshot(&target, 4096).expect("读取目标文件");
    let mut source_served = ServedFile::new();
    let source_tag =
        tag_text(source_served.serve(source_snapshot.epoch, &source_snapshot.lines, &[0..2, 5..6]));
    let mut target_served = ServedFile::new();
    let target_tag = tag_text(target_served.serve(
        target_snapshot.epoch,
        &target_snapshot.lines,
        std::slice::from_ref(&(0..target_snapshot.lines.len())),
    ));
    let store = AnchorStore::new();
    let mut edit = request("source", &source_tag, vec![insert(1, "x")]);
    edit.move_lines = Some(MoveRequest {
        from: 2,
        to: 2,
        target_path: "target".into(),
        target_tag: target_tag.clone(),
        after: Some(1),
    });
    let output = apply_edit(
        dir.path(),
        &edit,
        &source_served,
        Some(&target_served),
        &store,
        32,
        4096,
    )
    .await
    .expect("move 成功");
    assert_eq!(
        std::fs::read_to_string(&source).expect("读取源文件"),
        "a\nx\nc\nd\ne\nf\n"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("读取目标文件"),
        "t1\nb\nt2\nt3\nt4\nt5\nt6\nt7\nt8\n"
    );
    assert!(output.content.contains(&format!("[{}#", target.display())));
    assert_eq!(
        std::fs::read_dir(dir.path()).expect("读取目录").count(),
        2,
        "临时文件必须清理"
    );

    // 两侧都用编辑前的旧 tag：位移必须已进入各自的版本链。
    let source_served = store.served(&source).expect("提交后的源 served");
    let target_served = store.served(&target).expect("提交后的目标 served");
    apply_edit(
        dir.path(),
        &request("source", &source_tag, vec![delete(6, 6)]),
        &source_served,
        None,
        &store,
        32,
        4096,
    )
    .await
    .expect("move 源删除必须进入位移表");
    assert_eq!(
        std::fs::read_to_string(&source).expect("读取源文件"),
        "a\nx\nc\nd\ne\n"
    );
    apply_edit(
        dir.path(),
        &request("target", &target_tag, vec![delete(7, 7)]),
        &target_served,
        None,
        &store,
        32,
        4096,
    )
    .await
    .expect("move 目标插入必须进入位移表");
    assert_eq!(
        std::fs::read_to_string(&target).expect("读取目标文件"),
        "t1\nb\nt2\nt3\nt4\nt5\nt6\nt8\n"
    );
}

/// 白盒核对：乱序编辑排序后的位移表把旧行映射到正确的新位置。
#[test]
fn out_of_order_edits_build_a_correct_move_table() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("file.txt");
    std::fs::write(&path, "}\n".repeat(12)).expect("写文件");
    let snapshot = read_snapshot(&path, 4096).expect("读取");
    let mut served = ServedFile::new();
    served.serve(snapshot.epoch, &snapshot.lines, &[1..2, 3..4, 10..11]);
    let mut edits = vec![
        VerifiedEdit {
            start: 10,
            end: 11,
            replacement: Vec::new(),
        },
        VerifiedEdit {
            start: 1,
            end: 2,
            replacement: Vec::new(),
        },
    ];
    edits.sort_by_key(|edit| (edit.start, edit.end));
    let moves = build_moves(&edits);
    let advanced = served.advanced(&moves, &vec!["}".to_owned(); 10]);
    assert_eq!(advanced.map_line(0, 3), Some(2), "第 4 行落到 index 2");
    assert!(advanced.is_seen(2));
    assert_eq!(advanced.map_line(0, 1), None, "第 2 行已删除");
    assert_eq!(advanced.map_line(0, 10), None, "第 11 行已删除");
}

#[tokio::test]
async fn undo_rejects_external_changes_without_consuming_history() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("source");
    let (_snapshot, served, tag) = served_snapshot(&path, "first\nlast\n");
    // undo 栈按规范化路径登记（macOS /var→/private/var）。
    let path = path.canonicalize().unwrap();
    let store = AnchorStore::new();
    apply_edit(
        dir.path(),
        &request("source", &tag, vec![insert(1, "added")]),
        &served,
        None,
        &store,
        32,
        4096,
    )
    .await
    .expect("编辑成功");
    std::fs::write(&path, "external content").expect("外部改写");
    for _ in 0..2 {
        let error = undo(&path, &store, 4096)
            .await
            .expect_err("外部改写必须拒绝撤销");
        assert!(error.to_string().contains("E_UNDO_STALE"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("读取文件"),
            "external content"
        );
    }
}

#[tokio::test]
async fn commit_rejects_external_writes_that_land_after_planning() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("source");
    let (_snapshot, served, tag) = served_snapshot(&path, "one\ntwo\n");
    let store = AnchorStore::new();
    let edit = request("source", &tag, vec![insert(1, "inserted")]);
    let plan = plan_edit(dir.path(), &edit, &served, None, 32, 4096).expect("计划成功");
    std::fs::write(&path, "external\n").expect("外部改写");
    let error = commit_plan(plan, &store, 4096)
        .await
        .expect_err("计划后外部写入必须拒绝提交");
    assert!(error.to_string().contains("E_STALE_TAG"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&path).expect("读取文件"),
        "external\n"
    );
    assert!(store.latest_undo_path().is_none());
    assert_eq!(
        std::fs::read_dir(dir.path()).expect("读取目录").count(),
        1,
        "临时文件与备份必须清理"
    );
}

#[tokio::test]
async fn oversized_edits_preserve_files_and_undo() {
    for moving in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        // BOM 和 CRLF 也计入最终字节数，不能只检查请求中的文本长度。
        let original = "\u{feff}move\r\nkeep\r\n";
        let (_, served, tag) = served_snapshot(&source, original);
        let (_, target_served, target_tag) = served_snapshot(&target, "target12345\n");
        let store = AnchorStore::new();
        let mut edit = request("source", &tag, vec![replace(1, 1, "x\nx\nx\nx")]);
        if moving {
            edit.edits.clear();
            edit.move_lines = Some(MoveRequest {
                from: 1,
                to: 1,
                target_path: "target".into(),
                target_tag,
                after: None,
            });
        }
        let error = apply_edit(
            dir.path(),
            &edit,
            &served,
            Some(&target_served),
            &store,
            32,
            15,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("E_TOO_LARGE"), "{error}");
        assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "target12345\n");
        assert!(store.latest_undo_path().is_none());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}

#[tokio::test]
async fn target_epoch_change_rolls_back_the_committed_source() {
    let dir = tempfile::tempdir().expect("临时目录");
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    let (_source_snapshot, source_served, source_tag) = served_snapshot(&source, "move\nkeep\n");
    let (_target_snapshot, target_served, target_tag) = served_snapshot(&target, "target\n");
    let store = AnchorStore::new();
    let mut edit = request("source", &source_tag, vec![insert(2, "after")]);
    edit.move_lines = Some(MoveRequest {
        from: 1,
        to: 1,
        target_path: "target".into(),
        target_tag,
        after: None,
    });
    let plan = plan_edit(
        dir.path(),
        &edit,
        &source_served,
        Some(&target_served),
        32,
        4096,
    )
    .expect("计划成功");
    std::fs::write(&target, "external\n").expect("外部改写目标");
    let error = commit_plan(plan, &store, 4096)
        .await
        .expect_err("目标 epoch 变化必须回滚源文件");
    assert!(error.to_string().contains("E_STALE_TAG"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&source).expect("读取源文件"),
        "move\nkeep\n"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("读取目标文件"),
        "external\n"
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).expect("读取目录").count(),
        2,
        "临时文件与备份必须清理"
    );
    assert!(store.latest_undo_path().is_none());
}

/// 大文件里的末尾编辑必须生成完整 diff，而不是空 hunk + truncated。
#[tokio::test]
async fn large_file_edit_diff_is_complete() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("large.txt");
    let text = (0..2500)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let (_snapshot, served, tag) = served_snapshot(&path, &text);
    let edit = request("large.txt", &tag, vec![insert(2401, "inserted")]);
    let preview = preview_edit(dir.path(), &edit, &served, None, 32, 1_000_000).expect("预览成功");
    assert!(!preview.truncated, "大文件里的单行改动不应截断");
    fn diff_lines(diff: &ToolDiff) -> Vec<(moc_protocol::DiffLineKind, &str)> {
        diff.files[0]
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .map(|line| (line.kind, line.text.as_str()))
            .collect()
    }
    assert!(
        diff_lines(&preview).contains(&(moc_protocol::DiffLineKind::Insert, "inserted")),
        "预览 diff 必须包含实际改动"
    );

    let output = apply_edit(
        dir.path(),
        &edit,
        &served,
        None,
        &AnchorStore::new(),
        32,
        1_000_000,
    )
    .await
    .expect("编辑成功");
    let diff = output.diff.expect("成功编辑必须携带 diff");
    assert!(!diff.truncated);
    let lines = diff_lines(&diff);
    assert!(lines.contains(&(moc_protocol::DiffLineKind::Insert, "inserted")));
    assert!(
        !lines
            .iter()
            .any(|(kind, _)| *kind == moc_protocol::DiffLineKind::Delete),
        "纯插入不应凭空生成删除行"
    );
    assert!(
        std::fs::read_to_string(&path)
            .expect("读取结果")
            .contains("line 2400\ninserted\nline 2401")
    );
}

/// 同一次 edit 调用里相距很远的改动都要进 diff。
#[tokio::test]
async fn distant_edits_all_appear_in_diff() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("sparse.txt");
    let text = (0..5000)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let (_snapshot, served, tag) = served_snapshot(&path, &text);
    let output = apply_edit(
        dir.path(),
        &request(
            "sparse.txt",
            &tag,
            vec![insert(11, "first"), insert(4001, "second")],
        ),
        &served,
        None,
        &AnchorStore::new(),
        32,
        1_000_000,
    )
    .await
    .expect("编辑成功");
    let diff = output.diff.expect("成功编辑携带 diff");
    assert!(!diff.truncated);
    assert_eq!(diff.files[0].hunks.len(), 2, "相距很远的改动应各自成 hunk");
    let texts: Vec<&str> = diff.files[0]
        .hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .map(|line| line.text.as_str())
        .collect();
    assert!(texts.contains(&"first"));
    assert!(texts.contains(&"second"));
}

/// move 到目标文件的插入也要按已知范围生成完整 diff（目标可以很大）。
#[tokio::test]
async fn move_target_diff_is_complete() {
    let dir = tempfile::tempdir().expect("临时目录");
    let source = dir.path().join("source.txt");
    let target = dir.path().join("target.txt");
    let target_text = (0..2500)
        .map(|index| format!("target {index}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let (_source_snapshot, source_served, source_tag) = served_snapshot(&source, "move\nkeep\n");
    let (_target_snapshot, target_served, target_tag) = served_snapshot(&target, &target_text);
    let mut edit = request("source.txt", &source_tag, vec![insert(2, "after")]);
    edit.move_lines = Some(MoveRequest {
        from: 1,
        to: 1,
        target_path: "target.txt".into(),
        target_tag,
        after: None,
    });
    let output = apply_edit(
        dir.path(),
        &edit,
        &source_served,
        Some(&target_served),
        &AnchorStore::new(),
        32,
        1_000_000,
    )
    .await
    .expect("move 成功");
    let diff = output.diff.expect("move 必须携带 diff");
    assert!(!diff.truncated);
    assert_eq!(diff.files.len(), 2);
    let moved_insert = diff.files[1]
        .hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .find(|line| line.kind == moc_protocol::DiffLineKind::Insert && line.text == "move")
        .expect("目标 diff 必须包含搬运行");
    assert_eq!(moved_insert.new_no, Some(2501), "追加到第 2500 行之后");
}

/// 同一次调用里相距很远的编辑必须分段渲染：中间未改动的行不进 diff 文本，
/// 也不登记成"已展示"。
#[tokio::test]
async fn distant_edits_render_separate_hunks_and_hide_the_gap() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("sparse.txt");
    let text = (0..1000)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&path, &text).expect("写测试文件");
    // 锚点按规范化路径登记（macOS /var→/private/var）。
    let path = path.canonicalize().unwrap();
    let snapshot = read_snapshot(&path, 1_000_000).expect("读取快照");
    let mut served = ServedFile::new();
    // 只展示两处改动附近的行：中间的大段从未展示给模型。
    let tag = tag_text(served.serve(snapshot.epoch, &snapshot.lines, &[17..24, 814..821]));
    let store = AnchorStore::new();
    let output = apply_edit(
        dir.path(),
        &request(
            "sparse.txt",
            &tag,
            vec![replace(21, 21, "first"), replace(818, 818, "second")],
        ),
        &served,
        None,
        &store,
        32,
        1_000_000,
    )
    .await
    .expect("编辑成功");
    let content = &output.content;
    assert!(content.contains("- line 20"), "{content}");
    assert!(content.contains("+ 21│first"), "{content}");
    assert!(content.contains("- line 817"), "{content}");
    assert!(content.contains("+ 818│second"), "{content}");
    assert!(content.contains("[… lines 25-814 unchanged]"), "{content}");
    assert!(
        !content.contains("500│"),
        "省略区不能进 diff 文本: {content}"
    );

    let header = content.lines().next().expect("diff 头部");
    let new_tag = header
        .rsplit('#')
        .next()
        .and_then(|tail| tail.strip_suffix(']'))
        .expect("头部带新 tag");
    let served = store.served(&path).expect("提交后的 served");
    let error = apply_edit(
        dir.path(),
        &request("sparse.txt", new_tag, vec![replace(500, 500, "middle")]),
        &served,
        None,
        &store,
        32,
        1_000_000,
    )
    .await
    .expect_err("省略区里的行必须按未展示拒绝");
    assert!(error.to_string().contains("E_UNSEEN"), "{error}");
}
