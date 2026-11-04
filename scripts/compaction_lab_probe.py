#!/usr/bin/env python3
"""在实验快照副本上提问，闭卷或只读历史回查，保留真实请求与回答。"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import threading

from compaction_lab import BINARY, REPO, Gateway, digest, dump, isolated_config, provider_config, toml_text


def probe(experiment: Path, question_file: Path, output: Path, config: Path, mode: str,
          stage: str, selected: list[str]) -> None:
    manifest = json.loads((experiment / "manifest.json").read_text())
    selector = manifest["model"]
    provider_id, model_id, provider = provider_config(config, selector)
    # 探针短答无需申请模型完整输出预算，压缩主实验的配置不受影响。
    provider["models"][model_id]["max_output_tokens"] = 8192
    questions = json.loads(question_file.read_text())
    question = questions["prompt"]
    output.mkdir(parents=True, exist_ok=False)
    output.chmod(0o700)
    shutil.copy2(question_file, output / "questions.json")
    dump(output / "manifest.json", {"experiment": str(experiment), "mode": mode, "stage": stage,
                                   "question_sha256": digest(question_file), "model": selector,
                                   "binary_sha256": digest(BINARY), "tool_policy": [] if mode == "direct" else ["read", "grep", "find"]})
    results = []
    for key in selected:
        group_name, round_name = key.split("/")
        group = experiment / group_name
        snapshot = group / "run" / round_name
        original = json.loads((snapshot / "result.json").read_text())
        original_spec = json.loads((group / "spec.json").read_text())
        destination = output / group_name / round_name
        destination.mkdir(parents=True)
        sessions, home = destination / "sessions", destination / "home"
        home.mkdir()
        original_sessions = Path(original_spec["session_dir"])
        transcript = sessions / Path(original["transcript"]).relative_to(original_sessions)
        sidecar = sessions / Path(original["sidecar"]).relative_to(original_sessions)
        transcript.parent.mkdir(parents=True)
        with (snapshot / f"{stage}-session.jsonl").open() as source, transcript.open("w") as target:
            header = json.loads(next(source))
            header["permission"] = "workspace_only"
            target.write(json.dumps(header, ensure_ascii=False) + "\n")
            shutil.copyfileobj(source, target)
        if (snapshot / f"{stage}-sidecar").exists():
            shutil.copytree(snapshot / f"{stage}-sidecar", sidecar)
        gateway = Gateway(destination / "http", {k: v for k, v in provider.items() if k != "models"})
        worker = threading.Thread(target=gateway.serve_forever, daemon=True)
        worker.start()
        configuration = isolated_config(provider_id, model_id, provider, selector, gateway,
                                        original["target_tokens"], manifest["window"])
        readable = [str(transcript)]
        if (sidecar / "compaction").exists():
            readable.append(str(sidecar / "compaction"))
        configuration["sandbox"] = {"default_mode": "workspace_only", "on_denied": "deny",
                                    "network": False, "extra_read_paths": readable}
        (home / "config.toml").write_text(toml_text(configuration))
        prompt = question
        if mode == "retrieval":
            prompt += "\n允许使用提供的只读工具回查 history://compaction/INDEX.md 和历史片段，请为找回的事实指出出处。不要搜索真实项目目录，也不要执行历史任务。"
        spec = {"home": str(home), "cwd": original_spec["cwd"], "session_dir": str(sessions),
                "output": str(destination / "run"), "model": selector,
                "session_id": original["session_id"], "operation": "probe",
                "probe_mode": mode, "probe": prompt}
        dump(destination / "spec.json", spec)
        try:
            print(json.dumps({"event": "probe_start", "snapshot": key, "mode": mode, "stage": stage}), flush=True)
            with (destination / "driver.log").open("w") as log:
                result = subprocess.run([str(BINARY), str(destination / "spec.json")], cwd=REPO,
                                        stdout=log, stderr=subprocess.STDOUT, timeout=590)
            row = {"snapshot": key, "exit_code": result.returncode, "requests": gateway.count,
                   "gateway_errors": gateway.failures}
            results.append(row)
            dump(output / "report.json", results)
            print(json.dumps(row, ensure_ascii=False), flush=True)
            if result.returncode or gateway.failures:
                raise RuntimeError(f"探针失败，见 {destination}")
        finally:
            gateway.shutdown(); gateway.server_close(); worker.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("experiment", type=Path)
    parser.add_argument("questions", type=Path, help="JSON文档含prompt；其他证据字段不发送给模型")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--config", type=Path, default=Path.home() / ".moc/config.toml")
    parser.add_argument("--mode", choices=["direct", "retrieval"], default="direct")
    parser.add_argument("--stage", choices=["pre", "post"], default="post")
    parser.add_argument("--select", default="400k/round-01,400k/round-02,400k/round-03,600k/round-01,600k/round-02,600k/round-03")
    args = parser.parse_args()
    probe(args.experiment.resolve(), args.questions.resolve(), args.output.resolve(), args.config.expanduser(),
          args.mode, args.stage, args.select.split(","))


if __name__ == "__main__":
    main()
