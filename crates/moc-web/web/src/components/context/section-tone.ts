// 上下文分块的色调：messages 用强调色，其余按出现顺序递减透明度的灰阶；
// 简略窗口与详细窗口的占用条、图例共用，保证同一分块在两处颜色一致。
export function sectionTone(name: string, index: number): { className: string; opacity: number } {
  return name === "messages"
    ? { className: "bg-brand/60", opacity: 1 }
    : { className: "bg-foreground/25", opacity: 1 - index * 0.1 };
}

// 消息条目按来源着色与展示名：与 core/service/context/outline.rs 的 origin 分类保持一致。
const ORIGIN_META: Record<string, { label: string; tone: string }> = {
  user_input: { label: "User input", tone: "bg-chart-1" },
  assistant_output: { label: "Assistant output", tone: "bg-chart-5" },
  tool_result: { label: "Tool result", tone: "bg-chart-6" },
  manual_command: { label: "Manual command", tone: "bg-chart-6" },
  runtime_injection: { label: "Runtime injection", tone: "bg-chart-7" },
  session_state: { label: "Session state", tone: "bg-chart-7" },
  agent_message: { label: "Agent message", tone: "bg-chart-2" },
  goal_round: { label: "Goal round", tone: "bg-chart-7" },
  instruction: { label: "Instruction", tone: "bg-chart-3" },
  system_prompt: { label: "System prompt", tone: "bg-chart-3" },
  clm_context: { label: "CLM context", tone: "bg-chart-3" },
};

/** 筛选器中来源的固定展示顺序；未收录来源排在末尾按字典序排列。 */
export const ORIGIN_ORDER = Object.keys(ORIGIN_META);

export function originTone(origin: string): string {
  return ORIGIN_META[origin]?.tone ?? "bg-chart-3";
}

export function originLabel(origin: string): string {
  const known = ORIGIN_META[origin]?.label;
  if (known) return known;
  // 未知 origin 兜底：下划线转空格并首字母大写，避免暴露原始 snake_case
  const spaced = origin.replace(/_/g, " ");
  return spaced ? spaced[0].toUpperCase() + spaced.slice(1) : origin;
}
