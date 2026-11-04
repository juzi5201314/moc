#!/usr/bin/env python3
"""真实压缩实验：固定语料、隔离运行、HTTP 录制与严格离线重放。"""
from __future__ import annotations
import argparse
import copy
import hashlib
import http.server
import json
from pathlib import Path
import shutil
import subprocess
import threading
import time
import tomllib
import urllib.error
import urllib.request

REPO = Path(__file__).resolve().parents[1]
BINARY = REPO / "target/debug/examples/compaction_lab"


def dump(path: Path, value) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def toml_text(value: dict, prefix: tuple[str, ...] = ()) -> str:
    """编码实验配置使用的 TOML 标量、数组与表。"""
    lines = ["[" + ".".join(json.dumps(p) for p in prefix) + "]"] if prefix else []
    for key, item in value.items():
        if not isinstance(item, dict):
            lines.append(f"{json.dumps(key)} = {json.dumps(item, ensure_ascii=False)}")
    for key, item in value.items():
        if isinstance(item, dict):
            lines.extend(["", toml_text(item, (*prefix, key))])
    return "\n".join(lines) + "\n"


def canonical(body: bytes, roots: list[str]) -> object:
    """只归一化实验根路径与非语义缓存键；消息正文必须匹配。"""
    value = json.loads(body)
    value.pop("prompt_cache_key", None)
    def visit(item):
        if isinstance(item, str):
            for root in roots:
                item = item.replace(root, "<EXPERIMENT_ROOT>")
            return item
        if isinstance(item, list):
            return [visit(v) for v in item]
        if isinstance(item, dict):
            return {k: visit(v) for k, v in item.items()}
        return item
    return visit(value)


def assemble_sse(raw: bytes) -> bytes:
    """将单候选 OpenAI SSE 合并为普通响应，正文、推理、工具参数和 usage 均来自原流。"""
    response = {}
    message = {"role": "assistant", "content": ""}
    calls = {}
    finish = None
    done = False
    for line in raw.decode("utf-8").splitlines():
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if data == "[DONE]":
            done = True
            continue
        chunk = json.loads(data)
        if "error" in chunk:
            raise ValueError(f"上游流内错误：{chunk['error']}")
        for key in ("id", "created", "model", "system_fingerprint", "usage"):
            if chunk.get(key) is not None:
                response[key] = chunk[key]
        for choice in chunk.get("choices", []):
            if choice.get("index", 0) != 0:
                raise ValueError("实验只接受一个候选响应")
            delta = choice.get("delta", {})
            for key in ("content", "reasoning", "reasoning_content", "refusal"):
                if delta.get(key):
                    message[key] = message.get(key, "") + delta[key]
            for call in delta.get("tool_calls", []):
                accumulated = calls.setdefault(call["index"], {"type": "function", "function": {"name": "", "arguments": ""}})
                if call.get("id"):
                    accumulated["id"] = call["id"]
                for key in ("name", "arguments"):
                    accumulated["function"][key] += call.get("function", {}).get(key, "")
            if choice.get("finish_reason") is not None:
                finish = choice["finish_reason"]
    if not done or finish is None:
        raise ValueError("上游流未完整结束，不能把半截摘要作为有效压缩结果")
    if calls:
        message["tool_calls"] = [calls[index] for index in sorted(calls)]
    response.update({"object": "chat.completion", "choices": [{"index": 0, "message": message, "finish_reason": finish}]})
    return json.dumps(response, ensure_ascii=False).encode()


