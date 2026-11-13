use super::*;

/// 建一个临时工作区：src/（a.rs、b.rs、main.rs）与 tests/。
fn workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("临时目录");
    let root = dir.path().to_owned();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    for file in ["a.rs", "b.rs", "main.rs"] {
        std::fs::write(root.join("src").join(file), "fn x() {}\n").unwrap();
    }
    (dir, root)
}

/// 渲染后的建议列表；`None` 表示放行。
fn blocked(root: &Path, dir: &Path, stdin: bool, command: &str) -> Option<Vec<String>> {
    check(command, root, dir, stdin).map(|verdict| {
        verdict
            .suggestions
            .iter()
            .map(|s| format!("{} {}", s.tool, Value::Object(s.args.clone())))
            .collect()
    })
}

fn hit(command: &str) -> Option<Vec<String>> {
    let (_dir, root) = workspace();
    blocked(&root, &root, false, command)
}

/// 断言命令被拦截且建议列表与期望一致；args 按 JSON 值比较，键序随
/// `serde_json/preserve_order` 特性统一而变，不能按字符串序断言。
fn check_case(command: &str, expected: &[&str]) {
    let parse = |s: &str| {
        let (tool, args) = s.split_once(' ').unwrap();
        (
            tool.to_string(),
            serde_json::from_str::<Value>(args).unwrap(),
        )
    };
    let expected: Vec<_> = expected.iter().map(|s| parse(s)).collect();
    let actual = hit(command).map(|items| items.iter().map(|s| parse(s)).collect::<Vec<_>>());
    assert_eq!(actual, Some(expected), "{command}");
}

fn miss(root: &Path, command: &str) {
    assert_eq!(
        blocked(root, root, false, command),
        None,
        "应放行: {command}"
    );
}

#[test]
fn read_family_translations() {
    let cases: &[(&str, &[&str])] = &[
        ("cat src/main.rs", &["read {\"path\":\"src/main.rs\"}"]),
        (
            "cat -n src/a.rs src/b.rs",
            &[
                "read {\"path\":\"src/a.rs\"}",
                "read {\"path\":\"src/b.rs\"}",
            ],
        ),
        (
            "head -n 40 src/a.rs",
            &["read {\"limit\":40,\"path\":\"src/a.rs\"}"],
        ),
        (
            "head -20 src/a.rs",
            &["read {\"limit\":20,\"path\":\"src/a.rs\"}"],
        ),
        (
            "tail -n 30 src/a.rs",
            &["read {\"offset\":-30,\"path\":\"src/a.rs\"}"],
        ),
        (
            "tail -n +100 src/a.rs",
            &["read {\"offset\":100,\"path\":\"src/a.rs\"}"],
        ),
        (
            "sed -n '10,40p' src/a.rs",
            &["read {\"limit\":31,\"offset\":10,\"path\":\"src/a.rs\"}"],
        ),
        (
            "sed -n 5p src/a.rs",
            &["read {\"limit\":1,\"offset\":5,\"path\":\"src/a.rs\"}"],
        ),
        (
            "nl -ba src/a.rs | sed -n '1,120p'",
            &["read {\"limit\":120,\"offset\":1,\"path\":\"src/a.rs\"}"],
        ),
        (
            "cat src/a.rs | head -n 5",
            &["read {\"limit\":5,\"path\":\"src/a.rs\"}"],
        ),
        (
            "cat src/a.rs | grep -n foo",
            &["grep {\"path\":\"src/a.rs\",\"pattern\":\"foo\"}"],
        ),
        (
            "grep foo < src/a.rs",
            &["grep {\"path\":\"src/a.rs\",\"pattern\":\"foo\"}"],
        ),
        ("cd src && cat a.rs", &["read {\"path\":\"src/a.rs\"}"]),
        ("nl -b a src/a.rs", &["read {\"path\":\"src/a.rs\"}"]),
        ("ls", &["read {\"limit\":200,\"path\":\".\"}"]),
        ("ls -la src", &["read {\"limit\":200,\"path\":\"src\"}"]),
        (
            "cat artifact://jobs/x.log",
            &["read {\"path\":\"artifact://jobs/x.log\"}"],
        ),
        (
            "tail -n 50 artifact://jobs/x.log",
            &["read {\"offset\":-50,\"path\":\"artifact://jobs/x.log\"}"],
        ),
    ];
    for (command, expected) in cases {
        check_case(command, expected);
    }
}

