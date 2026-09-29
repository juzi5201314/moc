//! SKILL.md 目录发现：多来源扫描、rank 优先级与去重。
//!
//! 来源与 rank（小者优先；项目级随向上遍历深度递增，越近越优先）：
//! 1. 项目 `.moc/skills`（rank 100+depth）
//! 2. 项目 `.agents/skills`（rank 200+depth，社区通用约定）
//! 3. 配置 `[skills] dirs`（rank 300）
//! 4. 用户 `~/.moc/skills`（rank 400）
//! 5. 用户 `~/.agents/skills`（rank 500）
//!
//! 同名冲突取 rank 最小者并告警；SKILL.md 真实路径去重防 symlink 双载
//! （skills.sh 等安装器以 symlink 方式分发）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::SkillIndex;
use crate::config::SkillsConfig;

/// skill 目录的扫描深度上限与目录数上限，防御异常深的仓库。
const MAX_SCAN_DEPTH: usize = 4;
const MAX_SCAN_DIRS: usize = 2000;
/// 规范 name：1-64 位小写字母/数字/连字符，不以连字符开头结尾，无连续连字符。
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// 从 cwd 向上（含 git root 本身）收集目录；越近越靠前。
fn project_dirs(cwd: &Path, git_root: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in cwd.ancestors() {
        dirs.push(dir.to_owned());
        if Some(dir) == git_root {
            break;
        }
    }
    dirs
}

/// 构建当前项目的 skill 索引。
///
/// `home` 为 moc 数据目录（`~/.moc`），`user_home` 为真实用户主目录
/// （`~/.agents` 的挂靠点）；测试注入隔离目录避免扫到真实环境。
pub fn discover_index(
    cwd: &Path,
    home: &Path,
    user_home: Option<&Path>,
    config: &SkillsConfig,
) -> SkillIndex {
    if !config.enabled {
        return SkillIndex::empty();
    }
    let git_root = crate::prompt::context_files::git_root(cwd);
    let mut roots: Vec<(u32, PathBuf, String)> = Vec::new();
    for (depth, dir) in project_dirs(cwd, git_root.as_deref())
        .into_iter()
        .enumerate()
    {
        let depth = depth as u32;
        if config.moc_project {
            roots.push((
                100 + depth,
                dir.join(".moc").join("skills"),
                format!("项目 .moc/skills（{}）", dir.display()),
            ));
        }
        if config.agents_project {
            roots.push((
                200 + depth,
                dir.join(".agents").join("skills"),
                format!("项目 .agents/skills（{}）", dir.display()),
            ));
        }
    }
    for dir in &config.dirs {
        let expanded = crate::config::expand_user_path(dir);
        roots.push((300, expanded, format!("配置 dirs（{dir}）")));
    }
    if config.moc_user {
        roots.push((400, home.join("skills"), "用户 .moc/skills".into()));
    }
    if config.agents_user
        && let Some(user_home) = user_home
    {
        roots.push((
            500,
            user_home.join(".agents").join("skills"),
            "用户 ~/.agents/skills".into(),
        ));
    }
    roots.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    let mut index = SkillIndex::empty();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for (_rank, root, source) in &roots {
        if !root.is_dir() {
            continue;
        }
        for (skill_dir, dir_name) in scan_skill_dirs(root) {
            let Some(real) = skill_dir.canonicalize().ok() else {
                continue;
            };
            if !seen.insert(real.clone()) {
                continue;
            }
            let file = real.join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let parsed = match super::frontmatter::parse(&text) {
                Ok(parsed) => parsed,
                Err(error) => {
                    tracing::warn!("skill `{}` 解析失败，已跳过：{error}", file.display());
                    continue;
                }
            };
            let super::frontmatter::SkillFrontmatter {
                name: declared_name,
                description: description_field,
                when_to_use,
                globs,
                disable_model_invocation,
                enabled,
                ..
            } = parsed.frontmatter;
            if enabled == Some(false) {
                continue;
            }
            let Some(description) = description_field.filter(|d| !d.trim().is_empty()) else {
                tracing::warn!("skill `{}` 缺少 description，已跳过", file.display());
                continue;
            };
            let declared = declared_name.filter(|name| valid_name(name));
            let name = declared.clone().unwrap_or_else(|| dir_name.clone());
            if declared.as_deref() != Some(dir_name.as_str()) {
                tracing::warn!(
                    "skill `{}` 的 frontmatter name（{declared:?}）与目录名（{dir_name}）不一致，使用 `{name}`",
                    file.display()
                );
            }
            if config.disable.iter().any(|blocked| blocked == &name) {
                continue;
            }
            if index.get(&name).is_some() {
                tracing::warn!("skill `{name}` 重复（{source} 与既有来源同名），保留更高优先级者");
                continue;
            }
            index.insert(super::SkillMeta {
                name,
                description,
                when_to_use: when_to_use.filter(|text| !text.trim().is_empty()),
                globs: globs
                    .map(super::frontmatter::StringList::into_vec)
                    .unwrap_or_default(),
                model_invocable: disable_model_invocation != Some(true),
                base_dir: real,
                source: source.clone(),
            });
        }
    }
    index
}