class Gateway(http.server.ThreadingHTTPServer):
    daemon_threads = True
    def __init__(self, directory: Path, upstream: dict | None, replay: Path | None = None,
                 roots: list[str] | None = None, stream_bridge: bool = False):
        super().__init__(("127.0.0.1", 0), Handler)
        self.stream_bridge = stream_bridge
        self.directory = directory
        directory.mkdir(parents=True, exist_ok=True)
        self.upstream, self.replay = upstream, replay
        self.roots = roots or []
        self.count = 0
        self.failures: list[str] = []
        self.lock = threading.Lock()
        self.expected = sorted(replay.glob("[0-9][0-9][0-9][0-9].json")) if replay else []

    def event(self, event: dict) -> None:
        with self.lock:
            with (self.directory / "events.jsonl").open("a", encoding="utf-8") as stream:
                stream.write(json.dumps(event, ensure_ascii=False) + "\n")
        print(json.dumps(event, ensure_ascii=False), flush=True)


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        gateway: Gateway = self.server
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        with gateway.lock:
            index = gateway.count
            gateway.count += 1
        stem = f"{index + 1:04d}"
        request_path = gateway.directory / f"{stem}.request.json"
        request_path.write_bytes(body)
        started = time.monotonic()
        gateway.event({"event": "http_start", "attempt": index + 1, "bytes": len(body),
                       "stream": json.loads(body).get("stream", False), "replay": bool(gateway.replay)})
        try:
            if gateway.replay:
                if index >= len(gateway.expected):
                    raise ValueError("离线重放出现未录制的额外请求")
                meta = json.loads(gateway.expected[index].read_text())
                recorded = (gateway.replay / meta["request"]).read_bytes()
                if hashlib.sha256(recorded).hexdigest() != meta["request_sha256"]:
                    raise ValueError("请求录制文件 SHA256 不匹配")
                if self.path != meta["path"] or canonical(body, gateway.roots) != canonical(recorded, gateway.roots):
                    raise ValueError(f"第 {index + 1} 次请求与录制内容不匹配，已保存实际请求供比较")
                response = (gateway.replay / meta["response"]).read_bytes()
                if hashlib.sha256(response).hexdigest() != meta["response_sha256"]:
                    raise ValueError("响应录制文件 SHA256 不匹配")
                if meta.get("stream_bridge") and meta["status"] == 200:
                    upstream_files = {}
                    for name in ("upstream-request.json", "upstream-response.sse"):
                        artifact = meta[name]
                        content = (gateway.replay / artifact["file"]).read_bytes()
                        if hashlib.sha256(content).hexdigest() != artifact["sha256"]:
                            raise ValueError(f"上游录制哈希不匹配：{name}")
                        upstream_files[name] = content
                    expected_upstream = json.loads(recorded)
                    expected_upstream["stream"] = True
                    expected_upstream["stream_options"] = {"include_usage": True}
                    if json.loads(upstream_files["upstream-request.json"]) != expected_upstream:
                        raise ValueError("上游桥接请求不只是传输方式变化")
                    if assemble_sse(upstream_files["upstream-response.sse"]) != response:
                        raise ValueError("上游原始流重新聚合后与录制响应不一致")
                status, headers = meta["status"], meta["headers"]
            else:
                upstream = gateway.upstream
                headers = {"Content-Type": self.headers.get("Content-Type", "application/json"),
                           "Accept": self.headers.get("Accept", "application/json")}
                headers.update(upstream.get("headers", {}))
                key = upstream.get("api_key", "")
                if key:
                    headers["Authorization"] = f"Bearer {key}"
                outgoing = body
                bridge = gateway.stream_bridge and not json.loads(body).get("stream", False)
                if bridge:
                    streamed = json.loads(body)
                    streamed["stream"] = True
                    streamed["stream_options"] = {"include_usage": True}
                    outgoing = json.dumps(streamed, ensure_ascii=False).encode()
                    (gateway.directory / f"{stem}.upstream-request.json").write_bytes(outgoing)
                suffix = self.path.removeprefix("/v1") if self.path.startswith("/v1/") else self.path
                request = urllib.request.Request(upstream["base_url"].rstrip("/") + suffix,
                                                 outgoing, headers, method="POST")
                try:
                    reply = urllib.request.urlopen(request, timeout=540)
                except urllib.error.HTTPError as error:
                    reply = error
                with reply:
                    status = reply.status
                    # 仅白名单响应头落盘，鉴权不进入产物。
                    headers = {name: value for name, value in reply.headers.items()
                               if name.lower() in {"content-type", "retry-after", "x-request-id"}}
                    response = reply.read()
                if bridge and status == 200:
                    (gateway.directory / f"{stem}.upstream-response.sse").write_bytes(response)
                    response = assemble_sse(response)
                    headers = {"Content-Type": "application/json"}
            response_name = f"{stem}.response.bin"
            (gateway.directory / response_name).write_bytes(response)
            meta = {"path": self.path, "request": request_path.name, "response": response_name,
                    "request_sha256": hashlib.sha256(body).hexdigest(),
                    "response_sha256": hashlib.sha256(response).hexdigest(),
                    "status": status, "headers": headers, "elapsed_seconds": time.monotonic() - started,
                    "replay": bool(gateway.replay),
                    "stream_bridge": gateway.stream_bridge and not json.loads(body).get("stream", False)}
            if gateway.stream_bridge and not gateway.replay:
                for suffix in ("upstream-request.json", "upstream-response.sse"):
                    path = gateway.directory / f"{stem}.{suffix}"
                    if path.exists():
                        meta[suffix] = {"file": path.name, "sha256": digest(path)}
            dump(gateway.directory / f"{stem}.json", meta)
            self.send_response(status)
            for key, value in headers.items():
                self.send_header(key, value)
            self.send_header("Content-Length", str(len(response)))
            self.end_headers()
            self.wfile.write(response)
            gateway.event({"event": "http_end", "attempt": index + 1, "status": status,
                           "elapsed_seconds": meta["elapsed_seconds"], "response_bytes": len(response)})
        except Exception as error:
            message = f"{type(error).__name__}: {error}"
            gateway.failures.append(message)
            dump(gateway.directory / f"{stem}.error.json", {"error": message})
            gateway.event({"event": "http_error", "attempt": index + 1, "error": message})
            self.send_error(400, "Compaction laboratory gateway failed")


