"""moc 的 Harbor InstalledAgent 适配器。

把预编译的 Linux `moc` 二进制上传进任务环境，以 `--print` 单次提示模式跑任务；
模型经生成的 moc provider 配置（`openai_chat` 协议）接入任意 OpenAI 兼容端点。
API key 默认读 `MOC_API_KEY` 环境变量，便于切换不同第三方供应商。

通过 `-a moc_agent:MocAgent` 加载；`--ak key=value` 传参：

    binary            宿主机上预编译 moc 二进制路径（默认 target/release/moc）
    moc_base_url      provider 的 OpenAI 兼容 base_url（必填，或设 MOC_BASE_URL）
    api_key_env       容器内 moc 读取 key 的环境变量名（默认 MOC_API_KEY）；
                      宿主机同名环境变量的值会被注入任务环境
    model             上游真实模型 id；缺省用 `-m` 去掉 LiteLLM provider 前缀
    thinking          追加到 selector 的思考等级（如 high；默认不带）
    context_window    模型 context_window（默认 262144）
    max_output_tokens 模型 max_output_tokens（默认 32768）
    reasoning         模型 reasoning = true/false（默认 false）
    moc_args          追加给 moc 命令行的额外参数
    workdir           moc 进程工作目录；缺省用环境默认 cwd（任务工作区）
    config            Harbor 通用 native config：一个 moc TOML 文件路径，
                      作为额外 --config 覆盖层传入（需在 options_model 之外声明，
                      由 capabilities.native_config 开启）

示例：
    PYTHONPATH=eval/frontierharness harbor run \
      -d frontier-harness-eval/frontier-harness-eval@v1.1.0 \
      --agent-import-path moc_agent:MocAgent \
      -m fireworks_ai/accounts/fireworks/models/kimi-k3 \
      --ak moc_base_url=https://api.fireworks.ai/inference/v1 \
      -e runta -l 1 --jobs-dir ./jobs
"""

import json
import os
import shlex
from pathlib import Path, PurePosixPath
from typing import override

from pydantic import Field

from harbor.agents.capabilities import AgentCapabilities
from harbor.agents.installed.base import BaseInstalledAgent
from harbor.agents.options import InstalledAgentOptions
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext, ModelUsage


class MocOptions(InstalledAgentOptions):
    binary: str | None = Field(
        default=None,
        description="宿主机上预编译的 Linux moc 二进制路径（默认 target/release/moc）。",
    )
    moc_base_url: str | None = Field(
        default=None,
        description="provider 的 OpenAI 兼容 base_url；缺省读 MOC_BASE_URL。",
    )
    api_key_env: str = Field(
        default="MOC_API_KEY",
        description="容器内 moc provider 读取 key 的环境变量名。",
    )
    api_key: str | None = Field(
        default=None,
        description="明文 API key；缺省读宿主机上 api_key_env 同名环境变量。",
    )
    api_format: str = Field(
        default="openai_chat",
        description="provider 协议：openai_chat / openai_responses / anthropic_messages。",
    )
    model: str | None = Field(
        default=None,
        description="上游模型 id；缺省取 -m 去掉 provider 前缀。",
    )
    thinking: str | None = Field(
        default=None,
        description="selector 附加的思考等级（eval/<model>:<level>）。",
    )
    context_window: int = Field(default=262_144)
    max_output_tokens: int = Field(default=32_768)
    reasoning: bool = Field(default=False)
    moc_args: str | None = Field(
        default=None, description="追加给 moc 命令行的额外参数。"
    )
    workdir: str | None = Field(
        default=None, description="moc 进程 cwd；缺省为环境默认目录。"
    )


