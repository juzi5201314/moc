// 上下文分块的色调：messages 用强调色，其余按出现顺序递减透明度的灰阶；
// 简略窗口与详细窗口的占用条、图例共用，保证同一分块在两处颜色一致。
export function sectionTone(name: string, index: number): { className: string; opacity: number } {
  return name === "messages"
    ? { className: "bg-brand/60", opacity: 1 }
    : { className: "bg-foreground/25", opacity: 1 - index * 0.1 };
}

// 消息条目按来源着色：用户输入、模型输出、工具结果、运行时注入各一色，其余指令类用中性灰。
const ORIGIN_TONE: Record<string, string> = {
  user_input: "bg-chart-1",
  assistant_output: "bg-chart-5",
  tool_result: "bg-chart-6",
  manual_command: "bg-chart-6",
  runtime_injection: "bg-chart-7",
  goal_round: "bg-chart-7",
  session_state: "bg-chart-7",
  agent_message: "bg-chart-2",
};

export function originTone(origin: string): string {
  return ORIGIN_TONE[origin] ?? "bg-chart-3";
}
