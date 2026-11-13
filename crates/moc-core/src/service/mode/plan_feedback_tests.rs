use super::*;

fn plan() -> PlanInfo {
    PlanInfo {
        plan_id: "plan_1".into(),
        title: "方案".into(),
        path: "local://plan_1.md".into(),
        markdown: "# 方案\n\n1. 先看代码\n2. 再做修改\n".into(),
        status: PlanStatus::Submitted,
        version: 2,
        previous_plan_id: Some("plan_0".into()),
        annotations: Vec::new(),
    }
}

fn annotation(start: u32, end: u32, body: &str) -> PlanAnnotation {
    PlanAnnotation {
        id: format!("a{start}"),
        start_line: start,
        end_line: end,
        quote: "先看代码".into(),
        body: body.into(),
    }
}

#[test]
fn prompt_quotes_saved_lines_and_keeps_suggestion_fence() {
    let annotations = vec![
        annotation(3, 3, "太笼统\n\n```suggestion\n1. 先看 InspectorPanel\n```"),
        annotation(4, 4, "保留"),
    ];
    let prompt = compose_revision_prompt(&plan(), &annotations, Some("保持简洁"));
    assert!(prompt.contains("(v2: 方案)"), "{prompt}");
    assert!(prompt.contains("### 1. Lines 3-3"), "{prompt}");
    assert!(prompt.contains("> 1. 先看代码"), "{prompt}");
    assert!(prompt.contains("```suggestion"), "{prompt}");
    assert!(prompt.contains("### 2. Lines 4-4"), "{prompt}");
    assert!(prompt.contains("### Overall"), "{prompt}");
    assert!(prompt.contains("保持简洁"), "{prompt}");
}

#[test]
fn long_excerpts_are_truncated() {
    let mut markdown = String::new();
    for index in 0..80 {
        markdown.push_str(&format!("line {index}\n"));
    }
    let mut info = plan();
    info.markdown = markdown;
    let prompt = compose_revision_prompt(&info, &[annotation(1, 80, "整段改")], None);
    assert!(prompt.contains("excerpt truncated"), "{prompt}");
    assert_eq!(
        prompt.matches("> line ").count(),
        MAX_PLAN_EXCERPT_LINES,
        "引用行数必须被截断"
    );
}

#[test]
fn validation_rejects_out_of_range_and_empty_feedback() {
    let info = plan();
    assert!(
        validate_plan_feedback(&info, &[], None).is_err(),
        "批注与整体意见不能同时为空"
    );
    assert!(
        validate_plan_feedback(&info, &[annotation(3, 99, "越界")], None).is_err(),
        "行号不能超出计划"
    );
    assert!(
        validate_plan_feedback(&info, &[annotation(3, 3, "   ")], None).is_err(),
        "正文不能全空白"
    );
    assert!(
        validate_plan_feedback(&info, &[], Some("整体说两句")).is_ok(),
        "只给整体意见也合法"
    );
    assert!(
        validate_plan_feedback(&info, &[annotation(3, 4, "合并两行")], None).is_ok(),
        "跨行批注合法"
    );
}
