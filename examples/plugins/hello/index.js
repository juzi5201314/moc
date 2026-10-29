// 示例插件：演示工具、斜杠命令、钩子与提示词贡献。
//
// 启用方式：把整个目录拷到 <仓库>/.moc/plugins/hello（或 $MOC_HOME/plugins/hello）。
// 之后模型能用 `hello` 工具，`/hello` 可以直接执行，`/plugins` 里能看到它。
//
// 这里用到的 API 全部是同步的：宿主在插件专用线程里执行，跨边界调用会阻塞该线程。
// 类型提示靠同目录的 moc.d.ts（`import("./moc")`），运行时不需要它。

import { log } from "moc";

/** @type {import("./moc").Plugin} */
export default {
  name: "hello",

  activate() {
    // 加载发生在会话之外：这里没有会话上下文，`log` 的多个参数按空格拼接。
    log.info("hello 插件启用");
  },

  tools: {
    hello: {
      description:
        "向指定对象打招呼；演示插件工具的最小形态（只读、单参数）。",
      parameters: {
        type: "object",
        properties: {
          name: { type: "string", description: "称呼，省略时用 world" },
        },
        required: [],
        additionalProperties: false,
      },
      // 省略 modes 时在 default/ask/plan 都可见；需要收窄就写 modes: ["default"]。
      run(input, ctx) {
        const who = (input.name || "world").trim();
        return `你好，${who}！模式 ${ctx.mode}，目录 ${ctx.cwd}`;
      },
    },
  },

  commands: {
    hello: {
      description: "打印一次问候（演示插件命令）",
      usage: "[称呼]",
      run(args, ctx) {
        const who = args.trim() || "world";
        return `hello ${who} ｜ 会话 ${ctx.sessionId} ｜ 模型 ${ctx.model}`;
      },
    },
  },

  hooks: {
    // 观察型钩子：`beforeToolCall` 才能拒绝或改写参数，这里只留痕。
    afterToolCall(event) {
      if (event.isError) log.warn("工具失败：", event.name);
    },
  },

  prompt() {
    return "示例插件 hello 已启用：需要示范问候格式时优先用 `hello` 工具输出。";
  },
};