#[test]
fn grep_family_translations() {
    let cases: &[(&str, &[&str])] = &[
        ("rg foo", &["grep {\"pattern\":\"foo\"}"]),
        (
            "rg -n -i 'fn main' src",
            &["grep {\"case\":false,\"path\":\"src\",\"pattern\":\"fn main\"}"],
        ),
        (
            "rg -F 'a.b(' -t rust",
            &["grep {\"glob\":[\"*.rs\"],\"literal\":true,\"pattern\":\"a.b(\"}"],
        ),
        (
            "rg -l TODO src tests",
            &[
                "grep {\"files_only\":true,\"path\":\"src\",\"pattern\":\"TODO\"}",
                "grep {\"files_only\":true,\"path\":\"tests\",\"pattern\":\"TODO\"}",
            ],
        ),
        (
            "rg -C 3 foo src | head -50",
            &[
                "grep {\"context_after\":3,\"context_before\":3,\"path\":\"src\",\"pattern\":\"foo\"}",
            ],
        ),
        (
            "grep -rn \"foo\\|bar\" src/",
            &["grep {\"path\":\"src\",\"pattern\":\"foo|bar\"}"],
        ),
        (
            "grep -rniw --include='*.ts' handler .",
            &["grep {\"case\":false,\"glob\":[\"*.ts\"],\"pattern\":\"handler\",\"word\":true}"],
        ),
        (
            "grep -E 'a+b' -r src",
            &["grep {\"path\":\"src\",\"pattern\":\"a+b\"}"],
        ),
        (
            "grep 'a+b' -r src",
            &["grep {\"path\":\"src\",\"pattern\":\"a\\\\+b\"}"],
        ),
        (
            "cargo build && rg foo src",
            &["grep {\"path\":\"src\",\"pattern\":\"foo\"}"],
        ),
    ];
    for (command, expected) in cases {
        check_case(command, expected);
    }
}

#[test]
fn find_family_translations() {
    let cases: &[(&str, &[&str])] = &[
        ("find . -name '*.rs'", &["find {\"glob\":[\"*.rs\"]}"]),
        (
            "find src -type f -name '*.toml' -o -name '*.json'",
            &["find {\"glob\":[\"*.toml\",\"*.json\"],\"path\":\"src\"}"],
        ),
        ("fd -e rs", &["find {\"glob\":[\"*.rs\"]}"]),
        (
            "fd -g '*.rs' src",
            &["find {\"glob\":[\"*.rs\"],\"path\":\"src\"}"],
        ),
        ("rg --files -g '*.md'", &["find {\"glob\":[\"*.md\"]}"]),
        (
            "find . -name '*.rs' | head -20",
            &["find {\"glob\":[\"*.rs\"],\"limit\":20}"],
        ),
    ];
    for (command, expected) in cases {
        check_case(command, expected);
    }
}

#[test]
fn write_and_edit_detection() {
    let cases: &[(&str, &[&str])] = &[
        (
            "sed -i 's/a/b/' src/a.rs",
            &["edit {\"path\":\"src/a.rs\"}"],
        ),
        (
            "sed -i.bak -e 's/a/b/' src/a.rs",
            &["edit {\"path\":\"src/a.rs\"}"],
        ),
        (
            "sed -Ei 's/a/b/' src/a.rs",
            &["edit {\"path\":\"src/a.rs\"}"],
        ),
        (
            "perl -pi -e 's/a/b/' src/a.rs",
            &["edit {\"path\":\"src/a.rs\"}"],
        ),
        (
            "cat > src/new.rs <<'EOF'\nfn main() {}\nEOF",
            &["write {\"path\":\"src/new.rs\"}"],
        ),
        (
            "echo 'x = 1' >> config.toml",
            &["edit {\"path\":\"config.toml\"}"],
        ),
        (
            "printf '%s\\n' a b > notes.txt",
            &["write {\"path\":\"notes.txt\"}"],
        ),
        (
            "cat <<EOF | tee src/a.rs\nhi\nEOF",
            &["write {\"path\":\"src/a.rs\"}"],
        ),
        ("tee src/a.rs <<< 'hi'", &["write {\"path\":\"src/a.rs\"}"]),
        (": > src/a.rs", &["write {\"path\":\"src/a.rs\"}"]),
        (
            "python3 - <<'EOF'\nfrom pathlib import Path\np = Path('src/a.rs'); p.write_text(p.read_text().replace('a', 'b'))\nEOF",
            &["edit {}"],
        ),
        ("python -c \"open('out.txt','w').write('x')\"", &["edit {}"]),
        (
            "node -e \"require('fs').writeFileSync('a.json','{}')\"",
            &["edit {}"],
        ),
        ("bash -c 'cat src/a.rs'", &["read {\"path\":\"src/a.rs\"}"]),
        ("git apply <<'EOF'\n--- a/x\n+++ b/x\nEOF", &["edit {}"]),
        (
            "sudo sed -i 's/a/b/' src/a.rs",
            &["edit {\"path\":\"src/a.rs\"}"],
        ),
        (
            "echo hi > ./src/../out.txt",
            &["write {\"path\":\"out.txt\"}"],
        ),
        (
            "echo == a == && cat src/a.rs",
            &["read {\"path\":\"src/a.rs\"}"],
        ),
    ];
    for (command, expected) in cases {
        check_case(command, expected);
    }
    // stdin 参数让首段 tee 拿到字面量输入。
    let (_dir, root) = workspace();
    assert_eq!(
        blocked(&root, &root, true, "tee src/a.rs"),
        Some(vec!["write {\"path\":\"src/a.rs\"}".to_string()])
    );
}

