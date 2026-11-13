use super::*;
use crate::theme::ColorLevel;

fn draw(source: &str, width: usize) -> Option<String> {
    let theme = Theme::resolved(ColorLevel::TrueColor);
    render(source, width, &theme).map(|lines| {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
}

#[test]
fn flowchart_draws_nodes_and_arrow() {
    let picture = draw("flowchart TD\n  A[Start] --> B[End]\n", 40).expect("流程图");
    assert!(picture.contains("Start"), "{picture}");
    assert!(picture.contains("End"), "{picture}");
    assert!(picture.contains('▼'), "{picture}");
    assert!(!picture.contains("flowchart"), "{picture}");
}

#[test]
fn flowchart_keeps_edge_label() {
    let picture = draw("flowchart TD\n  A -->|yes| B\n", 40).expect("带标签的边");
    assert!(picture.contains("yes"), "{picture}");
}

#[test]
fn sequence_draws_message() {
    let source = "sequenceDiagram\n  participant Alice\n  participant Bob\n  Alice->>Bob: Hello\n  loop Wait\n  end\n";
    let picture = draw(source, 60).expect("时序图");
    assert!(picture.contains("Alice"), "{picture}");
    assert!(picture.contains("Bob"), "{picture}");
    assert!(picture.contains("Hello"), "{picture}");
    assert!(picture.contains('▶'), "{picture}");
    assert!(
        picture.contains("Wait") && picture.contains('└'),
        "{picture}"
    );
}

#[test]
fn state_draws_transition() {
    let source = "stateDiagram-v2\n  [*] --> Idle\n  Idle --> Run : go\n";
    let picture = draw(source, 40).expect("状态图");
    assert!(picture.contains("Idle"), "{picture}");
    assert!(picture.contains("Run"), "{picture}");
    assert!(picture.contains("go"), "{picture}");
    assert!(picture.contains('●'), "{picture}");
}

#[test]
fn class_diagram_is_left_as_code() {
    assert!(draw("classDiagram\n  Animal <|-- Duck\n", 40).is_none());
}

#[test]
fn narrow_width_still_shows_every_label() {
    let source = "flowchart LR\n  A[Alpha] --> B[Beta] --> C[Gamma] --> D[Delta]\n";
    let picture = draw(source, 16).expect("窄宽度要改成纵向");
    for label in ["Alpha", "Beta", "Gamma", "Delta"] {
        assert!(picture.contains(label), "{label} missing in {picture}");
    }
}

/// `[[label]]` 子程序形曾经整条边被丢：节点、边、目标都得在。
#[test]
fn subroutine_shape_keeps_label_and_edge() {
    let picture = draw("flowchart TD\n  A[[sub]] --> B\n", 40).expect("子程序形");
    assert!(picture.contains("sub"), "{picture}");
    assert!(picture.contains('B'), "{picture}");
    assert!(picture.contains('▼'), "{picture}");
}

/// 长链接、x/o 标记头、反箭头、无向链接、`---|标签|` 都要成边。
#[test]
fn link_variants_reach_their_targets() {
    for (link, arrowed) in [
        ("---->", true),
        ("--o", true),
        ("--x", true),
        ("<--", true),
        ("---", false),
        ("-----", false),
        ("-.-", false),
    ] {
        let source = format!("flowchart TD\n  A {link} B\n");
        let picture = draw(&source, 40).unwrap_or_else(|| panic!("{link} 应能画"));
        assert!(picture.contains('A'), "{link}: {picture}");
        assert!(picture.contains('B'), "{link}: {picture}");
        assert_eq!(
            picture.contains('▼'),
            arrowed,
            "{link} 的有向性与箭头不符: {picture}"
        );
    }
    let labeled = draw("flowchart TD\n  A ---|lbl| B\n", 40).expect("---|lbl|");
    assert!(labeled.contains("lbl"), "{labeled}");
    assert!(!labeled.contains('▼'), "{labeled}");
}

/// `A --> B --> C` 是链不是带标签边；`-- B --> C` 才是标签。
#[test]
fn chains_and_inline_labels_do_not_collide() {
    let chain = draw("flowchart TD\n  A --> B --> C\n", 40).expect("链");
    assert!(chain.contains('B'), "{chain}");

    // 夹标签边照常画出，标签进 footer；`yes` 不能成为节点。
    let labeled = draw("flowchart TD\n  A -- yes --> B\n", 40).expect("夹标签");
    assert!(labeled.contains("yes"), "{labeled}");
    assert!(!labeled.contains("│yes│"), "{labeled}");
}

/// `state` 别名、构造型、描述行和 `note` 都不能产生幻影节点。
#[test]
fn state_lines_do_not_make_phantom_nodes() {
    let source = "stateDiagram-v2\n  state c <<choice>>\n  state idle : waiting\n  note right of idle : hi\n  [*] --> c\n  c --> idle\n";
    let picture = draw(source, 40).expect("状态行");
    assert!(picture.contains('c'), "{picture}");
    assert!(picture.contains("waiting"), "{picture}");
    assert!(!picture.contains("state"), "{picture}");
    assert!(!picture.contains("note"), "{picture}");

    let multi = "stateDiagram-v2\n  note right of Idle\n    长说明\n  end note\n  [*] --> Idle\n";
    let picture = draw(multi, 40).expect("多行 note");
    assert!(picture.contains("Idle"), "{picture}");
    assert!(!picture.contains("note"), "{picture}");
}

/// stateDiagram v1 的 `id : 描述` 把描述当标签。
#[test]
fn v1_colon_description_labels_the_node() {
    let picture = draw("stateDiagram\n  s1 : waiting\n  [*] --> s1\n", 40).expect("v1 描述");
    assert!(picture.contains("waiting"), "{picture}");
}

/// `-x`、`<<->>` 这些消息箭头不能整条丢掉或造出怪 actor。
#[test]
fn sequence_extra_arrow_tokens_parse() {
    let source = "sequenceDiagram\n  A -x B: ping\n  C <<->> D: pong\n";
    let picture = draw(source, 60).expect("额外箭头");
    for name in ["A", "B", "C", "D"] {
        assert!(picture.contains(name), "{name} missing in {picture}");
    }
    assert!(picture.contains("ping"), "{picture}");
    assert!(picture.contains("pong"), "{picture}");
    assert!(!picture.contains(">>"), "{picture}");
    assert!(!picture.contains("<<"), "{picture}");
}
