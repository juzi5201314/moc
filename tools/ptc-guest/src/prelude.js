(() => {
  const pending = new Map();
  const errors = new WeakSet();
  const requests = [];
  let nextId = 0;
  let maxPending = 0;
  let maxOutput = 0;
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
    const message = String(e?.message ?? e);
    return {code: typeof e?.code === 'string' ? e.code : /out of memory/i.test(message) ? 'E_MEMORY_LIMIT' : 'E_JS', message, result:e?.result ?? null};
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
  function keyCheck(key) {
    if (typeof key !== 'string' || bytes(key) < 1 || bytes(key) > 128) throw fail('E_STATE_KEY', 'key must be 1..128 UTF-8 bytes');
  }
  function submit(operation) {
    try {
      const copy = json(operation, 'E_SERIALIZE');
      if (pending.size >= maxPending || nextId > 0xffffffff) throw fail('E_QUEUE_LIMIT', 'too many pending calls');
      const id = nextId++;
      return new Promise((resolve, reject) => {
        pending.set(id, {resolve, reject});
        requests.push({id, operation:copy});
      });
    } catch (e) { return Promise.reject(e); }
  }
  const tools = new Proxy(Object.create(null), {
    get(_, name) {
      if (name === 'then' || typeof name !== 'string') return undefined;
      return (args = {}) => {
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
  Object.assign(globalThis, {tools,catalog,state});
  return Object.freeze({
    start(code, count, output) {
      maxPending = count;
      maxOutput = output;
      Promise.resolve().then(() => (new (async function(){}).constructor('tools','catalog','state',code))(tools,catalog,state))
        .then(value => {
          const safe = json(value === undefined ? null : value, 'E_SERIALIZE');
          if (bytes(stringify(safe)) > maxOutput) throw fail('E_OUTPUT_LIMIT','return value exceeds the output limit');
          root = {state:'done',value:safe};
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
