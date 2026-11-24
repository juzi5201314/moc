use super::*;

fn test_paths(root: &Path) -> SessionPaths {
    SessionPaths::new(root.join("transcript.jsonl"), root.join("sidecar"))
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
        tools: None,
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

    let root_candidates = ResourceRouter::complete(&context, "//", None).expect("根候选");
    assert!(
        !root_candidates
            .iter()
            .any(|candidate| candidate.uri == "proc://")
    );
    assert!(root_candidates.len() <= 20);
    let local = ResourceRouter::complete(&context, "local:/", None).expect("local 前缀");
    assert!(local.iter().any(|candidate| candidate.uri == "local://"));
    let artifacts = ResourceRouter::complete(&context, "artifact://", None).expect("artifact 前缀");
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
    let history = ResourceRouter::complete(&context, "history://", None).expect("history 前缀");
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

    let history = ResourceRouter::resolve_context(&context, ResourceScheme::History, "history://")
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

    let transcript =
        ResourceRouter::resolve_context(&context, ResourceScheme::History, "history://raw")
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

    let transcript =
        ResourceRouter::resolve_context(&context, ResourceScheme::History, "history://raw")
            .expect("transcript URI")
            .expect("transcript 是资源");
    let ResourceBacking::File(mirror) = transcript.backing else {
        panic!("transcript 资源必须可读取");
    };
    assert_eq!(
        fs::read_to_string(&mirror).expect("读取镜像"),
        "{\"a\":1}\n{\"a\":2}\n",
        "压缩会话对外暴露完整解码后的日志"
    );
}

/// `resource/list`：根列出资源命名空间，各 scheme 展开成一层条目。
#[test]
fn list_exposes_namespaces_as_structured_entries() {
    let root = tempfile::tempdir().expect("临时目录");
    let paths = test_paths(root.path());
    fs::create_dir_all(paths.plans()).expect("plans 目录");
    fs::create_dir_all(paths.jobs()).expect("jobs 目录");
    fs::write(paths.plans().join("plan_1.md"), "# plan\n").expect("计划");
    fs::write(paths.plans().join("note.txt"), "x\n").expect("非 md 不列");
    fs::write(paths.jobs().join("job_1.log"), "alpha\n").expect("job 日志");
    let sessions = SessionTable::new(root.path().join("sessions"), false);
    let skills = SkillIndex::empty();
    let context = context_of(&paths, root.path(), &skills, &sessions);

    let root_listing = list(&context, "").expect("根列举");
    let uris: Vec<_> = root_listing
        .entries
        .iter()
        .map(|entry| (entry.uri.as_str(), entry.is_dir))
        .collect();
    assert_eq!(
        uris,
        [
            ("local://", true),
            ("artifact://jobs", true),
            ("artifact://artifacts", true),
            ("history://", true),
            ("skill://", true),
            ("tool://", true),
        ]
    );
    assert!(!root_listing.truncated);

    let local = list(&context, "local://").expect("local 列举");
    assert_eq!(local.entries.len(), 1);
    assert_eq!(local.entries[0].uri, "local://plan_1.md");
    assert!(!local.entries[0].is_dir);
    assert_eq!(local.entries[0].size, Some(7));

    let jobs = list(&context, "artifact://jobs").expect("jobs 列举");
    assert_eq!(jobs.entries.len(), 1);
    assert_eq!(jobs.entries[0].uri, "artifact://jobs/job_1.log");
    assert_eq!(jobs.entries[0].size, Some(6));

    assert_eq!(
        list(&context, "local://plan_1.md").unwrap_err().code,
        "E_LOCAL_URI",
        "文件 URI 不是命名空间"
    );
}

#[test]
fn list_skill_expands_base_dir_one_level() {
    let root = tempfile::tempdir().expect("临时目录");
    let skill_dir = root.path().join("demo");
    fs::create_dir_all(skill_dir.join("refs")).expect("refs 目录");
    fs::write(skill_dir.join("SKILL.md"), "x").expect("SKILL.md");
    fs::write(skill_dir.join("refs").join("a.md"), "a").expect("资源");
    fs::write(skill_dir.join(".hidden"), "h").expect("隐藏文件不列");
    let paths = test_paths(root.path());
    let sessions = SessionTable::new(root.path().join("sessions"), false);
    let mut skills = SkillIndex::empty();
    skills.insert_builtin(crate::skills::SkillMeta {
        name: "demo".into(),
        description: "演示技能".into(),
        when_to_use: None,
        globs: Vec::new(),
        model_invocable: true,
        base_dir: skill_dir.clone(),
        source: "test".into(),
    });
    let context = context_of(&paths, root.path(), &skills, &sessions);

    let skill_root = list(&context, "skill://").expect("skill 根");
    assert_eq!(skill_root.entries.len(), 1);
    assert_eq!(skill_root.entries[0].uri, "skill://demo");
    assert!(skill_root.entries[0].is_dir);
    assert_eq!(
        skill_root.entries[0].description.as_deref(),
        Some("演示技能")
    );

    let listing = list(&context, "skill://demo").expect("技能一层");
    let uris: Vec<_> = listing
        .entries
        .iter()
        .map(|entry| (entry.uri.as_str(), entry.is_dir))
        .collect();
    assert_eq!(
        uris,
        [
            ("skill://demo/refs", true),
            ("skill://demo/SKILL.md", false),
        ]
    );

    let sub = list(&context, "skill://demo/refs").expect("子目录");
    assert_eq!(sub.entries.len(), 1);
    assert_eq!(sub.entries[0].uri, "skill://demo/refs/a.md");
    assert!(!sub.entries[0].is_dir);

    assert_eq!(
        list(&context, "skill://demo/.hidden").unwrap_err().code,
        "E_SKILL_PATH"
    );
    assert_eq!(
        list(&context, "skill://missing").unwrap_err().code,
        "E_SKILL_NOT_FOUND"
    );
}

