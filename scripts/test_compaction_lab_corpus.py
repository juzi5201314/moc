"""真实语料转换器的离线正确性测试。"""

import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from compaction_lab_corpus import estimate_tokens, prepare


def message(entry_id, parent, role, content, **extra):
    return {"type": "message", "id": entry_id, "parentId": parent,
            "message": {"role": role, "content": content, **extra}}


def text(value):
    return {"type": "text", "text": value}


def call(call_id):
    return {"type": "toolCall", "id": call_id, "name": "read", "arguments": {"path": "中文.rs"}}


class CorpusTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "session.jsonl"
        self.output = self.root / "out" / "corpus.jsonl"

    def source_rows(self, rows):
        raw = b"".join((json.dumps(row, ensure_ascii=False) + "\n").encode() for row in rows)
        self.source.write_bytes(raw)
        return raw

    def groups(self):
        return [json.loads(line) for line in self.output.read_text().splitlines()]

    def test_parallel_calls_stay_together_and_ignore_compaction(self):
        rows = [
            message("u", None, "user", [text("用户约束")]),
            message("a", "u", "assistant", [{"type": "thinking", "thinking": "私有思考"}, call("c1"), call("c2")]),
            {"type": "custom", "id": "meta", "parentId": "a", "data": "重复工具字段"},
            message("r2", "meta", "toolResult", [text("第二个结果")], toolCallId="c2"),
            message("r1", "r2", "toolResult", [text("第一个结果")], toolCallId="c1", isError=True),
            {"type": "compaction", "id": "compact", "parentId": "r1", "summary": "不可注入的旧摘要"},
            message("final", "compact", "assistant", [text("继续任务")]),
        ]
        raw = self.source_rows(rows)
        report = prepare(self.source, self.output)
        groups = self.groups()
        self.assertEqual([len(group["messages"]) for group in groups], [1, 3, 1])
        self.assertEqual(groups[1]["source"]["entry_ids"], ["a", "r2", "r1"])
        self.assertEqual(groups[1]["source"]["line_start"], 2)
        self.assertEqual(groups[1]["source"]["line_end"], 5)
        selected_raw = b"".join(raw.splitlines(keepends=True)[index] for index in [1, 3, 4])
        self.assertEqual(groups[1]["source"]["sha256"], hashlib.sha256(selected_raw).hexdigest())
        self.assertEqual(report["source"]["sha256"], hashlib.sha256(raw).hexdigest())
        self.assertEqual(report["dropped_block_types"], {"assistant:thinking": 1})
        self.assertNotIn("旧摘要", self.output.read_text())
        self.assertTrue(groups[1]["messages"][2]["content"][0]["ToolResult"]["is_error"])
        args = groups[1]["messages"][0]["content"][0]["ToolCall"]["arguments"]
        self.assertEqual(json.loads(args), {"path": "中文.rs"})

    def test_last_leaf_selects_only_its_ancestors(self):
        self.source_rows([
            message("u", None, "user", [text("共同起点")]),
            message("abandoned", "u", "assistant", [text("被放弃的未来")]),
            message("old-end", "abandoned", "user", [text("旧分支")]),
            message("chosen", "u", "assistant", [text("当前分支")]),
        ])
        report = prepare(self.source, self.output)
        self.assertEqual(report["branch"]["excluded_entries"], 2)
        self.assertEqual([group["source"]["entry_ids"] for group in self.groups()], [["u"], ["chosen"]])
        self.assertNotIn("被放弃", self.output.read_text())

    def test_missing_parent_and_duplicate_entry_are_rejected(self):
        for rows in [
            [message("a", "missing", "user", [text("x")])],
            [message("a", None, "user", [text("x")]), message("a", None, "user", [text("y")])],
            [message("a", "b", "user", [text("x")]), message("b", "a", "user", [text("y")])],
        ]:
            with self.subTest(rows=rows):
                self.source_rows(rows)
                with self.assertRaises(ValueError):
                    prepare(self.source, self.output)

    def test_unpaired_duplicate_and_orphan_results_are_accounted(self):
        self.source_rows([
            message("u", None, "user", [text("开始")]),
            message("a", "u", "assistant", [call("c1"), call("c2")]),
            message("r1", "a", "toolResult", [text("半组结果")], toolCallId="c1"),
            message("rd", "r1", "toolResult", [text("重复结果")], toolCallId="c1"),
            message("u2", "rd", "user", [text("中断")]),
            message("ro", "u2", "toolResult", [text("迟到结果")], toolCallId="c2"),
            message("ad", "ro", "assistant", [call("c1")]),
        ])
        report = prepare(self.source, self.output)
        self.assertEqual(report["counts"]["duplicate_tool_result"], 1)
        self.assertEqual(report["counts"]["incomplete_tool_group"], 1)
        self.assertEqual(report["counts"]["orphan_tool_result"], 1)
        self.assertEqual(report["counts"]["duplicate_tool_call"], 1)
        self.assertEqual([group["source"]["entry_ids"] for group in self.groups()], [["u"], ["u2"]])

    def test_artifacts_restore_and_replay_without_original_sidecar(self):
        placeholder = "[shaken ~10 tokens — recover: artifact://5 (region 2)]"
        self.source_rows([
            message("u", None, "user", [text("保留约束")]),
            message("a", "u", "assistant", [call("c")]),
            message("r", "a", "toolResult", [text(placeholder)], toolCallId="c", prunedAt=1),
        ])
        sidecar = self.source.with_suffix("")
        sidecar.mkdir()
        raw = "### region 1 (read, ~1 tok)\n\n其它区域\n\n### region 2 (read, ~10 tok)\n\n原始输出：界限为四。\n".encode()
        (sidecar / "5.shake.log").write_bytes(raw)
        report = prepare(self.source, self.output)
        self.assertEqual(report["counts"]["restored_regions"], 1)
        self.assertEqual(report["placeholder_tokens"], 0)
        group = self.groups()[1]
        self.assertEqual(group["messages"][1]["content"][0]["ToolResult"]["content"], "原始输出：界限为四。")
        self.assertEqual(report["artifacts"][0]["sha256"], hashlib.sha256(raw).hexdigest())
        self.assertEqual((self.output.parent / "source-artifacts" / "5.shake.log").read_bytes(), raw)
        replay = self.root / "replay" / "corpus.jsonl"
        prepare(self.source, replay, artifact_dir=self.output.parent / "source-artifacts")
        self.assertEqual(replay.read_bytes(), self.output.read_bytes())

    def test_nested_region_headers_remain_in_recovered_body(self):
        self.source_rows([
            message("a", None, "assistant", [call("c")]),
            message("r", "a", "toolResult", [text("[shaken ~20 tokens — recover: artifact://5 (region 2)]")],
                    toolCallId="c", toolName="read", prunedAt=1),
        ])
        sidecar = self.source.with_suffix("")
        sidecar.mkdir()
        body = "读取旧存档：\n### region 1 (bash, ~2 tok)\n\n旧输出\n\n### region 2 (bash, ~3 tok)\n\n嵌套结果"
        (sidecar / "5.shake.log").write_text(
            "### region 1 (read, ~1 tok)\n\n前置\n\n### region 2 (read, ~20 tok)\n\n"
            + body + "\n\n### region 3 (read, ~1 tok)\n\n后置\n"
        )
        report = prepare(self.source, self.output)
        self.assertEqual(report["counts"]["restored_regions"], 1)
        self.assertEqual(self.groups()[0]["messages"][1]["content"][0]["ToolResult"]["content"], body)

    def test_ambiguous_region_boundary_is_reported_without_guessing(self):
        placeholder = "[shaken ~20 tokens — recover: artifact://5 (region 1)]"
        self.source_rows([
            message("a", None, "assistant", [call("c")]),
            message("r", "a", "toolResult", [text(placeholder)],
                    toolCallId="c", toolName="read", prunedAt=1),
        ])
        sidecar = self.source.with_suffix("")
        sidecar.mkdir()
        (sidecar / "5.shake.log").write_text(
            "### region 1 (read, ~20 tok)\n\n外层\n\n"
            "### region 1 (read, ~20 tok)\n\n无法辨别的嵌套\n"
        )
        report = prepare(self.source, self.output)
        self.assertEqual(report["artifact_failures"], {"ambiguous_region_boundaries": 1, "missing_region": 1})
        self.assertEqual(report["placeholder_tokens"], estimate_tokens(placeholder))
        self.assertEqual(self.groups()[0]["messages"][1]["content"][0]["ToolResult"]["content"], placeholder)

    def test_missing_artifact_has_separate_placeholder_budget(self):
        placeholder = "[shaken ~20 tokens — recover: artifact://99 (region 1)]"
        self.source_rows([
            message("a", None, "assistant", [call("c")]),
            message("r", "a", "toolResult", [text(placeholder)], toolCallId="c", prunedAt=1),
        ])
        report = prepare(self.source, self.output)
        self.assertEqual(report["artifact_failures"], {"missing_artifact": 1})
        self.assertEqual(report["placeholder_tokens"], estimate_tokens(placeholder))
        self.assertEqual(report["counts"]["unrestored_placeholders"], 1)
        self.assertEqual(report["real_content_tokens"] + report["placeholder_tokens"] + 16, report["estimated_tokens"])

    def test_unicode_token_estimation(self):
        self.assertEqual(estimate_tokens("abcd中文🙂"), 4)
        self.assertEqual(estimate_tokens("abc"), 0)
        self.assertEqual(estimate_tokens("é"), 1)


if __name__ == "__main__":
    unittest.main()