def provider_config(config: Path, selector: str) -> tuple[str, str, dict]:
    provider_id, model_selector = selector.split("/", 1)
    model_id = model_selector.rsplit(":", 1)[0]
    document = tomllib.loads(config.read_text(encoding="utf-8"))
    provider = copy.deepcopy(document["providers"][provider_id])
    if provider.get("api") != "openai_chat":
        raise ValueError("当前实验网关仅支持 openai_chat 供应商")
    provider["models"] = {model_id: provider["models"][model_id]}
    return provider_id, model_id, provider


def isolated_config(provider_id: str, model_id: str, provider: dict, selector: str,
                    gateway: Gateway, target: int, window: int) -> dict:
    selected = copy.deepcopy(provider)
    selected["base_url"] = f"http://127.0.0.1:{gateway.server_port}"
    selected["api_key"] = "lab-local"
    selected["headers"] = {}
    selected["timeout_secs"] = 560
    selected["models"][model_id]["context_window"] = window
    return {"models": {"roles": {"default": selector}}, "providers": {provider_id: selected},
            "mcp": {"enabled": False}, "skills": {"enabled": False},
            "context_files": {"enabled": False}, "web_search": {"enabled": False},
            "compaction": {"method": "moc", "auto": False, "auto_continue": False,
                           "threshold": {"mode": "tokens", "value": target}}}


