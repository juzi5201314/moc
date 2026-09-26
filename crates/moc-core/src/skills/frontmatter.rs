//! SKILL.md frontmatter 解析：agentskills.io 规范字段 + 社区扩展字段。
//!
//! 解码刻意宽松：坏 YAML 或缺失必填字段交由上层决定跳过并告警，
//! 不在本层 `deny_unknown_fields`——社区 skill 常带各家扩展键。

use std::collections::BTreeMap;

use serde::Deserialize;

/// frontmatter 结构。`name` / `description` 的必填性由上层校验。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SkillFrontmatter {
    /// 规范要求与目录名一致；不一致时告警但加载。
    pub name: Option<String>,
    /// 规范必填（1-1024 字符）；缺失时跳过该 skill。
    pub description: Option<String>,
    pub license: Option<String>,
    /// 环境要求说明；仅作元数据保留。
    pub compatibility: Option<String>,
    /// 任意 string→string 扩展元数据；标量值自动字符串化，其他形态忽略。
    #[serde(default, deserialize_with = "stringify_metadata")]
    pub metadata: BTreeMap<String, String>,
    /// Experimental：空格分隔的工具授权清单；记录但不强制。
    #[serde(alias = "allowed-tools")]
    pub allowed_tools: Option<String>,
    /// Claude 风格触发提示；拼入目录行。
    #[serde(alias = "when-to-use")]
    pub when_to_use: Option<String>,
    /// Cursor 风格文件相关性（`paths` 别名等价）。
    #[serde(alias = "paths")]
    pub globs: Option<StringList>,
    #[serde(alias = "enabled")]
    pub enabled: Option<bool>,
    /// 不暴露给模型；用户显式 `/skill:` 调用不受影响。
    #[serde(alias = "disable-model-invocation")]
    pub disable_model_invocation: Option<bool>,
}

/// 单串或串列表（社区写法两种都常见）。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringList {
    One(String),
    Many(Vec<String>),
}

/// 规范要求 metadata 是 string→string 映射；YAML 来源里标量会被字符串化，
/// 复合值（列表/嵌套映射）丢弃该键而不是让整个 frontmatter 失败。
fn stringify_metadata<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: BTreeMap<String, serde_yaml::Value> = BTreeMap::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|(key, value)| match value {
            serde_yaml::Value::String(text) => Some((key, text)),
            serde_yaml::Value::Number(number) => Some((key, number.to_string())),
            serde_yaml::Value::Bool(flag) => Some((key, flag.to_string())),
            _ => None,
        })
        .collect())
}

impl StringList {
    pub(crate) fn into_vec(self) -> Vec<String> {
        match self {
            StringList::One(value) => vec![value],
            StringList::Many(values) => values,
        }
    }
}

/// frontmatter 解析结果：元数据 + 去掉 frontmatter 的正文。
#[derive(Debug, Clone)]
pub struct ParsedSkill {
    pub frontmatter: SkillFrontmatter,
    pub body: String,
}

#[derive(Debug, thiserror::Error)]
#[error("SKILL.md frontmatter 无效：{0}")]
pub struct FrontmatterError(pub String);

/// 解析 SKILL.md 文本。
///
/// frontmatter 形态为文件头 `---` 行 + YAML + `---` 行；没有 frontmatter 时
/// 视为元数据全空（上层按缺 description 跳过）。
pub fn parse(text: &str) -> Result<ParsedSkill, FrontmatterError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return Ok(ParsedSkill {
            frontmatter: SkillFrontmatter::default(),
            body: text.to_owned(),
        });
    };
    let (yaml, body) = match rest.find("\n---") {
        Some(position) => {
            let yaml = &rest[..position];
            let after = &rest[position + 1 + 3..];
            // 闭合行可能是 "---" 或 "---\r"；吃掉紧随的换行。
            let after = after
                .strip_prefix('\r')
                .and_then(|tail| tail.strip_prefix('\n'))
                .unwrap_or_else(|| after.strip_prefix('\n').unwrap_or(after));
            (yaml, after)
        }
        None => {
            return Err(FrontmatterError("frontmatter 缺少闭合的 `---` 行".into()));
        }
    };
    let frontmatter: SkillFrontmatter = serde_yaml::from_str(yaml)
        .map_err(|error| FrontmatterError(format!("YAML 解析失败：{error}")))?;
    Ok(ParsedSkill {
        frontmatter,
        body: body.to_owned(),
    })
}

