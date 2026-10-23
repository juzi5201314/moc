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

// 持久化只接受无损 JSON；不能让 stringify 静默丢掉函数、稀疏项或非有限数。
function jsonValue(value, stack = new Set()) {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value;
  if (typeof value === 'number' && Number.isFinite(value)) return value;
  if (typeof value !== 'object' || stack.has(value)) host.invalidValue('持久化值必须是无环、无损 JSON');
  const array = Array.isArray(value);
  if (!array && Object.getPrototypeOf(value) !== Object.prototype && Object.getPrototypeOf(value) !== null) {
    host.invalidValue('持久化对象必须是普通对象');
  }
  stack.add(value);
  const result = array ? [] : Object.create(null);
  const keys = Reflect.ownKeys(value);
  if (array) {
    if (keys.length !== value.length + 1) host.invalidValue('持久化数组不能稀疏或带额外属性');
    for (let index = 0; index < value.length; index++) {
      if (!Object.hasOwn(value, index)) host.invalidValue('持久化数组不能包含空缺项');
    }
  }
  for (const key of keys) {
    if (array && key === 'length') continue;
    if (typeof key !== 'string') host.invalidValue('持久化对象不能包含 Symbol 属性');
    const descriptor = Object.getOwnPropertyDescriptor(value, key);
    if (!descriptor.enumerable || !Object.hasOwn(descriptor, 'value')) host.invalidValue('持久化对象不能包含访问器或隐藏属性');
    Object.defineProperty(result, key, { value: jsonValue(descriptor.value, stack), enumerable: true, writable: true, configurable: true });
  }
  stack.delete(value);
  return result;
}

function frozen(value) {
  if (value && typeof value === 'object') {
    for (const key of Object.keys(value)) frozen(value[key]);
    Object.freeze(value);
  }
  return value;
}

function persist(operation, key = '', value = null) {
  return parse(host.persistence(operation, key, stringify(value)));
}

function stateProxy() {
  // 每个代理绑定创建时的会话，不能把旧 ctx 偷渡到另一个会话中使用。
  const sessionId = parse(host.session()).sessionId;
  const check = () => {
    if (parse(host.session())?.sessionId !== sessionId) {
      const error = new Error('state 代理已离开所属会话');
      error.code = 'E_PLUGIN_STATE';
      throw error;
    }
  };
  return new Proxy(Object.create(null), {
    get(_target, key) {
      check();
      if (typeof key !== 'string') return undefined;
      const result = persist('stateGet', key);
      return result.found ? frozen(result.value) : undefined;
    },
    set(_target, key, value) {
      check();
      if (typeof key !== 'string') host.invalidValue('state 键必须是字符串');
      persist('stateSet', key, jsonValue(value));
      return true;
    },
    deleteProperty(_target, key) {
      check();
      if (typeof key !== 'string') host.invalidValue('state 键必须是字符串');
      persist('stateDelete', key);
      return true;
    },
    ownKeys() { check(); return persist('stateKeys'); },
    has(_target, key) { check(); return typeof key === 'string' && persist('stateGet', key).found; },
    getOwnPropertyDescriptor(_target, key) {
      check();
      if (typeof key !== 'string') return undefined;
      const result = persist('stateGet', key);
      return result.found ? { value: frozen(result.value), writable: true, enumerable: true, configurable: true } : undefined;
    },
    defineProperty() { host.invalidValue('请使用 state[key] = value 写入'); },
    setPrototypeOf() { host.invalidValue('state 原型不可修改'); },
    preventExtensions() { host.invalidValue('state 代理不可冻结'); },
  });
}

/** 当前会话快照与插件私有的持久化 state。 */
export function session() {
  const snapshot = parse(host.session());
  if (!snapshot) host.invalidValue('当前调用没有会话');
  Object.defineProperty(snapshot, 'state', { value: stateProxy(), enumerable: true });
  return snapshot;
}
Object.defineProperty(session, 'state', { get: stateProxy });
// 供运行时给回调 ctx 附上与 session() 相同的能力；不暴露到插件全局。
host.context = session;

export const config = Object.freeze({
  register(key, definition) {
    if (typeof key !== 'string') host.invalidValue('配置键必须是字符串');
    host.configRegister(key, stringify(jsonValue(definition)));
  },
  get(key) {
    if (typeof key !== 'string') host.invalidValue('配置键必须是字符串');
    const result = persist('configGet', key);
    return result.found ? frozen(result.value) : undefined;
  },
  set(key, value) {
    if (typeof key !== 'string') host.invalidValue('配置键必须是字符串');
    persist('configSet', key, jsonValue(value));
  },
});

/**
 * 调用宿主工具。只在工具的 `run` 里可用，且只允许只读工具；
 * 命令、钩子与 `prompt` 里调用会抛 `E_PLUGIN_TOOLS_UNAVAILABLE`。
 */
export const tools = Object.freeze({
  call(name, args = {}) {
    return parse(host.toolCall(String(name), stringify(args === undefined ? {} : args)));
  },
});