#[test]
fn verdict_leftover_and_render() {
    let (_dir, root) = workspace();
    let verdict = check("cargo build && rg foo src", &root, &root, false).expect("应拦截");
    assert!(verdict.leftover);
    assert!(render(&verdict).ends_with("Run the remaining shell steps in a separate bash call."));
    let verdict = check("echo == a == && cat src/a.rs", &root, &root, false).expect("应拦截");
    assert!(!verdict.leftover);
    let text = render(&verdict);
    assert!(text.starts_with(
        "This bash command only reads, searches, lists, or edits workspace files; use built-in tools instead:"
    ));
    assert!(text.contains("- read {\"path\":\"src/a.rs\"}"));
    let verdict = check("sed -i 's/a/b/' src/a.rs", &root, &root, false).expect("应拦截");
    assert!(render(&verdict).contains(
        "- edit {\"path\":\"src/a.rs\"} (instead of sed -i: read the file, then edit the anchored lines)"
    ));
    let verdict = check("cat > src/new.rs <<'EOF'\nx\nEOF", &root, &root, false).expect("应拦截");
    assert!(render(&verdict).contains(
        "- write {\"path\":\"src/new.rs\"} (instead of redirect: pass the full content)"
    ));
}

#[test]
fn legitimate_shell_usage_is_left_alone() {
    let (_dir, root) = workspace();
    for command in [
        "git log | rg foo",
        "git diff | grep -c x",
        "cargo test 2>&1 | rg FAIL",
        "ps aux | grep node",
        "cargo test 2>&1 | tail -n 20",
        "git log --oneline | head -20",
        "cargo build > /tmp/build.log 2>&1",
        "cargo run > out.json",
        "echo hi > /dev/null",
        "echo hi >&2",
        "make 2>/dev/null",
        "echo hello",
        "cat <<EOF\nhi\nEOF",
        "cat /tmp/x.log",
        "cat ~/.bashrc",
        "head -n 5 /etc/os-release",
        "cat *.rs",
        "cat \"$f\"",
        "cat src/a.rs src/b.rs > c.rs",
        "\"$CMD\" > src/out.txt",
        "$GEN | tee src/a.rs",
        "sed 's/a/b/' src/a.rs",
        "sed -n '/foo/p' src/a.rs",
        "awk '{print $1}' src/a.rs",
        "rg -c foo src",
        "rg -v foo src",
        "rg -o 'v\\d+' src",
        "rg -U 'a\nb' src",
        "rg -g '!target' foo",
        "grep -c foo src/a.rs",
        "grep -rv foo src",
        "grep -r --exclude-dir=node_modules foo .",
        "rg -P '(?<=a)b' src",
        "head -c 100 src/a.rs",
        "tail -f log.txt",
        "ls -R",
        "ls src/a.rs",
        "ls /tmp",
        "find . -name '*.o' -delete",
        "find . -mtime -1",
        "find . -maxdepth 1 -name '*.rs'",
        "find . -type d -name target",
        "find /tmp -name x",
        "fd 'ma.n'",
        "fd foo -e rs",
        "nl -bt src/a.rs",
        "cp src/a.rs b.rs",
        "mv a b",
        "touch src/a.rs",
        "mkdir -p x",
        "rm -rf target",
        "cargo fmt",
        "git apply fix.patch",
        "patch -p1 < fix.patch",
        "sed -i 's/a/b/' /tmp/x.conf",
        "sed -i 's/a/b/' src/a.rs src/b.rs",
        "sed -i 's/a/b/' src/*.rs",
        "perl -pi -e 's/a/b/' $(rg -l a)",
        "rg -l foo | xargs sed -i 's/foo/bar/g'",
        "find . -name '*.rs' -exec sed -i 's/a/b/' {} +",
        "for f in src/*.rs; do sed -i 's/a/b/' \"$f\"; done",
        "echo x > /tmp/scratch.txt",
        "cat > /tmp/t.py <<EOF\nprint(1)\nEOF",
        "python3 -c 'print(1)'",
        "python3 gen.py",
        "python - <<EOF\nimport json; print(json.load(open('a.json')))\nEOF",
        "python3 - <<'EOF'\nwith open('a.txt') as f: print(f.read() + '>')\nEOF",
        "python -m json.tool <<'EOF'\n{\"a\": \"open(x, 'w')\"}\nEOF",
        "jq . package.json",
        "cat package.json | jq .name",
        "wc -l src/a.rs",
        "cargo test 2>&1 | tee test.log",
        "bash -c \"$CMD\"",
    ] {
        miss(&root, command);
    }
    // stdin 参数下无操作数的搜索读的是字面量 stdin。
    assert_eq!(blocked(&root, &root, true, "rg foo"), None);
    assert_eq!(blocked(&root, &root, true, "grep foo"), None);
    // bash cwd 在工作区外时相对路径不翻译。
    let outside = tempfile::tempdir().expect("临时目录");
    assert_eq!(blocked(&root, outside.path(), false, "cat a.rs"), None);
}
