#!/usr/bin/env python3
"""将 OMP 有效分支转为带来源证明的 moc 消息组；不访问模型或配置。"""

from __future__ import annotations

import argparse
from collections import Counter, OrderedDict
import hashlib
import json
from pathlib import Path
import re

SHAKE = re.compile(r"\[shaken ~([\d,]+) tokens — recover: artifact://(\d+) \(region (\d+)\)\]")
REGION = re.compile(r"^### region (\d+) \(([^\n]*), ~([\d,]+) tok\)\n\n", re.MULTILINE)
ROLES = {"user": "User", "assistant": "Assistant", "developer": "Developer"}
ELIDED = {"[Uneventful result elided]", "[Superseded by a newer read of this file]"}


def estimate_tokens(text: str) -> int:
    """与 moc estimate_tokens 一致：ASCII/4 向下取整，非 ASCII 每字符一个。"""
    ascii_count = len(text.encode("ascii", "ignore"))
    return ascii_count // 4 + len(text) - ascii_count


def message_tokens(message: dict) -> int:
    total = 8
    for part in message["content"]:
        if "Text" in part:
            total += estimate_tokens(part["Text"])
        elif "ToolCall" in part:
            call = part["ToolCall"]
            total += estimate_tokens(_json(call))
        elif "ToolResult" in part:
            total += estimate_tokens(part["ToolResult"]["content"])
    return total


def _json(value: object) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), allow_nan=False)


class Artifacts:
    def __init__(self, directory: Path, output_dir: Path, expected_headers: dict):
        self.directory = directory
        self.output_dir = output_dir
        self.expected_headers = expected_headers
        self.inputs: dict[str, dict] = {}
        # 来源按时间顺序读取；只缓存最近两份存档，避免长语料把全部原文常驻内存。
        self.cache: OrderedDict[str, dict[int, str]] = OrderedDict()
        self.failures: Counter = Counter()

    def restore(self, value: str, line: int, entry_id: str) -> tuple[str, dict | None]:
        match = SHAKE.fullmatch(value)
        if not match:
            return value, None
        _, artifact_id, region_id = match.groups()
        name = f"{artifact_id}.shake.log"
        path = self.directory / name
        if name not in self.cache:
            if not path.is_file():
                self.failures["missing_artifact"] += 1
                return value, None
            raw = path.read_bytes()
            digest = hashlib.sha256(raw).hexdigest()
            target = self.output_dir / "source-artifacts" / name
            target.parent.mkdir(exist_ok=True)
            if target.exists():
                if hashlib.sha256(target.read_bytes()).hexdigest() != digest:
                    raise ValueError(f"存档快照内容冲突：{target}")
            else:
                target.write_bytes(raw)
            text = raw.decode("utf-8")
            candidates = list(REGION.finditer(text))
            headers = []
            ambiguous = False
            for header in candidates:
                number = int(header.group(1))
                signature = self.expected_headers.get((name, number))
                if signature and signature != (header.group(2), header.group(3)):
                    continue
                if number == len(headers) + 1:
                    headers.append(header)
                elif number == len(headers):
                    ambiguous = True
            regions = {}
            if ambiguous:
                self.failures["ambiguous_region_boundaries"] += 1
                headers = []
            for index, header in enumerate(headers):
                number = int(header.group(1))
                end = headers[index + 1].start() if index + 1 < len(headers) else len(text)
                # OMP 存档用空行分隔区域，末尾追加换行；只归一化这一层的尾部换行。
                regions[number] = text[header.end():end].rstrip("\n")
            self.cache[name] = regions
            if len(self.cache) > 2:
                self.cache.popitem(last=False)
            self.inputs.setdefault(name, {
                "original_path": str(path.resolve()), "snapshot": f"source-artifacts/{name}",
                "sha256": digest, "bytes": len(raw), "references": [],
            })
        self.cache.move_to_end(name)
        recovered = self.cache[name].get(int(region_id))
        if recovered is None:
            self.failures["missing_region"] += 1
            return value, None
        reference = {
            "artifact": name, "region": int(region_id), "line": line, "entry_id": entry_id,
            "content_sha256": hashlib.sha256(recovered.encode()).hexdigest(),
        }
        self.inputs[name]["references"].append(reference)
        return recovered, reference


