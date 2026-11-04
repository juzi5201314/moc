"""实验网关的关键约束：请求保真、离线拒绝漂移、鉴权不落盘。"""
import json
from pathlib import Path
import tempfile
import threading
import tomllib
import unittest
import urllib.error
import urllib.request

from compaction_lab import Gateway, assemble_sse, canonical, isolated_config, toml_text


class GatewayTests(unittest.TestCase):
    def test_stream_bridge_preserves_content_usage_and_completion(self):
        chunks = [
            {"id": "r", "model": "m", "choices": [{"index": 0, "delta": {"reasoning": "推理", "content": "答"}}]},
            {"choices": [{"index": 0, "delta": {"content": "案"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 463763}},
        ]
        raw = b"".join(("data: " + json.dumps(c) + "\n\n").encode() for c in chunks)
        with self.assertRaises(ValueError):
            assemble_sse(raw)
        result = json.loads(assemble_sse(raw + b"data: [DONE]\n\n"))
        self.assertEqual(result["choices"][0]["message"], {"role": "assistant", "content": "答案", "reasoning": "推理"})
        self.assertEqual(result["usage"]["prompt_tokens"], 463763)

    def test_record_replay_and_reject_changed_message(self):
        import http.server
        seen = []
        class Upstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                seen.append((self.path, self.headers.get("Authorization"), body))
                reply = b'{"choices":[{"message":{"content":"recorded answer"}}]}'
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("X-Secret-Header", "never-save")
                self.end_headers()
                self.wfile.write(reply)
        upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            recorded = Gateway(root / "record", {"base_url": f"http://127.0.0.1:{upstream.server_port}/v1", "api_key": "private-key"})
            body = json.dumps({"messages": [{"role": "user", "content": "remember 17"}]}).encode()
            expected = self.post(recorded, body)
            self.assertEqual(seen[0], ("/v1/chat/completions", "Bearer private-key", body))
            self.assertNotIn(b"private-key", b"".join(p.read_bytes() for p in (root / "record").iterdir()))
            meta = json.loads((root / "record/0001.json").read_text())
            self.assertNotIn("X-Secret-Header", meta["headers"])
            replay = Gateway(root / "replay", None, root / "record")
            self.assertEqual(self.post(replay, body), expected)
            mismatch = Gateway(root / "mismatch", None, root / "record")
            with self.assertRaises(urllib.error.HTTPError) as error:
                self.post(mismatch, body.replace(b"17", b"18"))
            error.exception.close()
            self.assertTrue(mismatch.failures)
            self.assertEqual(len(seen), 1, "离线重放绝不能向真实上游发请求")
        upstream.shutdown(); upstream.server_close(); thread.join()

    def post(self, gateway, body):
        thread = threading.Thread(target=gateway.serve_forever, daemon=True)
        thread.start()
        try:
            request = urllib.request.Request(f"http://127.0.0.1:{gateway.server_port}/v1/chat/completions", body,
                                             {"Content-Type": "application/json"})
            with urllib.request.urlopen(request, timeout=5) as reply:
                return reply.read()
        finally:
            gateway.shutdown(); gateway.server_close(); thread.join()

    def test_bridge_replay_checks_original_stream(self):
        import hashlib
        from compaction_lab import dump
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            recorded = root / "record"
            recorded.mkdir()
            body = b'{"messages":[{"role":"user","content":"fact"}]}'
            outgoing = json.loads(body)
            outgoing.update({"stream": True, "stream_options": {"include_usage": True}})
            raw = b'data: {"choices":[{"index":0,"delta":{"content":"fact"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'
            response = assemble_sse(raw)
            files = {"0001.request.json": body, "0001.response.bin": response,
                     "0001.upstream-request.json": json.dumps(outgoing).encode(), "0001.upstream-response.sse": raw}
            for name, content in files.items():
                (recorded / name).write_bytes(content)
            meta = {"path": "/v1/chat/completions", "request": "0001.request.json", "response": "0001.response.bin",
                    "request_sha256": hashlib.sha256(body).hexdigest(), "response_sha256": hashlib.sha256(response).hexdigest(),
                    "headers": {"Content-Type": "application/json"}, "status": 200, "stream_bridge": True}
            for suffix in ("upstream-request.json", "upstream-response.sse"):
                name = "0001." + suffix
                meta[suffix] = {"file": name, "sha256": hashlib.sha256(files[name]).hexdigest()}
            dump(recorded / "0001.json", meta)
            self.assertEqual(self.post(Gateway(root / "good", None, recorded), body), response)
            (recorded / "0001.upstream-response.sse").write_bytes(raw.replace(b"fact", b"fake"))
            rejected = Gateway(root / "bad", None, recorded)
            with self.assertRaises(urllib.error.HTTPError) as error:
                self.post(rejected, body)
            error.exception.close()
            self.assertIn("上游录制哈希不匹配", rejected.failures[0])

    def test_canonicalization_does_not_erase_facts(self):
        a = json.dumps({"messages": [{"content": "/old/work answer=17"}], "prompt_cache_key": "a"}).encode()
        b = json.dumps({"messages": [{"content": "/new/work answer=17"}], "prompt_cache_key": "b"}).encode()
        self.assertEqual(canonical(a, ["/old"]), canonical(b, ["/new"]))
        self.assertNotEqual(canonical(a, ["/old"]), canonical(b.replace(b"17", b"18"), ["/new"]))

    def test_toml_roundtrip_and_threshold(self):
        with tempfile.TemporaryDirectory() as temp:
            gateway = Gateway(Path(temp), None)
            try:
                provider = {"api": "openai_chat", "api_key": "secret", "base_url": "unused",
                            "models": {"deepseek": {"max_output_tokens": 384000, "input": ["text"],
                                                    "compat": {"extra_body": {"thinking": {"type": "enabled"}}}}}}
                config = isolated_config("cpa", "deepseek", provider, "cpa/deepseek:high", gateway, 600000, 1000000)
                self.assertEqual(tomllib.loads(toml_text(config)), config)
                self.assertEqual(config["compaction"]["threshold"], {"mode": "tokens", "value": 600000})
                self.assertNotIn("secret", toml_text(config))
                self.assertEqual(config["providers"]["cpa"]["models"]["deepseek"]["max_output_tokens"], 384000)
            finally:
                gateway.server_close()


if __name__ == "__main__":
    unittest.main()
