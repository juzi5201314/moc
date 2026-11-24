// 输入框与用户消息里的引用 token：纯文本扫描与改写，不依赖 DOM。
//
// - `/command`：整条输入开头的已知命令（范围由调用方给出）；
// - `#N`：本条消息的第 N 张图片；`#name`：本条消息按路径引用的文件；
// - `@path` 与 `skill://` 等资源 URI：空白分隔，是否有效交给 core 的 `resource/inspect`。

export type RefSpan = {
  from: number;
  to: number;
  token: string;
  kind: "command" | "image" | "file-ref" | "resource";
};

const RESOURCE_URI = /^(artifact|history|local|skill|tool):\/\/\S+$/;
/** `#` 引用前后不能紧贴 ASCII 单词字符，避免把 `C#1`、`#a.rs2` 这类文本误认成引用。 */
const WORD = /[A-Za-z0-9_]/;
const NAME_TAIL = /[A-Za-z0-9_.-]/;

/** 资源 URI 是否是具体资源（不是 `skill://` 这类命名空间根）。 */
export function isResourceToken(token: string): boolean {
  if (token.startsWith("@")) return token.length > 1 && !token.startsWith("@@");
  return RESOURCE_URI.test(token) && !token.endsWith("://");
}

/**
 * 扫出全部候选引用，按位置升序。`#` 引用只认已存在的图片序号与文件名（最长匹配）；
 * 资源 token 只做形状判断，有效性由调用方查询。
 */
export function scanRefs(
  text: string,
  options: {
    command?: { start: number; end: number } | null;
    imageCount: number;
    fileNames: readonly string[];
  },
): RefSpan[] {
  const spans: RefSpan[] = [];
  const { command } = options;
  if (command && command.end > command.start) {
    spans.push({
      from: command.start,
      to: command.end,
      token: text.slice(command.start, command.end),
      kind: "command",
    });
  }
  const names = [...options.fileNames].sort((left, right) => right.length - left.length);
  for (let index = text.indexOf("#"); index >= 0; index = text.indexOf("#", index + 1)) {
    if (index > 0 && (WORD.test(text[index - 1]) || text[index - 1] === "#")) continue;
    const digits = /^\d+/.exec(text.slice(index + 1))?.[0];
    if (digits) {
      const n = Number(digits);
      const end = index + 1 + digits.length;
      if (n >= 1 && n <= options.imageCount && !WORD.test(text[end] ?? "")) {
        spans.push({ from: index, to: end, token: text.slice(index, end), kind: "image" });
        continue;
      }
    }
    const name = names.find(
      (candidate) =>
        text.startsWith(candidate, index + 1) &&
        !NAME_TAIL.test(text[index + 1 + candidate.length] ?? ""),
    );
    if (name) {
      const end = index + 1 + name.length;
      spans.push({ from: index, to: end, token: text.slice(index, end), kind: "file-ref" });
    }
  }
  const tokenPattern = /\S+/g;
  for (const match of text.matchAll(tokenPattern)) {
    const from = match.index;
    if (command && from < command.end) continue;
    if (isResourceToken(match[0]))
      spans.push({ from, to: from + match[0].length, token: match[0], kind: "resource" });
  }
  // `#` 引用落在资源 token 里面时（如 `@a#1.rs`）让位给资源 token。
  spans.sort((left, right) => left.from - right.from || right.to - left.to);
  const result: RefSpan[] = [];
  for (const span of spans) {
    const last = result.at(-1);
    if (last && span.from < last.to) continue;
    result.push(span);
  }
  return result;
}

/** 把 `#N` 图片引用按映射改写；映射为 `null` 的引用连同其后一个空格删除。 */
export function remapImageRefs(
  text: string,
  imageCount: number,
  map: (n: number) => number | null,
): string {
  const spans = scanRefs(text, { imageCount, fileNames: [] }).filter(
    (span) => span.kind === "image",
  );
  let out = "";
  let cursor = 0;
  for (const span of spans) {
    const next = map(Number(span.token.slice(1)));
    out += text.slice(cursor, span.from);
    cursor = span.to;
    if (next === null) {
      if (text[cursor] === " ") cursor += 1;
    } else {
      out += `#${next}`;
    }
  }
  return out + text.slice(cursor);
}

/** 把 `#name` 文件引用按映射改写；映射为 `null` 的引用连同其后一个空格删除。 */
export function remapFileRefs(
  text: string,
  fileNames: readonly string[],
  map: (name: string) => string | null,
): string {
  const spans = scanRefs(text, { imageCount: 0, fileNames }).filter(
    (span) => span.kind === "file-ref",
  );
  let out = "";
  let cursor = 0;
  for (const span of spans) {
    const next = map(span.token.slice(1));
    out += text.slice(cursor, span.from);
    cursor = span.to;
    if (next === null) {
      if (text[cursor] === " ") cursor += 1;
    } else {
      out += `#${next}`;
    }
  }
  return out + text.slice(cursor);
}

/**
 * 文件的引用名：空白换成 `_`；与已有名字或纯数字（会被当成图片序号）冲突时
 * 在扩展名前追加 `-2`、`-3`……
 */
export function uniqueRefName(fileName: string, taken: readonly string[]): string {
  const base = fileName.replace(/\s+/g, "_") || "file";
  const dot = base.lastIndexOf(".");
  const [stem, extension] = dot > 0 ? [base.slice(0, dot), base.slice(dot)] : [base, ""];
  const used = new Set(taken);
  let candidate = /^\d+$/.test(base) ? `file-${base}` : base;
  for (let suffix = 2; used.has(candidate); suffix++) candidate = `${stem}-${suffix}${extension}`;
  return candidate;
}

/** 在 `at` 处插入一个独立 token：前后不是空白时补空格，返回新文本与插入后的光标。 */
export function insertToken(
  text: string,
  at: number,
  token: string,
): { text: string; caret: number } {
  const before = at > 0 && !/\s/.test(text[at - 1]) ? " " : "";
  const after = at < text.length && /\s/.test(text[at]) ? "" : " ";
  const inserted = `${before}${token}${after}`;
  return { text: `${text.slice(0, at)}${inserted}${text.slice(at)}`, caret: at + inserted.length };
}