class MocAgent(BaseInstalledAgent):
    """在任务环境内以 `moc --print` 运行 moc 的 installed agent。"""

    capabilities = AgentCapabilities(native_config=True)
    options_model = MocOptions

    ENV_DIR = PurePosixPath("/installed-agent/moc")
    CONFIG_PATH = ENV_DIR / "config.toml"
    EXTRA_CONFIG_PATH = ENV_DIR / "extra.toml"
    INSTRUCTION_PATH = ENV_DIR / "instruction.txt"
    SESSION_DIR = ENV_DIR / "sessions"

    @staticmethod
    @override
    def name() -> str:
        return "moc"

    @override
    def get_version_command(self) -> str | None:
        return "moc --version"

    def _opts(self) -> MocOptions:
        """`self.options` 在未传任何 --ak 时可能仍是默认实例或 None。"""
        if isinstance(self.options, MocOptions):
            return self.options
        return MocOptions()

    def _base_url(self) -> str:
        url = self._opts().moc_base_url or os.environ.get("MOC_BASE_URL")
        if not url:
            raise ValueError(
                "缺少 provider base_url：用 --ak moc_base_url=... 或导出 MOC_BASE_URL"
            )
        return url

    def _model_id(self) -> str:
        options = self._opts()
        if options.model:
            return options.model
        if self.skip_model_parsing and self.model_name:
            return self.model_name
        if self._parsed_model_name:
            return self._parsed_model_name
        raise ValueError(
            "缺少模型 id：传 -m <provider>/<model> 或 --ak model=<上游模型id>"
        )

    def _selector(self) -> str:
        options = self._opts()
        selector = f"eval/{self._model_id()}"
        if options.thinking:
            selector += f":{options.thinking}"
        return selector

    def _render_config(self) -> str:
        """生成环境内的 moc provider 配置（TOML）。

        provider 固定为 `eval`，api_key 恒走环境变量引用，密钥不落地。
        """
        options = self._opts()
        valid_apis = {"openai_chat", "openai_responses", "anthropic_messages"}
        if options.api_format not in valid_apis:
            raise ValueError(
                f"api_format={options.api_format!r} 不是 moc 支持的协议，"
                f"可选：{sorted(valid_apis)}"
            )
        model_key = json.dumps(self._model_id())
        return (
            "[providers.eval]\n"
            f'api = "{options.api_format}"\n'
            f"base_url = {json.dumps(self._base_url())}\n"
            f'api_key = "env:{options.api_key_env}"\n'
            "timeout_secs = 1800.0\n"
            "\n"
            "[session]\n"
            "compress = false\n"
            "\n"
            f"[providers.eval.models.{model_key}]\n"
            f"context_window = {options.context_window}\n"
            f"max_output_tokens = {options.max_output_tokens}\n"
            f"reasoning = {'true' if options.reasoning else 'false'}\n"
        )

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        await self.ensure_system_dependencies(
            environment, ("bash", "coreutils", "ca_certificates")
        )
        binary = Path(self._opts().binary or "target/release/moc").expanduser()
        if not binary.is_file():
            raise FileNotFoundError(
                f"moc 二进制不存在：{binary}。先在 Linux 上 `cargo build --release`，"
                "或用 --ak binary=<path> 指定"
            )
        env_dir = shlex.quote(str(self.ENV_DIR))
        await self.exec_as_root(
            environment, command=f"mkdir -p {env_dir} {env_dir}/sessions"
        )
        if environment.default_user is not None:
            owner = shlex.quote(str(environment.default_user))
            await self.exec_as_root(
                environment, command=f"chown -R {owner} {env_dir}"
            )
        target = self.ENV_DIR / "moc"
        await self._upload_agent_owned_file(environment, binary, str(target))
        await self.exec_as_root(
            environment,
            command=(
                f"chmod 755 {shlex.quote(str(target))} && "
                f"ln -sf {shlex.quote(str(target))} /usr/local/bin/moc && "
                "moc --version"
            ),
        )

    @override
    async def configure(self, environment: BaseEnvironment) -> None:
        logs_dir = shlex.quote(str(self.environment_logs_dir))
        await self.exec_as_root(environment, command=f"mkdir -p {logs_dir}")
        await self._upload_config_text(
            environment,
            content=self._render_config(),
            remote_path=str(self.CONFIG_PATH),
            filename="config.toml",
        )
        # Harbor 的 native config 作为额外 --config 覆盖层上传。
        if (source := self.config_source) is not None:
            if not isinstance(source, Path):
                raise ValueError("moc 的 config 只接受 TOML 文件路径，不支持内联 JSON")
            await self._upload_agent_owned_file(
                environment, source, str(self.EXTRA_CONFIG_PATH)
            )

    def _run_env(self) -> dict[str, str]:
        """把 API key 注入任务环境：--ak api_key 优先，其次宿主机环境变量。"""
        options = self._opts()
        env: dict[str, str] = {}
        if value := options.api_key or os.environ.get(options.api_key_env):
            env[options.api_key_env] = value
        return env

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        options = self._opts()
        await self._upload_agent_owned_file(
            environment, self._write_instruction(instruction), str(self.INSTRUCTION_PATH)
        )

        config_flags = [f"--config {shlex.quote(str(self.CONFIG_PATH))}"]
        if self.config_source is not None:
            config_flags.append(f"--config {shlex.quote(str(self.EXTRA_CONFIG_PATH))}")
        args = [
            "moc --print",
            "--hide-thinking",
            f"--model {shlex.quote(self._selector())}",
            *config_flags,
            f"--session-dir {shlex.quote(str(self.SESSION_DIR))}",
        ]
        if options.moc_args:
            args.append(options.moc_args)
        log_path = self.environment_logs_dir / "moc.log"
        command = (
            "stdbuf -oL -eL "
            + " ".join(args)
            + f" < {shlex.quote(str(self.INSTRUCTION_PATH))}"
            + f" 2>&1 | stdbuf -oL tee {shlex.quote(str(log_path))}"
        )
        try:
            await self.exec_as_agent(
                environment,
                command=command,
                env=self._run_env(),
                cwd=options.workdir,
            )
        finally:
            await self._collect_artifacts(environment)

    async def _collect_artifacts(self, environment: BaseEnvironment) -> None:
        """把会话 JSONL 拷进 agent 日志目录；失败只记日志不影响结果。"""
        logs_dir = shlex.quote(str(self.environment_logs_dir))
        sessions = shlex.quote(str(self.SESSION_DIR))
        try:
            await environment.exec(
                command=(
                    f"cat {sessions}/*.jsonl > {logs_dir}/moc-session.jsonl "
                    "2>/dev/null; "
                    f"for f in {sessions}/*.jsonl.zst; do "
                    f'[ -e "$f" ] && zstd -dc "$f" >> {logs_dir}/moc-session.jsonl '
                    "2>/dev/null; done; true"
                )
            )
        except Exception as exc:  # noqa: BLE001 - 证据收集不能让 trial 失败
            self.logger.warning("收集 moc 会话文件失败：%s", exc)

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        """从会话 JSONL 的 usage 记录回填 token 统计。

        会话文件已在 run() 末尾拷到宿主机 logs_dir；这里聚合全部
        {"type":"usage"} 条目。首轮 cache read 单独记进 metadata，
        供 first-cold 成本重定价使用。
        """
        session_log = self.logs_dir / "agent" / "moc-session.jsonl"
        if not session_log.is_file():
            # environment_logs_dir 与 logs_dir/agent 的对应关系不保证成立时，
            # 退回到 logs_dir 全量扫描一份拷贝。
            candidates = sorted(self.logs_dir.rglob("moc-session.jsonl"))
            if not candidates:
                self.logger.warning("未找到 moc-session.jsonl，usage 无法回填")
                return
            session_log = candidates[0]

        input_tokens = output_tokens = cache_tokens = cache_write = turns = 0
        first_turn_cached: int | None = None
        for line in session_log.read_text().splitlines():
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue
            if entry.get("type") != "usage":
                continue
            usage = entry.get("usage") or {}
            input_tokens += int(usage.get("input_tokens") or 0)
            output_tokens += int(usage.get("output_tokens") or 0)
            cache_tokens += int(usage.get("cached_input_tokens") or 0)
            cache_write += int(usage.get("cache_creation_input_tokens") or 0)
            if first_turn_cached is None:
                first_turn_cached = int(usage.get("cached_input_tokens") or 0)
            turns += 1

        if turns == 0:
            return
        context.n_input_tokens = input_tokens
        context.n_output_tokens = output_tokens
        context.n_cache_tokens = cache_tokens
        context.model_usage = {
            self._model_id(): ModelUsage(
                n_input_tokens=input_tokens,
                n_cache_tokens=cache_tokens,
                n_output_tokens=output_tokens,
            )
        }
        context.metadata = {
            **(context.metadata or {}),
            "moc_turns": turns,
            "first_turn_cached_tokens": first_turn_cached,
            "cache_creation_input_tokens": cache_write,
            "moc_base_url": self._base_url(),
        }

    def _write_instruction(self, instruction: str) -> Path:
        path = Path(self.logs_dir) / "instruction.txt"
        path.write_text(instruction)
        return path