def execute_group(root: Path, target: int, rounds: int, selector: str, window: int,
                  provider_id: str, model_id: str, provider: dict, replay: Path | None = None,
                  stream_bridge: bool = False, binary: Path = BINARY):
    group = root / f"{target // 1000}k"
    group.mkdir()
    upstream = {k: v for k, v in provider.items() if k != "models"}
    old_group = replay / group.name if replay else None
    gateway = Gateway(group / "http", None if replay else upstream,
                      old_group / "http" if old_group else None,
                      [str(root), str(replay)] if replay else [str(root)], stream_bridge)
    worker = threading.Thread(target=gateway.serve_forever, daemon=True)
    worker.start()
    home, work, sessions = group / "home", group / "work", group / "sessions"
    home.mkdir(); work.mkdir(); sessions.mkdir()
    if old_group:
        # 工作区只有实验空目录；保持模型看到的 cwd 及 token 预算完全一致。
        work = Path(json.loads((old_group / "spec.json").read_text())["cwd"])
    configuration = isolated_config(provider_id, model_id, provider, selector, gateway, target, window)
    (home / "config.toml").write_text(toml_text(configuration), encoding="utf-8")
    spec = {"home": str(home), "cwd": str(work), "session_dir": str(sessions),
            "corpus": str(root / "corpus.jsonl"), "output": str(group / "run"),
            "model": selector, "target_tokens": target, "rounds": rounds,
            "session_id": f"ses_compaction_lab_{target}"}
    dump(group / "spec.json", spec)
    try:
        with (group / "driver.log").open("w", encoding="utf-8") as log:
            process = subprocess.Popen([str(binary), str(group / "spec.json")], cwd=REPO,
                                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            for line in process.stdout:
                log.write(line); log.flush()
                print(line, end="", flush=True)
            code = process.wait()
        original = json.loads((old_group / "run/report.json").read_text()) if old_group else None
        expected_failure = original is not None and original["status"] == "failed"
        if code and not expected_failure:
            raise RuntimeError(f"{target // 1000}k 驱动退出码 {code}，见 {group / 'driver.log'}")
        if gateway.failures:
            raise RuntimeError(f"HTTP 记录/重放失败：{gateway.failures}")
        if replay and gateway.count != len(gateway.expected):
            raise RuntimeError(f"重放只消费 {gateway.count}/{len(gateway.expected)} 个请求")
        if old_group:
            actual = json.loads((group / "run/report.json").read_text())
            fields = ("cursor_start", "cursor_end", "tokens_before", "tokens_after", "compact", "status", "error")
            expected_rounds = [{k: row.get(k) for k in fields} for row in original["rounds"]]
            actual_rounds = [{k: row.get(k) for k in fields} for row in actual["rounds"]]
            if expected_rounds != actual_rounds:
                raise RuntimeError("离线重放的轮次边界或压缩结果与录制实验不一致")
            dump(group / "replay-verification.json", {"matched": True, "compared_fields": fields,
                                                     "rounds": len(actual_rounds)})
        dump(group / "transport-report.json", {"requests": gateway.count, "replay": bool(replay),
                                               "all_recordings_consumed": not replay or gateway.count == len(gateway.expected)})
    finally:
        gateway.shutdown(); gateway.server_close(); worker.join()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path)
    parser.add_argument("--config", type=Path, default=Path.home() / ".moc/config.toml")
    parser.add_argument("--bin", type=Path, default=BINARY)
    parser.add_argument("--model", default="cpa-chat/deepseek-v4.1-flash:high")
    parser.add_argument("--window", type=int, default=1_000_000)
    parser.add_argument("--targets", default="400000,600000")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--stream-bridge", action="store_true", help="显式将非流式摘要请求转成上游SSE并录制转换过程")
    parser.add_argument("--replay", type=Path, help="原实验目录；离线重放不读取供应商凭据")
    args = parser.parse_args()
    binary = args.bin.expanduser().resolve()
    root = args.output.expanduser().resolve()
    root.mkdir(parents=True, exist_ok=False)
    root.chmod(0o700)
    targets = [int(value) for value in args.targets.split(",")]
    if args.rounds < 1 or any(value < 1 or value >= args.window for value in targets):
        raise ValueError("轮次须为正，目标须小于模型窗口")
    if args.replay:
        previous = args.replay.expanduser().resolve()
        execution = json.loads((previous / "execution.json").read_text())
        if digest(binary) != execution["binary_sha256"]:
            raise ValueError("离线重放须使用录制时的同一实验二进制")
        manifest = json.loads((previous / "manifest.json").read_text())
        args.model, args.window, args.rounds = manifest["model"], manifest["window"], manifest["rounds"]
        targets = manifest["targets"]
        args.stream_bridge = manifest.get("stream_bridge", False)
        if digest(previous / "corpus.jsonl") != manifest["corpus_sha256"]:
            raise ValueError("原实验语料哈希不匹配")
        shutil.copy2(previous / "corpus.jsonl", root / "corpus.jsonl")
        provider_id, model_id, provider = manifest["provider_id"], manifest["model_id"], manifest["provider_public"]
    else:
        from compaction_lab_corpus import prepare
        if not args.source:
            parser.error("录制需要 --source")
        source = args.source.expanduser().resolve()
        shutil.copy2(source, root / "source.jsonl")
        metadata = prepare(root / "source.jsonl", root / "corpus.jsonl", artifact_dir=source.with_suffix(""))
        dump(root / "corpus-manifest.json", metadata)
        provider_id, model_id, provider = provider_config(args.config.expanduser(), args.model)
        public = copy.deepcopy(provider)
        public["api_key"] = "<runtime-only>"
        public["headers"] = {}
        manifest = {"model": args.model, "window": args.window, "targets": targets, "rounds": args.rounds,
                    "stream_bridge": args.stream_bridge,
                    "source": str(source), "source_sha256": digest(root / "source.jsonl"),
                    "corpus_sha256": digest(root / "corpus.jsonl"),
                    "provider_id": provider_id, "model_id": model_id, "provider_public": public,
                    "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=REPO, text=True).strip(),
                    "normalization": ["实验根路径", "prompt_cache_key"],
                    "notes": ["阈值按实验目标覆盖，method=moc、instructions=None；其余压缩参数沿用默认值。",
                              "目标是 moc 本地 token 估算，供应商 usage 单独记录。",
                              "SSE响应完整缓冲后转交，不将传输耗时解释为首 token 延迟。"]}
        previous = None
    implementation = {}
    for relative in ("scripts/compaction_lab.py", "scripts/compaction_lab_corpus.py",
                     "crates/moc-core/examples/compaction_lab.rs"):
        path = REPO / relative
        if path.exists():
            destination = root / "implementation" / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, destination)
            implementation[relative] = digest(path)
    dump(root / "implementation.json", implementation)
    dump(root / "manifest.json", manifest)
    if args.prepare_only:
        print(f"语料已准备：{root}", flush=True)
        return
    if not binary.exists():
        raise FileNotFoundError("先 cargo build -p moc-core --example compaction_lab")
    shutil.copy2(binary, root / "implementation/compaction_lab")
    dump(root / "execution.json", {"binary_sha256": digest(binary), "replay_source": str(previous) if previous else None})
    absent = []
    for target in targets:
        if previous and not (previous / f"{target // 1000}k/run/report.json").exists():
            absent.append(target)
            continue
        execute_group(root, target, args.rounds, args.model, args.window,
                      provider_id, model_id, provider, previous, args.stream_bridge, binary)
    if previous:
        dump(root / "replay-report.json", {"matched_recorded_groups": True, "unrecorded_targets": absent})
    print(f"实验完成：{root}", flush=True)


if __name__ == "__main__":
    main()