/// `resource/read`：经 ResourceRouter 把 URI 解析成文件后读内容；
/// 命名空间不可读。
#[test]
fn read_plan_file_through_router() {
    let root = tempfile::tempdir().expect("临时目录");
    let paths = test_paths(root.path());
    fs::create_dir_all(paths.plans()).expect("plans 目录");
    fs::write(paths.plans().join("plan_1.md"), "# plan\n").expect("计划");
    let sessions = SessionTable::new(root.path().join("sessions"), false);
    let skills = SkillIndex::empty();
    let context = context_of(&paths, root.path(), &skills, &sessions);

    let resolved = ResourceRouter::resolve(&context, "local://plan_1.md").expect("解析计划");
    let ResourceBacking::File(path) = &resolved.backing else {
        panic!("local://plan_1.md 必须是文件：{:?}", resolved.backing);
    };
    let read = crate::fs::read_path(path, None).expect("读计划");
    assert!(!read.binary);
    assert_eq!(read.content.as_deref(), Some("# plan\n"));

    let namespace = ResourceRouter::resolve(&context, "local://").expect("解析命名空间");
    assert!(matches!(
        namespace.backing,
        ResourceBacking::Namespace { .. }
    ));
    assert_eq!(
        ResourceRouter::resolve(&context, "proc://1")
            .unwrap_err()
            .code,
        "E_URI_INVALID"
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

    let root_listing = ResourceRouter::resolve_context(&context, ResourceScheme::Skill, "skill://")
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

    let skill = ResourceRouter::resolve_context(&context, ResourceScheme::Skill, "skill://demo")
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

/// `@` 前缀走工作区文件补全：子序列模糊匹配、尊重 .gitignore、目录候选以 `/` 收尾。
#[test]
fn at_prefix_completes_workspace_files_and_skips_ignored() {
    let root = tempfile::tempdir().expect("临时目录");
    let paths = test_paths(root.path());
    let sessions = SessionTable::new(root.path().join("sessions"), false);
    let skills = SkillIndex::empty();
    fs::create_dir_all(root.path().join("src")).expect("src 目录");
    // .gitignore 只在 git 仓库内生效（ignore::WalkBuilder 的 require_git 默认）。
    fs::create_dir_all(root.path().join(".git")).expect(".git 目录");
    fs::write(root.path().join("src").join("main.rs"), "fn main() {}\n").expect("main.rs");
    fs::write(root.path().join("notes.txt"), "hi\n").expect("notes.txt");
    fs::create_dir_all(root.path().join("crates")).expect("crates 目录");
    fs::write(root.path().join(".gitignore"), "secret.txt\n").expect(".gitignore");
    fs::write(root.path().join("secret.txt"), "s\n").expect("secret.txt");
    let context = context_of(&paths, root.path(), &skills, &sessions);

    let all = ResourceRouter::complete(&context, "@", None).expect("@ 候选");
    let uris = all.iter().map(|c| c.uri.as_str()).collect::<Vec<_>>();
    assert!(uris.contains(&"@src/"), "{uris:?}");
    assert!(uris.contains(&"@src/main.rs"), "{uris:?}");
    assert!(uris.contains(&"@notes.txt"), "{uris:?}");
    assert!(!uris.iter().any(|uri| uri.contains("secret")), "{uris:?}");
    let dir = all.iter().find(|c| c.uri == "@src/").expect("目录候选");
    assert_eq!(dir.kind, "dir");
    assert!(!dir.append_space, "目录候选不加空格以便续打");
    let file = all
        .iter()
        .find(|c| c.uri == "@notes.txt")
        .expect("文件候选");
    assert_eq!(file.kind, "file");
    assert!(file.append_space);

    let fuzzy = ResourceRouter::complete(&context, "@smr", None).expect("模糊候选");
    assert_eq!(fuzzy[0].uri, "@src/main.rs", "{fuzzy:?}");
    // 文件名连续命中优先于目录名零散命中；高亮下标落在 basename 上。
    let suffix = ResourceRouter::complete(&context, "@rs", None).expect("后缀候选");
    assert_eq!(suffix[0].uri, "@src/main.rs", "{suffix:?}");
    assert_eq!(suffix[0].matches, [9, 10]);
    let dirs = ResourceRouter::complete(&context, "@", Some("dir")).expect("目录候选");
    assert!(
        dirs.iter().all(|c| c.kind == "dir") && !dirs.is_empty(),
        "{dirs:?}"
    );

    // `@@` 是字面转义，不补全。
    let escaped = ResourceRouter::complete(&context, "@@", None).expect("转义候选");
    assert!(escaped.is_empty(), "{escaped:?}");
}