/// 扫描一个 skill 根目录：`<root>/<name>/SKILL.md`，支持嵌套分类目录。
///
/// 返回（skill 目录, 目录名）；跳过点目录、已含 SKILL.md 的子树与超限目录。
fn scan_skill_dirs(root: &Path) -> Vec<(PathBuf, String)> {
    let mut found = Vec::new();
    let mut visited = 0usize;
    walk(root, 0, &mut found, &mut visited);
    found
}

fn walk(dir: &Path, depth: usize, found: &mut Vec<(PathBuf, String)>, visited: &mut usize) {
    if depth > MAX_SCAN_DEPTH || *visited >= MAX_SCAN_DIRS {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    children.sort();
    for child in children {
        *visited += 1;
        if *visited >= MAX_SCAN_DIRS {
            return;
        }
        let Some(name) = child
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        // 跟随 symlink：skills.sh 以 symlink 安装 skill 目录。
        let is_dir = std::fs::metadata(&child)
            .map(|meta| meta.is_dir())
            .unwrap_or(false);
        if !is_dir {
            continue;
        }
        if child.join("SKILL.md").is_file() {
            found.push((child, name));
            continue;
        }
        walk(&child, depth + 1, found, visited);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dirs: Vec<String>, disable: Vec<String>) -> SkillsConfig {
        SkillsConfig {
            enabled: true,
            dirs,
            disable,
            ..SkillsConfig::default()
        }
    }

    fn write_skill(root: &Path, name: &str, frontmatter: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\n{frontmatter}\n---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn valid_name_enforces_spec() {
        assert!(valid_name("pdf-report"));
        assert!(valid_name("a1"));
        assert!(!valid_name(""));
        assert!(!valid_name("Pdf"));
        assert!(!valid_name("-lead"));
        assert!(!valid_name("tail-"));
        assert!(!valid_name("a--b"));
        assert!(!valid_name(&"a".repeat(65)));
    }

    #[test]
    fn project_moc_beats_agents_beats_user() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        std::fs::create_dir_all(project.join(".moc/skills/shared")).unwrap();
        std::fs::create_dir_all(project.join(".agents/skills/shared")).unwrap();
        std::fs::create_dir_all(home.join(".agents/skills/shared")).unwrap();
        std::fs::create_dir_all(home.join("skills/user-only")).unwrap();
        std::fs::write(
            project.join(".moc/skills/shared/SKILL.md"),
            "---\nname: shared\ndescription: moc 版\n---\n正文",
        )
        .unwrap();
        std::fs::write(
            project.join(".agents/skills/shared/SKILL.md"),
            "---\nname: shared\ndescription: agents 版\n---\n正文",
        )
        .unwrap();
        std::fs::write(
            home.join(".agents/skills/shared/SKILL.md"),
            "---\nname: shared\ndescription: 用户 agents 版\n---\n正文",
        )
        .unwrap();
        std::fs::write(
            home.join("skills/user-only/SKILL.md"),
            "---\nname: user-only\ndescription: 用户独有\n---\n正文",
        )
        .unwrap();

        let index = discover_index(
            &project,
            &home,
            Some(&home),
            &config(Vec::new(), Vec::new()),
        );
        assert_eq!(index.get("shared").unwrap().description, "moc 版");
        assert_eq!(index.get("user-only").unwrap().description, "用户独有");
        assert_eq!(index.list().count(), 2);
    }

    #[test]
    fn disabling_a_source_reveals_the_shadowed_skill_once() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let home = root.path().join("home");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        write_skill(
            &project.join(".moc/skills"),
            "shared",
            "name: shared\ndescription: 项目版",
            "项目正文",
        );
        write_skill(
            &home.join("skills"),
            "shared",
            "name: shared\ndescription: 用户版",
            "用户正文",
        );
        let mut settings = SkillsConfig::default();
        let project_index = discover_index(&project, &home, None, &settings);
        assert_eq!(project_index.get("shared").unwrap().description, "项目版");
        settings.moc_project = false;
        let user_index = discover_index(&project, &home, None, &settings);
        assert_eq!(user_index.list().count(), 1);
        assert_eq!(user_index.get("shared").unwrap().description, "用户版");
    }

    #[test]
    fn walk_up_finds_ancestor_repo_skills() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let nested = repo.join("crates/app");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".agents/skills/root-level")).unwrap();
        std::fs::write(
            repo.join(".agents/skills/root-level/SKILL.md"),
            "---\nname: root-level\ndescription: 仓库根的 skill\n---\n正文",
        )
        .unwrap();
        // git 不可用时退化为 .git 探测。
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let index = discover_index(
            &nested,
            dir.path(),
            Some(dir.path()),
            &config(Vec::new(), Vec::new()),
        );
        assert_eq!(
            index.get("root-level").unwrap().description,
            "仓库根的 skill"
        );
    }

    #[test]
    fn missing_description_and_disabled_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        std::fs::create_dir_all(project.join(".agents/skills/bad")).unwrap();
        std::fs::write(
            project.join(".agents/skills/bad/SKILL.md"),
            "---\nname: bad\n---\n正文",
        )
        .unwrap();
        std::fs::create_dir_all(project.join(".agents/skills/off")).unwrap();
        std::fs::write(
            project.join(".agents/skills/off/SKILL.md"),
            "---\nname: off\ndescription: 已禁用\nenabled: false\n---\n正文",
        )
        .unwrap();
        std::fs::create_dir_all(project.join(".agents/skills/keep")).unwrap();
        write_skill(
            &project.join(".agents/skills"),
            "keep",
            "name: keep\ndescription: 正常",
            "正文",
        );

        let index = discover_index(
            &project,
            &home,
            Some(&home),
            &config(Vec::new(), Vec::new()),
        );
        assert!(index.get("bad").is_none());
        assert!(index.get("off").is_none());
        assert!(index.get("keep").is_some());
    }

    #[test]
    fn config_disable_list_and_custom_dirs_work() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let extra = dir.path().join("extra/skills");
        let home = dir.path().join("home");
        std::fs::create_dir_all(extra.join("from-config")).unwrap();
        write_skill(
            &extra,
            "from-config",
            "name: from-config\ndescription: 配置目录",
            "x",
        );
        std::fs::create_dir_all(project.join(".agents/skills/blocked")).unwrap();
        write_skill(
            &project.join(".agents/skills"),
            "blocked",
            "name: blocked\ndescription: 屏蔽我",
            "x",
        );

        let index = discover_index(
            &project,
            &home,
            Some(&home),
            &config(
                vec![extra.to_string_lossy().into_owned()],
                vec!["blocked".into()],
            ),
        );
        assert!(index.get("from-config").is_some());
        assert!(index.get("blocked").is_none());
    }

    #[test]
    fn symlinked_skill_dirs_are_followed_and_deduplicated() {
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            let project = dir.path().join("project");
            let home = dir.path().join("home");
            std::fs::create_dir_all(home.join(".agents/skills/real")).unwrap();
            write_skill(
                &home.join(".agents/skills/real"),
                "",
                "name: real\ndescription: 真身",
                "x",
            );
            std::fs::create_dir_all(project.join(".agents/skills")).unwrap();
            std::os::unix::fs::symlink(
                home.join(".agents/skills/real"),
                project.join(".agents/skills/link"),
            )
            .unwrap();

            let index = discover_index(
                &project,
                &home,
                Some(&home),
                &config(Vec::new(), Vec::new()),
            );
            // 项目级 symlink（rank 更小）胜出，但真实路径只装载一次。
            assert_eq!(index.list().count(), 1);
            assert!(index.get("real").is_some());
        }
    }

    #[test]
    fn nested_category_directories_are_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        let deep = project.join(".agents/skills/design/frontend");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(
            deep.join("SKILL.md"),
            "---\nname: deep\ndescription: 嵌套分类\n---\n正文",
        )
        .unwrap();

        let index = discover_index(
            &project,
            &home,
            Some(&home),
            &config(Vec::new(), Vec::new()),
        );
        assert_eq!(index.get("deep").unwrap().description, "嵌套分类");
    }

    #[test]
    fn name_directory_mismatch_still_loads_with_warning_name() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        std::fs::create_dir_all(project.join(".agents/skills/wrong-dir")).unwrap();
        std::fs::write(
            project.join(".agents/skills/wrong-dir/SKILL.md"),
            "---\nname: right-name\ndescription: 名字不一致\n---\n正文",
        )
        .unwrap();
        let index = discover_index(
            &project,
            &home,
            Some(&home),
            &config(Vec::new(), Vec::new()),
        );
        assert!(index.get("right-name").is_some());
    }
}
