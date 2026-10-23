// `moc` 宿主模块：插件通过 `import { log, session, tools, version } from "moc"` 使用。
//
// 所有跨越边界的值都经 JSON 文本传输：原生函数返回 JSON 字符串，本模块解析成值。
// 原生函数抛错时带 `code` 属性；插件可以像普通异常一样 catch 并读 `error.code`。
const host = globalThis.__moc;
if (!host) throw new Error('moc host bridge is missing');

const parse = JSON.parse;
const stringify = JSON.stringify;

function text(value) {
  if (typeof value === 'string') return value;
  try {
    return stringify(value);
  } catch {
    return String(value);
  }
}

/** 宿主 API 版本；与 moc 提示词里的 `plugin api` 对齐。 */
export const version = host.version;

function logger(level) {
  return (...parts) => host.log(level, parts.map(text).join(' '));
}

/** 写入 moc 日志（`RUST_LOG=moc_plugin=debug` 可看 debug）。 */
export const log = Object.freeze({
  debug: logger('debug'),
  info: logger('info'),
  warn: logger('warn'),
  error: logger('error'),
});

/** 当前会话快照：`{sessionId, cwd, mode, permission, model, isSubagent}`。 */
export function session() {
  return parse(host.session());
}

/**
 * 调用宿主工具。只在工具的 `run` 里可用，且只允许只读工具；
 * 命令、钩子与 `prompt` 里调用会抛 `E_PLUGIN_TOOLS_UNAVAILABLE`。
 */
export const tools = Object.freeze({
  call(name, args = {}) {
    return parse(host.toolCall(String(name), stringify(args === undefined ? {} : args)));
  },
});
