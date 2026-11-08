// Workflow guest 运行时：确定性边界 + facade 到宿主请求的桥。
// 与 prelude.js 同为「攒批请求、逐批回应」协议，但消息形状是 workflow 专用的
// （site_id 由编译器写入调用点；ordinal 由宿主按到达次序分配）。
(() => {
  // —— 确定性：非确定源直接禁用（编译诊断是第一道闸，这里是第二道）——
  const NativeDate = Date;
  class WorkflowDate extends NativeDate {
    constructor() {
      if (arguments.length === 0) throw new Error('new Date() without arguments is disabled in workflows');
      super(...arguments);
    }
    static now() { throw new Error('Date.now() is disabled in workflows'); }
    static parse(value) { return NativeDate.parse(value); }
    static UTC() { return NativeDate.UTC.apply(NativeDate, arguments); }
  }
  globalThis.Date = WorkflowDate;
  Math.random = function () { throw new Error('Math.random() is disabled in workflows'); };
  delete globalThis.performance;

  const pending = new Map();  // request id -> {resolve,reject}
  const requests = [];        // 本步攒批的宿主请求
  const events = [];          // 本步攒批的事件（即发即忘）
  const channels = [];        // 全部 channel 状态（停滞内省用）
  const fills = new Map();    // hole site_id -> 已交付的体，循环内重到直跑不过线
  const actorSeqs = new Map();// local actor id -> 已发 ask 数
  let nextId = 0;
  let nextActor = 0;
  let maxPending = 0;
  let maxTotal = 0;
  let totalCalls = 0;
  let maxOutput = 0;
  let root = { state: 'pending' };
  const stringify = JSON.stringify;
  const parse = JSON.parse;
  const END = Object.freeze({ value: undefined, done: true });

  function fail(code, message, result = null) {
    const e = new Error(message);
    Object.assign(e, { code, message, result });
    e.toJSON = () => ({ code, message, result });
    return e;
  }
  function normalizedError(e) {
    const message = String(e?.message ?? e);
    return { code: typeof e?.code === 'string' ? e.code : /out of memory/i.test(message) ? 'E_MEMORY_LIMIT' : 'E_JS', message, result: e?.result ?? null };
  }
  function bytes(s) {
    let n = 0;
    for (const c of s) { const p = c.codePointAt(0); n += p < 128 ? 1 : p < 2048 ? 2 : p < 65536 ? 3 : 4; }
    return n;
  }
  function json(value, code, seen = new Set()) {
    if (value === null || typeof value === 'string' || typeof value === 'boolean') return value;
    if (typeof value === 'number' && Number.isFinite(value)) return value;
    if (typeof value !== 'object' || seen.has(value)) throw fail(code, 'value must be strict JSON without cycles');
    if (typeof value === 'function') throw fail(code, 'value must be strict JSON');
    const array = Array.isArray(value);
    const proto = Object.getPrototypeOf(value);
    if (!array && proto !== Object.prototype && proto !== null) throw fail(code, 'objects must be plain JSON objects');
    if (Object.getOwnPropertySymbols(value).length) throw fail(code, 'JSON objects cannot have symbol properties');
    seen.add(value);
    const out = array ? [] : Object.create(null);
    if (array) {
      if (Object.keys(value).length !== value.length) throw fail(code, 'JSON arrays cannot have holes or extra properties');
      for (let i = 0; i < value.length; i++) out.push(json(value[i], code, seen));
    } else {
      for (const key of Object.keys(value)) out[key] = json(value[key], code, seen);
    }
    seen.delete(value);
    return out;
  }

  function submit(siteId, operation) {
    try {
      const copy = json(operation, 'E_SERIALIZE');
      if (pending.size >= maxPending || nextId > 0xffffffff) throw fail('E_QUEUE_LIMIT', 'too many pending calls');
      if (totalCalls >= maxTotal) throw fail('E_CALL_LIMIT', 'total host call limit exceeded');
      totalCalls++;
      const id = nextId++;
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        requests.push({ id, site_id: siteId, operation: copy });
      });
    } catch (e) { return Promise.reject(e); }
  }

  function emit(event) {
    // 序列化检查在调用点同步抛出（report 的环/非 JSON 在 report() 调用上炸）。
    events.push(json(event, 'E_SERIALIZE'));
  }

  // —— channel：无界 FIFO、一值一收、for await 排空 ——
  // items/waiters 用 head 指针 + 压实，避免 shift() 的 O(n) 拷贝。
  const COMPACT_MIN = 1024;
  function makeChannel(name) {
    const label = name === undefined || name === null ? 'channel #' + (channels.length + 1) : String(name);
    const state = { label, items: [], head: 0, waiters: [], waitHead: 0, closed: false };
    channels.push(state);
    function compact() {
      if (state.head >= COMPACT_MIN && state.head * 2 >= state.items.length) {
        state.items = state.items.slice(state.head);
        state.head = 0;
      }
      if (state.waitHead >= COMPACT_MIN && state.waitHead * 2 >= state.waiters.length) {
        state.waiters = state.waiters.slice(state.waitHead);
        state.waitHead = 0;
      }
    }
    function takeWaiter() {
      if (state.waitHead >= state.waiters.length) return undefined;
      const waiter = state.waiters[state.waitHead];
      state.waiters[state.waitHead] = undefined;
      state.waitHead += 1;
      compact();
      return waiter;
    }
    function next() {
      if (state.head < state.items.length) {
        const item = state.items[state.head];
        state.items[state.head] = undefined;
        state.head += 1;
        compact();
        return Promise.resolve({ value: item, done: false });
      }
      if (state.closed) return Promise.resolve(END);
      return new Promise((resolve) => { state.waiters.push(resolve); });
    }
    const channel = {
      send(item) {
        if (state.closed) throw fail('ChannelClosed', 'Channel "' + label + '" is closed: send() after close()');
        const waiter = takeWaiter();
        if (waiter !== undefined) waiter({ value: item, done: false });
        else state.items.push(item);
      },
      close() {
        if (state.closed) return;
        state.closed = true;
        for (let waiter = takeWaiter(); waiter !== undefined; waiter = takeWaiter()) waiter(END);
      },
    };
    channel[Symbol.asyncIterator] = function () {
      const iterator = {
        next,
        return: () => Promise.resolve(END),
      };
      iterator[Symbol.asyncIterator] = () => iterator;
      return iterator;
    };
    return channel;
  }

  function normalizePersona(name, persona) {
    const out = {};
    if (typeof persona === 'string') out.system = persona;
    else if (persona !== null && typeof persona === 'object') {
      if (typeof persona.name === 'string') out.name = persona.name;
      if (typeof persona.system === 'string') out.system = persona.system;
      if (persona.model !== undefined) out.model = persona.model;
      if (persona.tools !== undefined) {
        if (!Array.isArray(persona.tools) || persona.tools.some((tool) => typeof tool !== 'string' || !tool.trim())) {
          throw new TypeError('agent tools 必须是非空工具名称数组');
        }
        out.tools = Object.freeze([...persona.tools]);
      }
    }
    if (out.name === undefined && name !== undefined) out.name = name;
    return out;
  }

  // agent() 需要同步建 local id 并发 create_actor 事件。
  function agent(siteId, name, persona) {
    const normalized = normalizePersona(name, persona);
    const localId = nextActor++;
    actorSeqs.set(localId, 0);
    emit({ event: 'create_actor', local_id: localId, site_id: siteId, name: normalized.name ?? null, persona: normalized });
    return Object.freeze({
      ask(site, instructions) {
        const seq = actorSeqs.get(localId);
        actorSeqs.set(localId, seq + 1);
        return submit(site, { op: 'ask', actor: localId, actor_seq: seq, instructions: String(instructions) });
      },
    });
  }

  function evalFill(evaluate, code) {
    // evaluate 是降级在站点处插入的 (__src)=>eval(__src)：体的求值闭包住站点处的
    // 全部绑定（__wf 在内）。体文本是 (async()=>{...}) 形态，eval 出函数再调它。
    return Promise.resolve().then(() => evaluate(code)());
  }

  function hole(siteId, name, prompt, evaluate, body) {
    // 预置体形式：直接调体，不过线。
    if (body !== undefined) {
      try { return Promise.resolve(body()); } catch (e) { return Promise.reject(e); }
    }
    const remembered = fills.get(siteId);
    if (remembered !== undefined) return evalFill(evaluate, remembered);
    return submit(siteId, { op: 'hole', name, prompt: prompt ?? null }).then((answer) => {
      fills.set(siteId, answer.body);
      return evalFill(evaluate, answer.body);
    });
  }

  const files = Object.freeze({
    glob: (siteId, pattern) => submit(siteId, { op: 'world_read', kind: 'files_glob', arguments: [pattern] }),
    read: (siteId, path) => submit(siteId, { op: 'world_read', kind: 'files_read', arguments: [path] }),
    grep: (siteId, pattern, glob) => submit(siteId, { op: 'world_read', kind: 'files_grep', arguments: [pattern, glob ?? null] }),
  });
  const git = Object.freeze({
    changedFiles: (siteId, base) => submit(siteId, { op: 'world_read', kind: 'git_changed_files', arguments: [base ?? null] }),
    diff: (siteId, base, path) => submit(siteId, { op: 'world_read', kind: 'git_diff', arguments: [base ?? null, path ?? null] }),
    status: (siteId) => submit(siteId, { op: 'world_read', kind: 'git_status', arguments: [] }),
    log: (siteId, count) => submit(siteId, { op: 'world_read', kind: 'git_log', arguments: [count ?? null] }),
  });
  const artifact = Object.freeze({
    file: (siteId, id, path, opts) => submit(siteId, { op: 'publish_artifact', member: 'file', arguments: [id, path, opts ?? null] }),
    markdown: (siteId, id, content, opts) => submit(siteId, { op: 'publish_artifact', member: 'markdown', arguments: [id, content, opts ?? null] }),
    chart: (siteId, id, spec) => emit({ event: 'declare_artifact', site_id: siteId, member: 'chart', arguments: [id, spec] }),
    table: (siteId, id, spec) => emit({ event: 'declare_artifact', site_id: siteId, member: 'table', arguments: [id, spec] }),
    metrics: (siteId, id, spec) => emit({ event: 'declare_artifact', site_id: siteId, member: 'metrics', arguments: [id, spec] }),
    board: (siteId, id, spec) => emit({ event: 'declare_artifact', site_id: siteId, member: 'board', arguments: [id, spec] }),
  });
  const world = Object.freeze({
    run: (siteId, cmd, argv, opts) => {
      const options = { timeout_ms: opts?.timeoutMs ?? null, output: opts?.output ?? null };
      return submit(siteId, { op: 'world_run', command: String(cmd), args: (argv ?? []).map(String), options });
    },
  });

  const __wf = {
    agent,
    model: (siteId, id) => Object.freeze({ model_id: String(id), ref_id: siteId }),
    phase: (siteId, name) => emit({ event: 'phase_entered', site_id: siteId, name: String(name) }),
    log: (siteId, message) => emit({ event: 'log', site_id: siteId, message: String(message) }),
    report: (siteId, item, artifactId) => emit({ event: 'report', site_id: siteId, item, artifact_id: artifactId ?? null }),
    artifact, files, git, world,
    channel: (siteId, name) => makeChannel(name),
    future: (siteId, body) => {
      try { const r = body(); return r instanceof Promise ? r : Promise.resolve(r); } catch (e) { return Promise.reject(e); }
    },
    hole,
    args: undefined,   // start() 里注入
    run: undefined,    // start() 里注入
  };

  return Object.freeze({
    start(code, argsJson, runJson, count, total, output) {
      maxPending = count;
      maxTotal = total;
      totalCalls = 0;
      maxOutput = output;
      __wf.args = Object.freeze(parse(argsJson));
      const runInfo = parse(runJson);
      __wf.run = Object.freeze({ id: runInfo.id, resumedFrom: runInfo.resumed_from ?? undefined });
      Promise.resolve()
        .then(() => (new (async function () {}).constructor('__wf', code))(__wf))
        .then((value) => {
          const safe = json(value === undefined ? null : value, 'E_SERIALIZE');
          if (bytes(stringify(safe)) > maxOutput) throw fail('E_OUTPUT_LIMIT', 'return value exceeds the output limit');
          root = { state: 'done', value: safe };
        })
        .catch((e) => { root = { state: 'failed', error: normalizedError(e) }; });
    },
    take() {
      const reqs = requests.splice(0);
      const evs = events.splice(0);
      const batch = stringify({ events: evs, requests: reqs });
      if (bytes(batch) > maxOutput) {
        root = { state: 'failed', error: normalizedError(fail('E_OUTPUT_LIMIT', 'request batch exceeds the output limit')) };
        return '{"events":[],"requests":[]}';
      }
      return batch;
    },
    status: () => stringify(root),
    complete(id, result, error) {
      const p = pending.get(id);
      if (!p) throw fail('E_PTC_PROTOCOL', 'unknown completion ID');
      pending.delete(id);
      if (error) {
        const e = new Error(error.message || 'workflow host error');
        e.code = error.code;
        if (error.result !== undefined && error.result !== null) e.result = error.result;
        p.reject(e);
      } else {
        p.resolve(result);
      }
    },
    // 停滞内省：无在飞宿主请求 && 脚本未完成时，报告挂在 channel 上的接收者。
    stalled() {
      if (pending.size > 0) return 'null';
      const waiting = [];
      for (const state of channels) {
        const n = state.waiters.length - state.waitHead;
        if (n > 0) waiting.push({ channel: state.label, receivers: n });
      }
      return stringify({ waiting });
    },
  });
})()
