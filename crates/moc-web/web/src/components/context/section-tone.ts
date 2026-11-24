// 上下文分块的色调：messages 用强调色，其余按出现顺序递减透明度的灰阶；
// 简略窗口与详细窗口的占用条、图例共用，保证同一分块在两处颜色一致。
export function sectionTone(name: string, index: number): { className: string; opacity: number } {
  return name === "messages"
    ? { className: "bg-brand/60", opacity: 1 }
    : { className: "bg-foreground/25", opacity: 1 - index * 0.1 };
}
