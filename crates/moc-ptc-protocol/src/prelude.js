(() => {
  const pending = new Map();
  const errors = new WeakSet();
  const requests = [];
  let nextId = 0;
  let maxPending = 0;
  let maxTotal = 0;
  let totalCalls = 0;
  let maxOutput = 0;
  let source = '';
  const output = [];
  let outputBytes = 0;
  let root = {state:'pending'};
  const stringify = JSON.stringify;
  const parse = JSON.parse;
  function fail(code, message, result = null) {
    const e = new Error(message);
    Object.assign(e, {code, message, result});
    e.toJSON = () => ({code, message, result});
    errors.add(e);
    return e;
  }
  function normalizedError(e) {
    let message = String(e?.message ?? e);
    const code = typeof e?.code === 'string' ? e.code : /out of memory/i.test(message) ? 'E_MEMORY_LIMIT' : 'E_JS';
    if (code === 'E_JS') {
      // AsyncFunction 在脚本前插入两行；只展示用户代码的位置，不暴露宿主栈。
      const location = /<input>:(\d+)(?::(\d+))?/.exec(String(e?.stack ?? ''));
      if (location) {
        const line = Math.max(1, Number(location[1]) - 2);
        const text = source.split('\n')[line - 1] ?? '';
        message += `\nexecute:${line}${location[2] ? ':' + location[2] : ''}\n${line} | ${text.slice(0, 240)}`;
      }
      if (/before initialization|uninitialized|redefinition of parameter/.test(message)) {
        message += '\nDo not shadow host namespaces tools, catalog, or state with local variables.';
      }
    }
    if (output.length) message += '\nOutput before failure:\n' + output.join('');
    return {code, message, result:e?.result ?? null};
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
    if (code === 'E_SERIALIZE' && errors.has(value)) return json(value.toJSON(), code, seen);
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
  function print(...values) {
    const text = values.map(value => {
      if (typeof value === 'string') return value;
      if (value instanceof Error) return `${value.name}: ${value.message}`;
      const seen = new Set();
      return stringify(value, (_, item) => {
        if (typeof item === 'bigint') return String(item);
        if (item && typeof item === 'object') {
          if (seen.has(item)) return '[Circular]';
          seen.add(item);
        }
        return item;
      }) ?? String(value);
    }).join(' ') + '\n';
    const size = bytes(text);
    if (size > maxOutput - outputBytes) throw fail('E_OUTPUT_LIMIT', 'console output exceeds the output limit; print only needed fields');
    outputBytes += size;
    output.push(text);
  }
  function keyCheck(key) {
    if (typeof key !== 'string' || bytes(key) < 1 || bytes(key) > 128) throw fail('E_STATE_KEY', 'key must be 1..128 UTF-8 bytes');
  }
  function submit(operation) {
    try {
      const copy = json(operation, 'E_SERIALIZE');
      if (pending.size >= maxPending || nextId > 0xffffffff) throw fail('E_QUEUE_LIMIT', 'too many pending calls');
      if (totalCalls >= maxTotal) throw fail('E_CALL_LIMIT', 'total host call limit exceeded');
      totalCalls++;
      const id = nextId++;
      return new Promise((resolve, reject) => {
        pending.set(id, {resolve, reject});
        requests.push({id, operation:copy});
      });
    } catch (e) { return Promise.reject(e); }
  }
  const tools = new Proxy(Object.create(null), {
    get(_, name) {
      if (typeof name !== 'string') return undefined;
      return (args = {}, reject) => {
        // then 是合法工具名；Promise 同化传入两个回调，不能误发请求或悬挂等待。
        if (name === 'then' && typeof args === 'function' && typeof reject === 'function') {
          reject(fail('E_ARGUMENT', 'Await a tool call, not the tools namespace'));
          return;
        }
        if (!args || typeof args !== 'object' || Array.isArray(args)) return Promise.reject(fail('E_ARGUMENT', 'tool arguments must be an object'));
        return submit({op:'tool', name, arguments:args});
      };
    }
  });
  const catalog = Object.freeze({
    servers: () => submit({op:'servers'}),
    search: (options = {}) => {
      if (!options || typeof options !== 'object' || Array.isArray(options)
          || Object.keys(options).some(k => !['query','server','offset','limit'].includes(k))
          || ['query','server'].some(k => k in options && typeof options[k] !== 'string')
          || ['offset','limit'].some(k => k in options && (!Number.isSafeInteger(options[k]) || options[k] < 0))
          || ('limit' in options && (options.limit < 1 || options.limit > 100))) return Promise.reject(fail('E_ARGUMENT','invalid search options'));
      return submit({op:'search', ...options});
    },
    describe: name => typeof name === 'string' ? submit({op:'describe',name}) : Promise.reject(fail('E_ARGUMENT','name must be a string'))
  });
  const state = Object.freeze({
    store: async (key, value) => { keyCheck(key); await submit({op:'store', key, value:json(value,'E_STATE_VALUE')}); },
    load: async key => { keyCheck(key); const r = await submit({op:'load',key}); return r.found ? r.value : undefined; },
    remove: async key => { keyCheck(key); return await submit({op:'remove',key}); },
    keys: () => submit({op:'keys'})
  });
  Object.assign(globalThis, {tools,catalog,state,print,console:Object.freeze({log:print,info:print,warn:print,error:print,debug:print})});
  return Object.freeze({
    start(code, count, total, outputLimit) {
      maxPending = count;
      maxTotal = total;
      totalCalls = 0;
      maxOutput = outputLimit;
      source = code;
      Promise.resolve().then(() => (new (async function(){}).constructor(code))())
        .then(value => {
          const safe = json(value === undefined ? null : value, 'E_SERIALIZE');
          const text = value === undefined ? '' : typeof safe === 'string' ? safe : stringify(safe);
          if (outputBytes + bytes(text) > maxOutput) throw fail('E_OUTPUT_LIMIT','return value and console output exceed the output limit');
          root = {state:'done',value:outputBytes ? output.join('') + text : safe};
        }).catch(e => { root = {state:'failed',error:normalizedError(e)}; });
    },
    take() {
      const batch = requests.splice(0);
      if (bytes(stringify(batch)) > maxOutput) {
        root = {state:'failed',error:normalizedError(fail('E_OUTPUT_LIMIT','request batch exceeds the output limit'))};
        return '[]';
      }
      return stringify(batch);
    },
    status: () => stringify(root),
    complete(id, result, error) {
      const p = pending.get(id);
      if (!p) throw fail('E_PTC_PROTOCOL','unknown completion ID');
      pending.delete(id);
      if (error) p.reject(fail(error.code,error.message,error.result));
      else p.resolve(result);
    }
  });
})()
