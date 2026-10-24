// MOC Web 前端：直接说 JSON-RPC 2.0 / NDJSON，不依赖构建链与框架。
//
// 连接模型：一个标签页 = 一条 WebSocket = core 侧一条独立连接。
// 同一会话打开多个标签页时，core 内的提示锁保证同一时刻只有一个提示在跑；
// 另一个进程（TUI 或另一个 moc web）持有时，session/load 会回 -32015，
// 这里随即降级成 read_only 并禁用输入。

(() => {
  "use strict";

  const TOKEN = new URLSearchParams(location.search).get("token") || "";
  const WS_URL = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws?token=${encodeURIComponent(TOKEN)}`;
  const STORAGE_KEY = "moc-web-session";

  const state = {
    socket: null,
    nextId: 1,
    pending: new Map(),
    sessions: [],
    active: null,
    readOnly: false,
    readOnlyHolder: null,
    connected: false,
    reconnecting: false,
    reconnectDelay: 600,
    promptRunning: false,
    streaming: new Map(), // prompt_id -> { element, text, reasoning }
    toolBlocks: new Map(), // tool_call_id -> element
    autoScroll: true,
    query: "",
    pluginNotice: null, // { plugin, code }：收到插件停用通知后显示顶栏「重载插件」
  };

  const el = {
    sessions: document.getElementById("sessions"),
    search: document.getElementById("search"),
    title: document.getElementById("session-title"),
    meta: document.getElementById("session-meta"),
    badges: document.getElementById("badges"),
    transcript: document.getElementById("transcript"),
    input: document.getElementById("input"),
    send: document.getElementById("send"),
    stop: document.getElementById("stop"),
    hint: document.getElementById("composer-hint"),
    modelLabel: document.getElementById("model-label"),
    connDot: document.getElementById("conn-dot"),
    connLabel: document.getElementById("conn-label"),
    newSession: document.getElementById("new-session"),
    release: document.getElementById("release-session"),
    modalRoot: document.getElementById("modal-root"),
  };

  // ---------- 基础工具 ----------

  function escapeHtml(text) {
    return String(text)
      .replaceAll("&", "&amp;")
      .replaceAll("<", "&lt;")
      .replaceAll(">", "&gt;")
      .replaceAll('"', "&quot;");
  }

  /** 轻量 markdown：代码块、行内代码、粗体、斜体、链接。先转义再替换。 */
  function renderMarkdown(source) {
    const parts = [];
    const fence = /```([^\n`]*)\n([\s\S]*?)```/g;
    let cursor = 0;
    let match;
    while ((match = fence.exec(source)) !== null) {
      parts.push({ kind: "text", value: source.slice(cursor, match.index) });
      parts.push({ kind: "code", lang: match[1].trim(), value: match[2] });
      cursor = match.index + match[0].length;
    }
    parts.push({ kind: "text", value: source.slice(cursor) });

    return parts
      .map((part) => {
        if (part.kind === "code") {
          return `<pre><code>${escapeHtml(part.value.replace(/\n$/, ""))}</code></pre>`;
        }
        let html = escapeHtml(part.value);
        html = html.replace(/`([^`\n]+)`/g, "<code>$1</code>");
        html = html.replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>");
        html = html.replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>");
        html = html.replace(
          /\[([^\]\n]+)\]\((https?:[^)\s]+)\)/g,
          '<a href="$2" target="_blank" rel="noreferrer">$1</a>',
        );
        return `<p>${html.replaceAll("\n", "<br>")}</p>`;
      })
      .join("");
  }

  function relativeTime(ms) {
    const delta = Date.now() - ms;
    if (delta < 60_000) return "刚刚";
    if (delta < 3_600_000) return `${Math.floor(delta / 60_000)} 分钟前`;
    if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)} 小时前`;
    const date = new Date(ms);
    return `${date.getMonth() + 1}/${date.getDate()}`;
  }

  function shortPath(path) {
    const parts = String(path || "").split(/[\\/]/).filter(Boolean);
    if (parts.length <= 2) return path || "";
    return `…/${parts.slice(-2).join("/")}`;
  }

  function showHint(message, tone = "warn") {
    el.hint.textContent = message;
    el.hint.hidden = false;
    el.hint.className = `hint ${tone}`;
    clearTimeout(showHint.timer);
    showHint.timer = setTimeout(() => {
      el.hint.hidden = true;
    }, 6000);
  }

  function setConnection(kind, label) {
    el.connDot.className = `dot ${kind}`;
    el.connLabel.textContent = label;
    state.connected = kind === "online";
  }

  // ---------- RPC ----------

  function send(message) {
    if (!state.socket || state.socket.readyState !== WebSocket.OPEN) {
      throw new Error("连接已关闭");
    }
    state.socket.send(JSON.stringify(message));
  }

  function rpc(method, params = {}) {
    const id = state.nextId++;
    return new Promise((resolve, reject) => {
      state.pending.set(id, { resolve, reject, method });
      try {
        send({ jsonrpc: "2.0", id, method, params });
      } catch (error) {
        state.pending.delete(id);
        reject(error);
      }
    });
  }

  function respond(id, result) {
    send({ jsonrpc: "2.0", id, result });
  }

  function respondError(id, code, message) {
    send({ jsonrpc: "2.0", id, error: { code, message } });
  }

  function connect() {
    if (state.reconnecting) return;
    state.reconnecting = true;
    setConnection("connecting", state.socket ? "重连中…" : "正在连接…");

    const socket = new WebSocket(WS_URL);
    state.socket = socket;

    socket.onopen = async () => {
      state.reconnecting = false;
      state.reconnectDelay = 600;
      setConnection("online", "已连接");
      try {
        await rpc("initialize", {
          protocol_version: "1.3",
          client: { name: "moc-web", version: "0.1.0", frontend: "webui" },
          capabilities: { permission_requests: true, ask_requests: true },
        });
        await refreshSessions();
        const remembered = sessionStorage.getItem(STORAGE_KEY);
        if (remembered) {
          loadSession(remembered).catch(() => {});
        }
      } catch (error) {
        showHint(`握手失败：${error.message}`, "error");
      }
    };

    socket.onmessage = (event) => {
      let message;
      try {
        message = JSON.parse(event.data);
      } catch {
        return;
      }
      handleMessage(message);
    };

    socket.onclose = () => {
      state.reconnecting = false;
      state.pending.forEach((pending) => pending.reject(new Error("连接已关闭")));
      state.pending.clear();
      setConnection("offline", "已断开，重连中…");
      state.reconnectDelay = Math.min(state.reconnectDelay * 1.7, 8000);
      setTimeout(connect, state.reconnectDelay);
    };

    socket.onerror = () => {
      socket.close();
    };
  }

  function handleMessage(message) {
    if (message.id !== undefined && message.id !== null) {
      if (message.method) {
        // 服务端 → 前端的反向请求。
        handleServerRequest(message);
        return;
      }
      const pending = state.pending.get(message.id);
      if (!pending) return;
      state.pending.delete(message.id);
      if (message.error) {
        const error = new Error(message.error.message || "请求失败");
        error.code = message.error.code;
        error.data = message.error.data;
        pending.reject(error);
      } else {
        pending.resolve(message.result);
      }
      return;
    }

    if (message.method === "session/update") {
      handleUpdate(message.params);
    }
  }

  // ---------- 反向请求：权限与提问 ----------

  function handleServerRequest(message) {
    if (message.method === "session/request_permission") {
      openPermissionModal(message);
    } else if (message.method === "session/request_ask") {
      openAskModal(message);
    } else {
      respondError(message.id, -32601, `未实现的方法：${message.method}`);
    }
  }

  function openPermissionModal(message) {
    const params = message.params;
    const options = params.options || [];
    const buttons = options
      .map((option) => {
        const tone =
          option.kind === "allow_once" || option.kind === "allow_session"
            ? "allow"
            : option.kind === "reject_once"
              ? "reject"
              : "";
        return `<button class="option-button ${tone}" data-option="${escapeHtml(option.id)}">${escapeHtml(option.label)}</button>`;
      })
      .join("");

    const modal = createModal(`
      <h2>${escapeHtml(params.title || "需要授权")}</h2>
      ${params.body ? `<pre class="modal-body">${escapeHtml(params.body)}</pre>` : ""}
      <div class="modal-actions">${buttons}</div>
    `);

    const close = (optionId) => {
      modal.remove();
      respond(message.id, { option_id: optionId });
    };

    modal.querySelectorAll("[data-option]").forEach((button) => {
      button.addEventListener("click", () => close(button.dataset.option));
    });
    modal.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        const reject = options.find((option) => option.kind === "reject_once") || options[0];
        close(reject ? reject.id : "");
      }
    });
    modal.querySelector("button")?.focus();
  }

  function openAskModal(message) {
    const params = message.params || {};
    const questions = params.questions || [];
    const body = questions
      .map((question, index) => {
        const inputType = question.multi ? "checkbox" : "radio";
        const options = (question.options || [])
          .map((option, optionIndex) => {
            const recommended = (question.recommended || []).includes(optionIndex);
            return `
              <label class="ask-option">
                <input type="${inputType}" name="q${index}" value="${escapeHtml(option.label)}">
                <span>
                  ${escapeHtml(option.label)}${recommended ? "（推荐）" : ""}
                  ${option.description ? `<span class="desc">${escapeHtml(option.description)}</span>` : ""}
                </span>
              </label>`;
          })
          .join("");
        return `
          <div class="question" data-question="${escapeHtml(question.id)}">
            ${question.header ? `<div class="q-header">${escapeHtml(question.header)}</div>` : ""}
            <h3>${escapeHtml(question.question)}</h3>
            <div class="ask-options">${options}</div>
            <input class="custom-answer" type="text" placeholder="或输入自定义答案（可留空）">
          </div>`;
      })
      .join("");

    const modal = createModal(`
      <h2>需要你的回答</h2>
      <div class="modal-body" style="background:transparent;border:0;padding:0">${body}</div>
      <div class="modal-actions">
        <button class="option-button reject" id="ask-cancel">取消</button>
        <button class="option-button allow" id="ask-submit">提交</button>
      </div>
    `);

    modal.querySelectorAll(".ask-option").forEach((option) => {
      option.addEventListener("click", (event) => {
        if (event.target.tagName === "INPUT") return;
        const input = option.querySelector("input");
        if (input.type === "radio") {
          modal.querySelectorAll(`input[name="${input.name}"]`).forEach((item) => {
            item.checked = false;
          });
          input.checked = true;
        } else {
          input.checked = !input.checked;
        }
        modal.querySelectorAll(".ask-option").forEach((item) => {
          item.classList.toggle("selected", item.querySelector("input").checked);
        });
      });
    });

    const submit = () => {
      const answers = questions.map((question, index) => {
        const container = modal.querySelector(`[data-question="${CSS.escape(question.id)}"]`);
        const selected = Array.from(container.querySelectorAll(`input[name="q${index}"]:checked`)).map(
          (input) => input.value,
        );
        const custom = container.querySelector(".custom-answer").value.trim();
        const answer = { id: question.id, selected };
        if (custom) answer.custom = custom;
        return answer;
      });
      modal.remove();
      respond(message.id, { answers });
    };

    modal.querySelector("#ask-submit").addEventListener("click", submit);
    modal.querySelector("#ask-cancel").addEventListener("click", () => {
      modal.remove();
      respond(message.id, {
        answers: questions.map((question) => ({ id: question.id, selected: [] })),
      });
    });
  }

  function createModal(html) {
    const backdrop = document.createElement("div");
    backdrop.className = "modal-backdrop";
    backdrop.tabIndex = -1;
    const modal = document.createElement("div");
    modal.className = "modal";
    modal.tabIndex = 0;
    modal.innerHTML = html;
    backdrop.appendChild(modal);
    el.modalRoot.appendChild(backdrop);
    backdrop.focus();
    backdrop.addEventListener("keydown", (event) => {
      if (event.key === "Escape") backdrop.remove();
    });
    return modal;
  }

  // ---------- 会话列表 ----------

  async function refreshSessions() {
    const result = await rpc("session/list", {});
    state.sessions = result.sessions || [];
    renderSessions();
  }

  function renderSessions() {
    // 新建会话在第一条消息落盘前不会出现在 session/list 里；把它补进列表，
    // 否则用户刚建完会话会在侧栏看不到自己正在用的那个。
    const all = [...state.sessions];
    const active = state.active;
    if (active && !all.some((session) => session.id === active.id)) {
      all.unshift({ ...active, updated_at_ms: active.updated_at_ms || Date.now() });
    }
    const query = state.query.trim().toLowerCase();
    const filtered = query
      ? all.filter(
          (session) =>
            session.title.toLowerCase().includes(query) ||
            session.cwd.toLowerCase().includes(query),
        )
      : all;

    const groups = new Map();
    for (const session of filtered) {
      if (!groups.has(session.cwd)) groups.set(session.cwd, []);
      groups.get(session.cwd).push(session);
    }

    el.sessions.innerHTML = "";
    if (groups.size === 0) {
      const empty = document.createElement("div");
      empty.className = "group-label";
      empty.textContent = query ? "没有匹配的会话" : "还没有会话";
      el.sessions.appendChild(empty);
      return;
    }

    for (const [cwd, sessions] of groups) {
      const label = document.createElement("div");
      label.className = "group-label";
      label.textContent = shortPath(cwd);
      label.title = cwd;
      el.sessions.appendChild(label);

      for (const session of sessions) {
        const button = document.createElement("button");
        button.className = `session-item${state.active?.id === session.id ? " active" : ""}`;
        button.dataset.session = session.id;

        const title = document.createElement("span");
        title.className = "session-title";
        title.textContent = session.title || session.id;

        const sub = document.createElement("span");
        sub.className = "session-sub";
        sub.innerHTML = `<span>${escapeHtml(relativeTime(session.updated_at_ms))}</span>`;
        if (session.parent_session_id) {
          const mark = document.createElement("span");
          mark.className = "badge";
          mark.textContent = "子代理";
          sub.appendChild(mark);
        }
        if (session.lock) {
          const lock = document.createElement("span");
          lock.className = "lock-mark";
          lock.textContent = `● ${session.lock.frontend}`;
          lock.title = `pid ${session.lock.pid}${session.lock.process_name ? ` · ${session.lock.process_name}` : ""}`;
          sub.appendChild(lock);
        }

        button.appendChild(title);
        button.appendChild(sub);
        button.addEventListener("click", () => loadSession(session.id));
        el.sessions.appendChild(button);
      }
    }
  }

  async function createSession() {
    const cwd = state.active?.cwd || null;
    try {
      const info = await rpc("session/new", {
        cwd,
        model: null,
        title: null,
        mode: "normal",
      });
      await refreshSessions();
      await loadSession(info.id);
    } catch (error) {
      showHint(`新建会话失败：${error.message}`, "error");
    }
  }

  /** 释放写租约：unload 后本进程不再持有该会话，另一个 moc 进程可以接管。 */
  async function releaseSession() {
    if (!state.active) return;
    const sessionId = state.active.id;
    try {
      await rpc("session/unload", { session_id: sessionId });
    } catch (error) {
      showHint(`释放会话失败：${error.message}`, "error");
      return;
    }
    sessionStorage.removeItem(STORAGE_KEY);
    state.active = null;
    state.readOnly = false;
    state.readOnlyHolder = null;
    state.promptRunning = false;
    state.streaming.clear();
    state.toolBlocks.clear();
    el.transcript.innerHTML = `
      <div class="empty-state">
        <h2>会话已释放</h2>
        <p>写租约已交还；从左侧重新打开会重新获取。</p>
      </div>`;
    renderHeader();
    updateComposer();
    refreshSessions().catch(() => {});
    showHint("会话已释放");
  }

  async function loadSession(sessionId) {
    let loaded;
    let readOnly = false;
    try {
      loaded = await rpc("session/load", { session_id: sessionId, read_only: false });
    } catch (error) {
      if (error.code !== -32015) {
        showHint(`加载会话失败：${error.message}`, "error");
        throw error;
      }
      readOnly = true;
      loaded = await rpc("session/load", { session_id: sessionId, read_only: true });
    }

    state.active = loaded.session;
    state.readOnly = readOnly;
    state.readOnlyHolder = loaded.session.lock || null;
    state.promptRunning = false;
    state.streaming.clear();
    state.toolBlocks.clear();
    state.pluginNotice = null;
    sessionStorage.setItem(STORAGE_KEY, sessionId);

    renderTranscript(loaded.entries || []);
    renderHeader();
    renderSessions();
    updateComposer();
    el.transcript.scrollTop = el.transcript.scrollHeight;
    state.autoScroll = true;

    if (readOnly) {
      showHint(
        `另一个进程${state.readOnlyHolder ? `（${state.readOnlyHolder.frontend}，pid ${state.readOnlyHolder.pid}）` : ""}正在写入，当前为只读`,
      );
    }
    refreshPluginState();
  }

  /** 载入会话后同步插件停用态：刷新页面时也能看到顶栏「重载插件」。 */
  async function refreshPluginState() {
    if (!state.active) return;
    try {
      const result = await rpc("plugin/list", { session_id: state.active.id });
      const failed = (result.plugins || []).find((plugin) => plugin.error);
      state.pluginNotice = failed
        ? { plugin: failed.name, code: "E_PLUGIN_DISABLED", message: failed.error }
        : null;
      renderHeader();
    } catch {
      // 清单读不到只影响这一个提示按钮，不打断对话。
    }
  }

  function renderHeader() {
    const session = state.active;
    if (!session) {
      el.title.textContent = "未选择会话";
      el.meta.textContent = "";
      el.badges.innerHTML = "";
      el.modelLabel.textContent = "";
      el.release.hidden = true;
      return;
    }
    el.release.hidden = false;
    el.title.textContent = session.title || session.id;
    el.meta.textContent = `${shortPath(session.cwd)} · ${session.id}`;
    el.meta.title = session.cwd;
    el.modelLabel.textContent = session.model;

    const badges = [];
    if (state.readOnly) {
      badges.push(
        `<span class="badge readonly" title="另一个进程持有写租约">只读${state.readOnlyHolder ? ` · ${escapeHtml(state.readOnlyHolder.frontend)}` : ""}</span>`,
      );
    }
    badges.push(`<span class="badge mode">${escapeHtml(modeLabel(session.mode))}</span>`);
    badges.push(`<span class="badge">${escapeHtml(permissionLabel(session.permission))}</span>`);
    if (session.parent_session_id) badges.push(`<span class="badge warn">子代理</span>`);
    if (state.pluginNotice) {
      const notice = state.pluginNotice;
      const title = notice.message
        ? `插件 ${notice.plugin} 不可用：${notice.message}`
        : `插件 ${notice.plugin} 已停用（${notice.code}）`;
      badges.push(
        `<button id="reload-plugins" class="badge warn reload" title="${escapeHtml(title)}">重载插件</button>`,
      );
    }
    el.badges.innerHTML = badges.join("");
  }

  function modeLabel(mode) {
    return (
      { normal: "普通", plan: "计划", ptc: "PTC", clm_experimental: "CLM 实验" }[mode] || mode
    );
  }

  function permissionLabel(permission) {
    return permission === "workspace" ? "工作区沙盒" : permission === "full" ? "完全权限" : permission;
  }

  function updateComposer() {
    const disabled = !state.active || state.readOnly;
    el.input.disabled = disabled;
    el.input.placeholder = state.readOnly
      ? "只读会话：另一个进程正在写入"
      : "输入消息，Enter 发送，Shift+Enter 换行";
    el.send.disabled = disabled || !el.input.value.trim() || state.promptRunning;
    el.stop.hidden = !state.promptRunning;
    el.input.style.height = "auto";
    el.input.style.height = `${Math.min(el.input.scrollHeight, 220)}px`;
  }

  // ---------- 渲染 ----------

  function renderTranscript(entries) {
    el.transcript.innerHTML = "";
    for (const entry of entries) {
      el.transcript.appendChild(renderEntry(entry));
    }
    scrollToBottom(true);
  }

  function renderEntry(entry) {
    const message = entry.message || {};
    const role = message.role;

    if (entry.bash) {
      const element = document.createElement("div");
      element.className = "message assistant";
      element.innerHTML = `
        <div class="message-head"><span class="message-role">bash</span><span>${escapeHtml(entry.bash.command || "")}</span></div>
        <div class="message-body"></div>`;
      element.querySelector(".message-body").textContent = entry.message?.content?.[0]?.Text || "";
      return element;
    }

    const element = document.createElement("div");
    element.className = `message ${role === "User" ? "user" : "assistant"}`;
    element.dataset.entryId = entry.id;

    const head = document.createElement("div");
    head.className = "message-head";
    const name = role === "User" ? "你" : role === "Assistant" ? "MOC" : role === "Tool" ? "工具" : "系统";
    head.innerHTML = `<span class="message-role">${name}</span><span>${relativeTime(entry.at_ms || Date.now())}</span>`;
    element.appendChild(head);

    if (role === "Tool") {
      element.classList.add("tool-result");
    }

    const content = document.createElement("div");
    content.className = "message-body";
    element.appendChild(content);
    renderContentParts(content, message);

    return element;
  }

  function renderContentParts(container, message) {
    const parts = message.content || [];
    for (const part of parts) {
      if (part.Text !== undefined) {
        const block = document.createElement("div");
        block.innerHTML = renderMarkdown(part.Text);
        container.appendChild(block);
      } else if (part.Reasoning !== undefined) {
        const text = part.Reasoning?.Text?.text || part.Reasoning?.Redacted?.data || "";
        if (!text) continue;
        const details = document.createElement("details");
        details.className = "reasoning";
        details.innerHTML = `<summary>思维链</summary><div class="reasoning-body"></div>`;
        details.querySelector(".reasoning-body").textContent = text;
        container.appendChild(details);
      } else if (part.ToolCall !== undefined) {
        container.appendChild(createToolBlock(part.ToolCall));
      } else if (part.ToolResult !== undefined) {
        const block = state.toolBlocks.get(part.ToolResult.tool_call_id);
        if (block) {
          applyToolResult(block, part.ToolResult);
        } else {
          container.appendChild(createLooseToolResult(part.ToolResult));
        }
      } else if (part.Image !== undefined) {
        const source = part.Image.source;
        if (source && source.Base64) {
          const image = document.createElement("img");
          image.className = "image-part";
          image.src = `data:${source.Base64.media_type};base64,${source.Base64.data}`;
          container.appendChild(image);
        } else if (source && source.Url) {
          const link = document.createElement("a");
          link.href = source.Url;
          link.textContent = source.Url;
          link.target = "_blank";
          link.rel = "noreferrer";
          container.appendChild(link);
        }
      }
    }
  }

  function createToolBlock(call) {
    const details = document.createElement("details");
    details.className = "tool";
    details.dataset.tool = call.id;
    details.innerHTML = `
      <summary class="tool-head">
        <span class="tool-name"></span>
        <span class="tool-status">执行中</span>
      </summary>
      <pre class="tool-body" hidden></pre>`;
    details.querySelector(".tool-name").textContent = call.name;
    const body = details.querySelector(".tool-body");
    body.textContent = prettyArguments(call.arguments);
    state.toolBlocks.set(call.id, details);
    return details;
  }

  function createLooseToolResult(result) {
    const details = document.createElement("details");
    details.className = "tool";
    details.innerHTML = `
      <summary class="tool-head">
        <span class="tool-name">工具结果</span>
        <span class="tool-status ${result.is_error ? "error" : "completed"}">${result.is_error ? "失败" : "完成"}</span>
      </summary>
      <pre class="tool-body"></pre>`;
    details.querySelector(".tool-body").textContent = result.content || "";
    return details;
  }

  function applyToolResult(block, result) {
    const status = block.querySelector(".tool-status");
    status.textContent = result.is_error ? "失败" : "完成";
    status.className = `tool-status ${result.is_error ? "error" : "completed"}`;
    const body = block.querySelector(".tool-body");
    body.hidden = false;
    body.textContent = result.content || "";
  }

  function prettyArguments(raw) {
    if (!raw) return "";
    try {
      return JSON.stringify(JSON.parse(raw), null, 2);
    } catch {
      return raw;
    }
  }

  function appendStatusLine(text, tone = "") {
    const line = document.createElement("div");
    line.className = `status-line ${tone}`;
    line.textContent = text;
    el.transcript.appendChild(line);
    scrollToBottom();
    return line;
  }

  function scrollToBottom(force = false) {
    if (!force && !state.autoScroll) return;
    el.transcript.scrollTop = el.transcript.scrollHeight;
  }

  // ---------- session/update ----------

  function handleUpdate(params) {
    if (!params || !state.active || params.session_id !== state.active.id) return;
    const update = params.update || {};
    switch (update.type) {
      case "prompt_started":
        beginPrompt(update);
        break;
      case "assistant_delta":
        appendDelta(update);
        break;
      case "tool_started":
        startTool(update);
        break;
      case "tool_finished":
        finishTool(update);
        break;
      case "prompt_finished":
        finishPrompt(update);
        break;
      case "error":
        appendStatusLine(update.message || "提示内错误", "error");
        break;
      case "guard_notice":
        appendStatusLine(update.message, update.terminal ? "error" : "warn");
        break;
      case "plugin_notice":
        state.pluginNotice = { plugin: update.plugin, code: update.code };
        appendStatusLine(
          `插件 ${update.plugin} 已停用（${update.code}）：${update.message}\n修复后点顶栏「重载插件」或重启 moc 可恢复`,
          "warn",
        );
        renderHeader();
        break;
      case "lock_changed":
        // 锁投影只用于展示持有者；只读与否在 session/load 时已经裁决。
        state.readOnlyHolder = update.lock || null;
        renderHeader();
        break;
      case "title_changed":
        state.active.title = update.title;
        renderHeader();
        refreshSessions().catch(() => {});
        break;
      case "mode_changed":
        state.active.mode = update.mode;
        renderHeader();
        break;
      case "model_changed":
        state.active.model = update.model;
        renderHeader();
        break;
      case "permission_changed":
        state.active.permission = update.permission;
        renderHeader();
        break;
      case "context_compaction_started":
        state.compacting = appendStatusLine("正在压缩上下文…");
        break;
      case "context_compacted":
        if (state.compacting) state.compacting.remove();
        appendStatusLine(`上下文已压缩：${update.tokens_before} → ${update.tokens_after} tokens`, "");
        break;
      case "context_compaction_failed":
        if (state.compacting) state.compacting.remove();
        appendStatusLine(`上下文压缩失败：${update.message}`, "error");
        break;
      case "agent_started":
        appendStatusLine(`子代理已启动：${update.agent?.title || update.agent?.id || ""}`);
        break;
      case "steering_applied":
        break;
      default:
        break;
    }
  }

  function beginPrompt(update) {
    state.promptRunning = true;
    updateComposer();

    const element = document.createElement("div");
    element.className = "message assistant";
    element.dataset.prompt = update.prompt_id;
    element.innerHTML = `
      <div class="message-head"><span class="message-role">MOC</span><span class="stream-state">生成中…</span></div>
      <div class="reasoning-slot"></div>
      <div class="message-body"></div>`;
    el.transcript.appendChild(element);
    state.streaming.set(update.prompt_id, {
      element,
      slot: element.querySelector(".reasoning-slot"),
      body: element.querySelector(".message-body"),
      text: "",
      reasoning: "",
    });
    scrollToBottom();
  }

  function appendDelta(update) {
    const stream = state.streaming.get(update.prompt_id);
    if (!stream || !update.delta) return;
    if (update.delta.type === "text") {
      stream.text += update.delta.delta;
      stream.body.innerHTML = renderMarkdown(stream.text);
    } else if (update.delta.type === "reasoning") {
      stream.reasoning += update.delta.delta;
      let details = stream.slot.querySelector(".reasoning");
      if (!details) {
        details = document.createElement("details");
        details.className = "reasoning";
        details.innerHTML = `<summary>思维链</summary><div class="reasoning-body"></div>`;
        stream.slot.appendChild(details);
      }
      details.querySelector(".reasoning-body").textContent = stream.reasoning;
    }
    scrollToBottom();
  }

  function startTool(update) {
    const stream = state.streaming.get(update.prompt_id);
    const target = stream ? stream.element : el.transcript;
    const block = createToolBlock({
      id: update.tool_call_id,
      name: update.name,
      arguments: update.arguments,
    });
    target.appendChild(block);
    scrollToBottom();
  }

  function finishTool(update) {
    let block = state.toolBlocks.get(update.tool_call_id);
    if (!block) {
      block = createToolBlock({
        id: update.tool_call_id,
        name: update.name,
        arguments: update.arguments,
      });
      el.transcript.appendChild(block);
    }
    const status = block.querySelector(".tool-status");
    status.textContent = toolStatusLabel(update.status);
    status.className = `tool-status ${update.status || ""}`;
    const body = block.querySelector(".tool-body");
    body.hidden = false;
    body.textContent = update.result?.content || "";
    scrollToBottom();
  }

  function toolStatusLabel(status) {
    return (
      { completed: "完成", error: "失败", denied: "拒绝", cancelled: "取消" }[status] || status || ""
    );
  }

  function finishPrompt(update) {
    const stream = state.streaming.get(update.prompt_id);
    if (stream) {
      // 流式阶段的思维链放在独立 slot；收尾时整块换成权威 message，避免重复。
      stream.slot.innerHTML = "";
      stream.body.innerHTML = "";
      renderContentParts(stream.body, update.message || { content: [] });
      const stateNode = stream.element.querySelector(".stream-state");
      if (stateNode) stateNode.textContent = finishLabel(update.finish_reason);
      state.streaming.delete(update.prompt_id);
    }
    state.promptRunning = false;
    updateComposer();
    refreshSessions().catch(() => {});
    scrollToBottom();
  }

  function finishLabel(reason) {
    return (
      {
        stop: "完成",
        length: "截断",
        cancelled: "已取消",
        loop_detected: "循环中止",
        error: "失败",
      }[reason] || ""
    );
  }

  // ---------- 输入 ----------

  async function submit() {
    const text = el.input.value.trim();
    if (!text || !state.active || state.readOnly) return;
    el.input.value = "";
    updateComposer();

    const element = document.createElement("div");
    element.className = "message user";
    element.innerHTML = `<div class="message-head"><span class="message-role">你</span><span>刚刚</span></div><div class="message-body"></div>`;
    element.querySelector(".message-body").innerHTML = renderMarkdown(text);
    el.transcript.appendChild(element);
    scrollToBottom(true);

    try {
      await rpc("session/prompt", {
        session_id: state.active.id,
        input: [{ type: "text", text }],
      });
    } catch (error) {
      if (error.code === -32015) {
        appendStatusLine("另一个进程持有写租约，提示没有发送", "error");
      } else {
        appendStatusLine(`提示失败：${error.message}`, "error");
      }
      state.promptRunning = false;
      updateComposer();
    }
  }

  async function cancel() {
    if (!state.active) return;
    try {
      await rpc("session/cancel", { session_id: state.active.id });
    } catch (error) {
      showHint(`取消失败：${error.message}`, "error");
    }
  }

  // ---------- 插件 ----------

  /** 顶栏「重载插件」：丢弃插件宿主后重新加载；没有插件仍不可用时收起按钮。 */
  async function reloadPlugins() {
    if (!state.active) return;
    try {
      const result = await rpc("plugin/reload", { session_id: state.active.id });
      const plugins = result.plugins || [];
      const disabled = plugins.filter((plugin) => plugin.error).length;
      appendStatusLine(
        `插件已重载：${plugins.length - disabled} 个可用 / ${disabled} 个仍不可用`,
        disabled > 0 ? "warn" : "",
      );
      if (disabled === 0) {
        state.pluginNotice = null;
        renderHeader();
      }
    } catch (error) {
      appendStatusLine(`重载插件失败：${error.message}`, "error");
    }
  }

  // ---------- 事件绑定 ----------

  el.search.addEventListener("input", () => {
    state.query = el.search.value;
    renderSessions();
  });

  el.newSession.addEventListener("click", createSession);
  el.release.addEventListener("click", releaseSession);

  el.input.addEventListener("input", updateComposer);
  el.input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      submit();
    }
  });
  el.send.addEventListener("click", submit);
  el.stop.addEventListener("click", cancel);

  el.transcript.addEventListener("scroll", () => {
    const distance = el.transcript.scrollHeight - el.transcript.scrollTop - el.transcript.clientHeight;
    state.autoScroll = distance < 80;
  });

  // 徽章区随 renderHeader 重建，点击用委托接住「重载插件」。
  el.badges.addEventListener("click", (event) => {
    if (event.target.closest("#reload-plugins")) reloadPlugins();
  });

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && el.modalRoot.childElementCount > 0) {
      el.modalRoot.lastElementChild.remove();
    }
  });

  connect();
})();