impl SkillFrontmatter {
    /// 归一化 globs（`globs` / `paths`，单串或列表）。
    pub fn glob_list(&self) -> Vec<String> {
        self.globs
            .as_ref()
            .map(|list| list.clone().into_vec())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spec_fields_and_extensions() {
        let text = "---\nname: pdf-report\ndescription: 生成 PDF 报告\nlicense: MIT\ncompatibility: 需要 typst\nmetadata:\n  author: soeur\n  version: \"2\"\nallowed-tools: \"Bash(typst:*) Read\"\nwhen_to_use: 用户要求生成 PDF 时\nglobs:\n  - \"*.md\"\ndisable-model-invocation: true\n---\n# 正文\n指令内容。";
        let parsed = parse(text).unwrap();
        assert_eq!(parsed.frontmatter.name.as_deref(), Some("pdf-report"));
        assert_eq!(
            parsed.frontmatter.description.as_deref(),
            Some("生成 PDF 报告")
        );
        assert_eq!(
            parsed
                .frontmatter
                .metadata
                .get("author")
                .map(String::as_str),
            Some("soeur")
        );
        assert_eq!(
            parsed.frontmatter.when_to_use.as_deref(),
            Some("用户要求生成 PDF 时")
        );
        assert_eq!(parsed.frontmatter.glob_list(), vec!["*.md".to_owned()]);
        assert_eq!(parsed.frontmatter.disable_model_invocation, Some(true));
        assert_eq!(parsed.body, "# 正文\n指令内容。");
    }

    #[test]
    fn paths_alias_single_string_and_globs_alias() {
        let parsed = parse("---\npaths: \"src/**/*.rs\"\n---\n正文").unwrap();
        assert_eq!(
            parsed.frontmatter.glob_list(),
            vec!["src/**/*.rs".to_owned()]
        );
        let parsed = parse("---\nglobs: [\"a\", \"b\"]\n---\n正文").unwrap();
        assert_eq!(parsed.frontmatter.glob_list().len(), 2);
    }

    #[test]
    fn missing_frontmatter_yields_empty_metadata() {
        let parsed = parse("# 纯正文\n没有 frontmatter").unwrap();
        assert!(parsed.frontmatter.name.is_none());
        assert_eq!(parsed.body, "# 纯正文\n没有 frontmatter");
    }

    #[test]
    fn crlf_frontmatter_and_unclosed_are_handled() {
        let parsed = parse("---\r\nname: x\r\ndescription: y\r\n---\r\n正文").unwrap();
        assert_eq!(parsed.frontmatter.name.as_deref(), Some("x"));
        assert_eq!(parsed.body, "正文");

        assert!(parse("---\nname: x\n正文").is_err());
    }

    #[test]
    fn bad_yaml_reports_error() {
        let error = parse("---\nname: [未闭合\n---\n正文").unwrap_err();
        assert!(error.to_string().contains("YAML"));
    }

    #[test]
    fn metadata_round_trips_through_yaml_types() {
        let parsed =
            parse("---\nmetadata:\n  n: 3\n  ok: \"yes\"\n  nested:\n    a: 1\n---\n正文").unwrap();
        // 标量字符串化；复合值丢弃。
        assert_eq!(
            parsed.frontmatter.metadata.get("n").map(String::as_str),
            Some("3")
        );
        assert_eq!(
            parsed.frontmatter.metadata.get("ok").map(String::as_str),
            Some("yes")
        );
        assert!(!parsed.frontmatter.metadata.contains_key("nested"));
    }
}