def prepare(source: Path, output: Path, *, artifact_dir: Path | None = None) -> dict:
    """写 JSONL 消息组与相邻 manifest.json；output 必须是尚不存在的文件。"""
    source, output = Path(source).resolve(), Path(output).resolve()
    manifest_path = output.parent / "manifest.json"
    if output.exists() or manifest_path.exists():
        raise FileExistsError("输出或 manifest 已存在，请使用新的实验目录")
    before = source.stat()
    index = {}
    entry_types: Counter = Counter()
    children: Counter = Counter()
    digest = hashlib.sha256()
    offset = 0
    tip = None
    session = None
    expected_headers = {}
    line_count = 0
    with source.open("rb") as stream:
        for line_count, raw in enumerate(stream, 1):
            digest.update(raw)
            if not raw.strip():
                offset += len(raw)
                continue
            entry = json.loads(raw)
            kind = entry.get("type", "unknown")
            entry_types[kind] += 1
            if kind == "message":
                message = entry["message"]
                if message.get("role") == "toolResult":
                    for part in message.get("content", []):
                        match = SHAKE.fullmatch(part.get("text", "")) if isinstance(part, dict) else None
                        if match:
                            tokens, artifact_id, region_id = match.groups()
                            tool = message.get("toolName")
                            if tool:
                                expected_headers[(f"{artifact_id}.shake.log", int(region_id))] = (tool, tokens)
            if kind == "session":
                session = {key: entry.get(key) for key in ("id", "version", "cwd", "timestamp")}
            if "parentId" in entry:
                entry_id = entry["id"]
                if entry_id in index:
                    raise ValueError(f"重复 entry id：{entry_id}，行 {line_count}")
                index[entry_id] = (entry["parentId"], line_count, offset, len(raw), kind)
                children[entry["parentId"]] += 1
                tip = entry_id
            offset += len(raw)
    if tip is None:
        raise ValueError("未找到 OMP parentId 链")
    selected = []
    cursor = tip
    while cursor is not None:
        if cursor not in index:
            raise ValueError(f"有效分支缺失父条目：{cursor}")
        info = index[cursor]
        selected.append((cursor, info))
        parent = info[0]
        if parent in index and index[parent][1] >= info[1]:
            raise ValueError(f"父条目不在当前条目之前：{cursor} -> {parent}")
        cursor = parent
    selected.reverse()
    output.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    artifacts = Artifacts(
        Path(artifact_dir) if artifact_dir is not None else source.with_suffix(""),
        output.parent, expected_headers,
    )
    dropped_entries: Counter = Counter()
    dropped_blocks: Counter = Counter()
    dropped_fields: Counter = Counter()
    roles: Counter = Counter()
    counts: Counter = Counter()
    issues = []
    seen_calls = set()
    seen_results = set()
    pending = []
    expected = set()
    output_digest = hashlib.sha256()

    def issue(reason: str, records: list, **details) -> None:
        counts[reason] += 1
        counts["rejected_messages"] += len(records)
        issues.append({"reason": reason, "lines": [row[0] for row in records],
                       "entry_ids": [row[1] for row in records], **details})

    def convert(entry: dict, line: int) -> tuple[dict | None, list, int]:
        message = entry["message"]
        role = message.get("role", "unknown")
        roles[role] += 1
        dropped_fields.update(key for key in message if key not in {"role", "content", "toolCallId", "isError"})
        if message.get("prunedAt"):
            counts["source_pruned_messages"] += 1
        if role not in ROLES and role != "toolResult":
            counts[f"dropped_role:{role}"] += 1
            return None, [], 0
        content = message.get("content", [])
        if isinstance(content, str):
            content = [{"type": "text", "text": content}]
        parts, refs, placeholder_tokens = [], [], 0
        for part in content:
            kind = part.get("type", "unknown") if isinstance(part, dict) else "non_object"
            if kind == "text":
                text = part["text"]
                if role == "toolResult":
                    original = text
                    text, reference = artifacts.restore(text, line, entry["id"])
                    if reference:
                        refs.append(reference)
                        counts["restored_regions"] += 1
                    elif SHAKE.fullmatch(original) or original in ELIDED:
                        counts["unrestored_placeholders"] += 1
                        placeholder_tokens += estimate_tokens(text)
                parts.append({"Text": text})
                dropped_fields.update(f"block.text.{key}" for key in part if key not in {"type", "text"})
            elif kind == "toolCall" and role == "assistant":
                arguments = part["arguments"]
                arguments = arguments if isinstance(arguments, str) else _json(arguments)
                json.loads(arguments)
                parts.append({"ToolCall": {"id": part["id"], "name": part["name"], "arguments": arguments}})
                dropped_fields.update(f"block.toolCall.{key}" for key in part if key not in {"type", "id", "name", "arguments"})
            else:
                dropped_blocks[f"{role}:{kind}"] += 1
        if role == "toolResult":
            return {"role": "Tool", "content": [{"ToolResult": {
                "tool_call_id": message["toolCallId"],
                "content": "".join(part["Text"] for part in parts),
                "is_error": bool(message.get("isError", False)),
            }}]}, refs, placeholder_tokens
        if not parts:
            counts["empty_after_filter"] += 1
            return None, refs, placeholder_tokens
        return {"role": ROLES[role], "content": parts}, refs, placeholder_tokens

    with source.open("rb") as stream, output.open("xb") as target:
        def emit(records: list) -> None:
            messages = [record[3] for record in records]
            source_info = {
                "line_start": records[0][0], "line_end": records[-1][0],
                "entry_ids": [record[1] for record in records],
                "sha256": hashlib.sha256(b"".join(record[2] for record in records)).hexdigest(),
            }
            references = [ref for record in records for ref in record[4]]
            if references:
                source_info["artifacts"] = references
            raw = (_json({"source": source_info, "messages": messages}) + "\n").encode()
            target.write(raw)
            output_digest.update(raw)
            counts["groups"] += 1
            counts["messages"] += len(messages)
            counts["estimated_tokens"] += sum(message_tokens(message) for message in messages)
            counts["placeholder_tokens"] += sum(record[5] for record in records)
            counts["message_overhead_tokens"] += 8 * len(messages)

        for entry_id, (_, line, position, length, kind) in selected:
            if kind != "message":
                dropped_entries[kind] += 1
                continue
            stream.seek(position)
            raw = stream.read(length)
            entry = json.loads(raw)
            message = entry["message"]
            role = message.get("role")
            if role == "toolResult":
                call_id = message.get("toolCallId")
                if call_id in seen_results:
                    issue("duplicate_tool_result", [(line, entry_id)], tool_call_id=call_id)
                    continue
                if call_id not in expected:
                    issue("orphan_tool_result", [(line, entry_id)], tool_call_id=call_id)
                    continue
            elif pending:
                issue("incomplete_tool_group", pending, missing_tool_call_ids=sorted(expected))
                pending, expected = [], set()
            converted, refs, placeholder_tokens = convert(entry, line)
            if converted is None:
                continue
            row = (line, entry_id, raw, converted, refs, placeholder_tokens)
            if role == "toolResult":
                pending.append(row)
                seen_results.add(call_id)
                expected.remove(call_id)
                if not expected:
                    emit(pending)
                    pending = []
                continue
            calls = [part["ToolCall"]["id"] for part in converted["content"] if "ToolCall" in part]
            if len(set(calls)) != len(calls) or any(call in seen_calls for call in calls):
                issue("duplicate_tool_call", [row], tool_call_ids=calls)
                continue
            seen_calls.update(calls)
            if calls:
                pending, expected = [row], set(calls)
            else:
                emit([row])
        if pending:
            issue("incomplete_tool_group", pending, missing_tool_call_ids=sorted(expected))
    after = source.stat()
    if (before.st_size, before.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
        raise ValueError("转换期间源文件变化，必须先冻结源文件")
    metadata = {
        "version": 1,
        "source": {"path": str(source), "sha256": digest.hexdigest(), "bytes": offset,
                   "lines": line_count, "session": session},
        "output": {"path": str(output), "sha256": output_digest.hexdigest()},
        "branch": {"tip_entry_id": tip, "selected_entries": len(selected),
                   "excluded_entries": len(index) - len(selected),
                   "fork_points": sum(count > 1 for parent, count in children.items() if parent is not None),
                   "selection": "last_parentId_entry_ancestry"},
        "counts": dict(counts),
        "estimated_tokens": counts["estimated_tokens"],
        "real_content_tokens": counts["estimated_tokens"] - counts["placeholder_tokens"] - counts["message_overhead_tokens"],
        "placeholder_tokens": counts["placeholder_tokens"],
        "source_entry_types": dict(entry_types), "selected_message_roles": dict(roles),
        "dropped_entry_types": dict(dropped_entries), "dropped_block_types": dict(dropped_blocks),
        "dropped_metadata_fields": dict(dropped_fields), "rejections": issues,
        "artifacts": list(artifacts.inputs.values()), "artifact_failures": dict(artifacts.failures),
        "limitations": [
            "仅复原最后有效叶的祖先消息；未采用原 compaction 摘要或已放弃分支。",
            "源文件消息若已被物理删除且父链缺失则拒绝导入；父链完整不能证明源内容从未被修改。",
            "恢复仅识别 OMP shaken artifact://N (region R)；按存档区域去除尾部换行，无法恢复原始尾部空白。",
            "思考、图片、非消息 custom 记录和不支持的消息角色不进入语料，数量已记录。",
            "无法恢复的原占位文本保留用于审计，placeholder_tokens 不计入真实正文体量。",
            "token 是 moc 本地估算，尚未包含本次实验系统提示与工具定义，也不是供应商计费用量。",
        ],
        "group_hash": "sha256(concatenated_original_message_lines_in_emitted_order)",
    }
    manifest_path.write_text(json.dumps(metadata, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return metadata


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--artifact-dir", type=Path)
    args = parser.parse_args()
    result = prepare(args.source, args.output, artifact_dir=args.artifact_dir)
    print(json.dumps({key: result[key] for key in ("source", "branch", "counts", "estimated_tokens", "real_content_tokens")}, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
