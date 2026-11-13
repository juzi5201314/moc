use crate::event::commands::parse_goal_action;
use moc_protocol::methods::GoalAction;

#[test]
fn parse_goal_action_covers_all_subcommands() {
    assert_eq!(parse_goal_action("").unwrap(), GoalAction::Show);
    assert_eq!(parse_goal_action("  ").unwrap(), GoalAction::Show);
    assert_eq!(parse_goal_action("pause").unwrap(), GoalAction::Pause);
    assert_eq!(parse_goal_action("resume").unwrap(), GoalAction::Resume);
    assert_eq!(parse_goal_action("clear").unwrap(), GoalAction::Unset);
    assert_eq!(
        parse_goal_action("edit 换个说法").unwrap(),
        GoalAction::Edit {
            objective: "换个说法".into()
        }
    );
    // `edit` 必须跟空白分隔；`editing` 整段当 objective 建目标。
    assert_eq!(
        parse_goal_action("editing plans").unwrap(),
        GoalAction::Create {
            objective: "editing plans".into()
        }
    );
    assert_eq!(
        parse_goal_action("把这件事做完").unwrap(),
        GoalAction::Create {
            objective: "把这件事做完".into()
        }
    );
    assert!(parse_goal_action("edit").is_err());
}
