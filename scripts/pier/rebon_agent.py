from __future__ import annotations

import json
import shlex
import shutil
import sys
import tempfile
from ipaddress import IPv4Address, ip_address
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from pier.agents.installed.base import (
    BaseInstalledAgent,
    CliFlag,
    NonZeroAgentExitCodeError,
    with_prompt_template,
)
from pier.agents.network import allowlist_from_urls
from pier.environments.base import BaseEnvironment
from pier.environments.docker.docker import DockerEnvironment
from pier.environments.modal import ModalEnvironment
from pier.models.agent.context import AgentContext
from pier.models.agent.install import AgentInstallSpec, InstallStep
from pier.models.agent.network import NetworkAllowlist
from pier.models.task.config import TaskOS
from pier.models.trajectories import (
    Agent,
    FinalMetrics,
    Metrics,
    Observation,
    ObservationResult,
    Step,
    ToolCall,
    Trajectory,
)
from pier.utils.trajectory_metrics import (
    extra_with_context_metrics,
    populate_context_from_final_metrics,
)
from pier.utils.trajectory_utils import format_trajectory_json


class _WindowsLinuxTextEnvironmentMixin:
    _TEXT_FILENAMES = {
        "Dockerfile",
        "GNUmakefile",
        "Makefile",
    }
    _TEXT_SUFFIXES = {
        ".bash",
        ".bat",
        ".c",
        ".cc",
        ".cfg",
        ".cmd",
        ".conf",
        ".cpp",
        ".css",
        ".diff",
        ".fish",
        ".go",
        ".h",
        ".hpp",
        ".html",
        ".ini",
        ".java",
        ".js",
        ".json",
        ".jsonl",
        ".jsx",
        ".md",
        ".mjs",
        ".patch",
        ".pl",
        ".ps1",
        ".py",
        ".rb",
        ".rs",
        ".sh",
        ".sql",
        ".toml",
        ".ts",
        ".tsx",
        ".txt",
        ".xml",
        ".yaml",
        ".yml",
        ".zsh",
    }

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        self._normalized_environment_context: (
            tempfile.TemporaryDirectory[str] | None
        ) = None
        self._original_environment_dir: Path | None = None
        super().__init__(*args, **kwargs)

    @classmethod
    def _is_text_file(cls, path: Path) -> bool:
        return (
            path.name in cls._TEXT_FILENAMES
            or path.suffix.lower() in cls._TEXT_SUFFIXES
        )

    @classmethod
    def _normalize_text_file(cls, path: Path) -> None:
        if path.is_symlink() or not cls._is_text_file(path):
            return
        content = path.read_bytes()
        normalized = content.replace(b"\r\n", b"\n")
        if normalized != content:
            path.write_bytes(normalized)

    @classmethod
    def _normalize_text_tree(cls, root: Path) -> None:
        for path in root.rglob("*"):
            if path.is_file():
                cls._normalize_text_file(path)

    def _normalizes_windows_transfers(self) -> bool:
        return sys.platform == "win32" and self.task_os != TaskOS.WINDOWS

    def _prepare_normalized_environment_context(self) -> None:
        if (
            not self._normalizes_windows_transfers()
            or self._normalized_environment_context is not None
        ):
            return

        temporary = tempfile.TemporaryDirectory()
        context_dir = Path(temporary.name) / "context"
        try:
            shutil.copytree(self.environment_dir, context_dir)
            self._normalize_text_tree(context_dir)
        except Exception:
            temporary.cleanup()
            raise

        self._original_environment_dir = self.environment_dir
        self._normalized_environment_context = temporary
        self.environment_dir = context_dir
        env_vars = getattr(self, "_env_vars", None)
        if env_vars is not None:
            env_vars.context_dir = str(context_dir.resolve().absolute())

    def _cleanup_normalized_environment_context(self) -> None:
        temporary = self._normalized_environment_context
        if temporary is None:
            return

        if self._original_environment_dir is not None:
            self.environment_dir = self._original_environment_dir
            env_vars = getattr(self, "_env_vars", None)
            if env_vars is not None:
                env_vars.context_dir = str(
                    self._original_environment_dir.resolve().absolute()
                )
        self._original_environment_dir = None
        self._normalized_environment_context = None
        temporary.cleanup()

    async def start(self, force_build: bool) -> None:
        self._prepare_normalized_environment_context()
        try:
            await super().start(force_build)
        except Exception:
            self._cleanup_normalized_environment_context()
            raise

    async def upload_file(self, source_path: Path | str, target_path: str) -> None:
        source = Path(source_path)
        if not self._normalizes_windows_transfers() or not self._is_text_file(source):
            await super().upload_file(source_path, target_path)
            return

        with tempfile.TemporaryDirectory() as directory:
            normalized_source = Path(directory) / source.name
            shutil.copy2(source, normalized_source)
            self._normalize_text_file(normalized_source)
            await super().upload_file(normalized_source, target_path)

    async def upload_dir(self, source_dir: Path | str, target_dir: str) -> None:
        if not self._normalizes_windows_transfers():
            await super().upload_dir(source_dir, target_dir)
            return

        with tempfile.TemporaryDirectory() as directory:
            normalized_source = Path(directory) / "source"
            shutil.copytree(source_dir, normalized_source)
            self._normalize_text_tree(normalized_source)
            await super().upload_dir(normalized_source, target_dir)

    async def stop(self, delete: bool) -> None:
        try:
            await super().stop(delete)
        finally:
            self._cleanup_normalized_environment_context()


class RebonDockerEnvironment(_WindowsLinuxTextEnvironmentMixin, DockerEnvironment):
    def _prepare_egress_proxy_compose(self) -> None:
        super()._prepare_egress_proxy_compose()

        compose_path = self._egress_proxy_compose_path
        if compose_path is None:
            return
        script_path = compose_path.parent / "egress-proxy" / "start-squid.sh"
        if not script_path.exists():
            return

        content = script_path.read_bytes()
        normalized = content.replace(b"\r\n", b"\n")
        if normalized != content:
            script_path.write_bytes(normalized)


class RebonModalEnvironment(_WindowsLinuxTextEnvironmentMixin, ModalEnvironment):
    async def _ensure_egress_proxy(self) -> None:
        await super()._ensure_egress_proxy()

        sandbox = self._egress_proxy_sandbox
        proxy_url = self._egress_proxy_env.get("HTTPS_PROXY")
        if sandbox is None or proxy_url is None:
            return

        hostname = urlsplit(proxy_url).hostname
        if hostname is None:
            raise RuntimeError("Pier egress proxy URL has no hostname")

        resolver = (
            "import json,socket,sys;"
            "host=sys.argv[1];"
            "ips=sorted({item[4][0] for item in "
            "socket.getaddrinfo(host,None,socket.AF_INET,socket.SOCK_STREAM)});"
            "print(json.dumps(ips))"
        )
        process = await sandbox.exec.aio(
            "python3", "-c", resolver, hostname, timeout=30
        )
        stdout = await process.stdout.read.aio()
        stderr = await process.stderr.read.aio()
        return_code = await process.wait.aio()
        if return_code != 0:
            raise RuntimeError(
                f"Failed to resolve Pier egress proxy inside Modal: {stderr.strip()}"
            )

        try:
            resolved = json.loads(stdout)
            addresses = sorted(
                {
                    str(address)
                    for value in resolved
                    if isinstance(value, str)
                    and isinstance(address := ip_address(value), IPv4Address)
                }
            )
        except (json.JSONDecodeError, ValueError, TypeError) as error:
            raise RuntimeError(
                "Pier egress proxy returned invalid IPv4 resolution data"
            ) from error
        if not addresses:
            raise RuntimeError("Pier egress proxy did not resolve to an IPv4 address")

        self._egress_cidr_allowlist = [f"{address}/32" for address in addresses]


class RebonAgent(BaseInstalledAgent):
    SUPPORTS_ATIF = True
    _OUTPUT_FILENAME = "rebon.jsonl"
    _REMOTE_CONFIG_DIR = "/tmp/rebon-config"
    _REMOTE_CREDENTIALS_PATH = f"{_REMOTE_CONFIG_DIR}/.credentials.json"
    _OPENAI_OAUTH_BASE_URL = "https://chatgpt.com/backend-api/codex/responses"
    _OPENAI_OAUTH_TOKEN_URL = "https://auth.openai.com/oauth/token"
    _OPENAI_OAUTH_SENTINEL = "$OPENAI_OAUTH_TOKEN"
    _DEFAULT_DENIED_TOOLS = "EnterPlanMode,ExitPlanMode"
    _DEFAULT_MAX_ITERATIONS = 128
    _PROVIDER_DEFAULTS = {
        "openai": ("openai-responses", "https://api.openai.com", "OPENAI_API_KEY"),
        "anthropic": ("anthropic", "https://api.anthropic.com", "ANTHROPIC_API_KEY"),
    }
    CLI_FLAGS = [
        CliFlag(
            "reasoning_effort",
            cli="--effort",
            type="enum",
            choices=["low", "medium", "high", "xhigh", "max"],
            default="high",
        )
    ]

    def __init__(
        self,
        *args: Any,
        command_model_name: str | None = None,
        provider_format: str | None = None,
        base_url: str | None = None,
        api_key_env: str | None = None,
        auth_mode: str = "api_key",
        oauth_credentials_path: str | None = None,
        fast_mode: bool = False,
        max_iterations: int = _DEFAULT_MAX_ITERATIONS,
        **kwargs: Any,
    ) -> None:
        if auth_mode not in {"api_key", "openai_oauth"}:
            raise ValueError("auth_mode must be api_key or openai_oauth")
        if not isinstance(fast_mode, bool):
            raise ValueError("fast_mode must be true or false")
        if (
            isinstance(max_iterations, bool)
            or not isinstance(max_iterations, int)
            or max_iterations <= 0
        ):
            raise ValueError("max_iterations must be a positive integer")
        self._command_model_name = command_model_name
        self._provider_format = provider_format
        self._base_url = base_url
        self._api_key_env = api_key_env
        self._auth_mode = auth_mode
        self._oauth_credentials_path = oauth_credentials_path
        self._fast_mode = fast_mode
        self._max_iterations = max_iterations
        super().__init__(*args, **kwargs)

    @staticmethod
    def name() -> str:
        return "rebon"

    def get_version_command(self) -> str | None:
        return '[ ! -s "$HOME/.nvm/nvm.sh" ] || . "$HOME/.nvm/nvm.sh"; rebon --version'

    def parse_version(self, stdout: str) -> str:
        text = stdout.strip()
        return text.removeprefix("rebon").strip() or text

    def install_spec(self) -> AgentInstallSpec:
        version_spec = f"@{self._version}" if self._version else "@latest"
        root_run = (
            "if command -v apt-get >/dev/null 2>&1; then"
            " apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y curl ca-certificates;"
            " elif command -v apk >/dev/null 2>&1; then"
            " apk add --no-cache curl bash nodejs npm;"
            " elif command -v yum >/dev/null 2>&1; then"
            " yum install -y curl ca-certificates;"
            " else echo 'No supported package manager found' >&2; exit 1; fi"
        )
        agent_run = (
            "set -euo pipefail; "
            "if [ -f /etc/alpine-release ]; then"
            " npm install -g @rebon/cli"
            f"{version_spec};"
            " else"
            " curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.2/install.sh | bash &&"
            ' export NVM_DIR="$HOME/.nvm" &&'
            ' \\. "$NVM_DIR/nvm.sh" &&'
            " nvm install 22 && nvm alias default 22 &&"
            f" npm install -g @rebon/cli{version_spec};"
            " fi;"
            ' [ ! -s "$HOME/.nvm/nvm.sh" ] || \\. "$HOME/.nvm/nvm.sh";'
            " rebon --version"
        )
        return AgentInstallSpec(
            agent_name=self.name(),
            version=self._version,
            steps=[
                InstallStep(user="root", run=root_run),
                InstallStep(user="agent", run=agent_run),
            ],
            verification_command=self.get_version_command(),
        )

    def _provider_settings(self) -> tuple[str, str, str | None, str]:
        if not self.model_name:
            raise ValueError("Rebon requires --model provider/model_name")

        if "/" in self.model_name:
            provider, metadata_model = self.model_name.split("/", 1)
        else:
            provider, metadata_model = "", self.model_name

        if self._auth_mode == "openai_oauth":
            if provider != "openai":
                raise ValueError("openai_oauth requires an openai/model_name model")
            if self._provider_format not in {None, "openai-responses"}:
                raise ValueError(
                    "openai_oauth requires provider_format=openai-responses"
                )
            if self._base_url not in {None, self._OPENAI_OAUTH_BASE_URL}:
                raise ValueError("openai_oauth cannot be sent to a custom base_url")
            return (
                "openai-responses",
                self._OPENAI_OAUTH_BASE_URL,
                None,
                self._command_model_name or metadata_model,
            )

        defaults = self._PROVIDER_DEFAULTS.get(provider)
        provider_format = self._provider_format or (defaults[0] if defaults else None)
        base_url = self._base_url
        api_key_env = self._api_key_env

        if base_url is None:
            if provider == "openai":
                base_url = self._get_env("OPENAI_BASE_URL")
            elif provider == "anthropic":
                base_url = self._get_env("ANTHROPIC_BASE_URL")
            if base_url is None and defaults:
                base_url = defaults[1]
        if api_key_env is None and defaults:
            api_key_env = defaults[2]

        if not provider_format or not base_url or not api_key_env:
            raise ValueError(
                "Unknown Rebon provider. Pass provider_format, base_url, and api_key_env."
            )

        command_model = self._command_model_name or metadata_model
        return provider_format, base_url, api_key_env, command_model

    def network_allowlist(self) -> NetworkAllowlist:
        _, base_url, _, _ = self._provider_settings()
        urls = [base_url]
        if self._auth_mode == "openai_oauth":
            urls.append(self._OPENAI_OAUTH_TOKEN_URL)
        return allowlist_from_urls(urls)

    def _openai_oauth_credentials(self) -> dict[str, Any]:
        raw_path = (
            self._oauth_credentials_path
            or self._get_env("REBON_OAUTH_CREDENTIALS_PATH")
            or str(Path.home() / ".rebon" / ".credentials.json")
        )
        path = Path(raw_path).expanduser()
        if not path.is_file():
            raise ValueError(f"Rebon OAuth credentials file does not exist: {path}")
        try:
            credentials = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise ValueError(
                f"Cannot read Rebon OAuth credentials file: {path}"
            ) from error
        oauth = (
            credentials.get("openaiOAuth") if isinstance(credentials, dict) else None
        )
        if not isinstance(oauth, dict) or not isinstance(oauth.get("accessToken"), str):
            raise ValueError(
                "Rebon OAuth credentials do not contain openaiOAuth.accessToken"
            )
        if not oauth["accessToken"]:
            raise ValueError("Rebon OAuth access token is empty")
        return {"openaiOAuth": oauth}

    def _runtime_config(self) -> tuple[dict[str, Any], str | None, str]:
        provider_format, base_url, api_key_env, command_model = (
            self._provider_settings()
        )
        if api_key_env is not None and not self._get_env(api_key_env):
            raise ValueError(
                f"Missing Rebon credential environment variable: {api_key_env}"
            )
        config = {
            "activeCustomProvider": "pier",
            "hasCompletedOnboarding": True,
            "customProviders": [
                {
                    "name": "pier",
                    "format": provider_format,
                    "baseUrl": base_url,
                    "apiKey": self._OPENAI_OAUTH_SENTINEL
                    if self._auth_mode == "openai_oauth"
                    else "",
                    "model": command_model,
                }
            ],
        }
        if self._fast_mode:
            config["serviceTier"] = "fast"
            config["features"] = {"fastMode": True}
        return config, api_key_env, command_model

    def _parse_events(self) -> list[dict[str, Any]]:
        path = self.logs_dir / self._OUTPUT_FILENAME
        if not path.exists():
            return []
        events: list[dict[str, Any]] = []
        for raw_line in path.read_text(encoding="utf-8").splitlines():
            try:
                event = json.loads(raw_line)
            except json.JSONDecodeError:
                continue
            if isinstance(event, dict):
                events.append(event)
        return events

    @staticmethod
    def _new_turn(iteration: int | None = None) -> dict[str, Any]:
        return {
            "iteration": iteration,
            "messages": [],
            "reasoning": [],
            "tool_calls": [],
            "observations": [],
            "usage": None,
            "model": None,
            "stop_reason": None,
        }

    @classmethod
    def _group_bounded_events(
        cls, events: list[dict[str, Any]]
    ) -> list[dict[str, Any]]:
        turns: list[dict[str, Any]] = []
        pending: dict[str, Any] | None = None
        last_completed: dict[str, Any] | None = None

        def pending_turn(iteration: int) -> dict[str, Any]:
            nonlocal pending
            if pending is None or pending["iteration"] != iteration:
                pending = cls._new_turn(iteration)
            return pending

        for event in events:
            event_type = event.get("type")
            iteration = event.get("iteration")
            if event_type in {"message", "thinking"} and isinstance(iteration, int):
                turn = pending_turn(iteration)
                if event_type == "message" and isinstance(event.get("text"), str):
                    turn["messages"].append(event["text"])
                elif event_type == "thinking" and isinstance(event.get("text"), str):
                    turn["reasoning"].append(event["text"])
                continue

            if event_type == "turn.completed" and isinstance(iteration, int):
                turn = pending_turn(iteration)
                turn["usage"] = event.get("usage")
                turn["model"] = event.get("model")
                turn["stop_reason"] = event.get("stopReason")
                turns.append(turn)
                last_completed = turn
                pending = None
                continue

            if event_type == "action.called" and last_completed is not None:
                last_completed["tool_calls"].append(event)
            elif event_type == "action.result" and last_completed is not None:
                last_completed["observations"].append(event)

        return turns

    @classmethod
    def _group_legacy_events(cls, events: list[dict[str, Any]]) -> list[dict[str, Any]]:
        turns: list[dict[str, Any]] = []
        current: dict[str, Any] | None = None

        def ensure_turn() -> dict[str, Any]:
            nonlocal current
            if current is None:
                current = cls._new_turn()
            return current

        def finish_turn() -> None:
            nonlocal current
            if current is not None:
                turns.append(current)
                current = None

        for event in events:
            event_type = event.get("type")
            if event_type in {"message", "thinking"}:
                if current is not None and current["observations"]:
                    finish_turn()
                turn = ensure_turn()
                text = event.get("text")
                if isinstance(text, str):
                    turn["messages" if event_type == "message" else "reasoning"].append(
                        text
                    )
            elif event_type == "action.called":
                ensure_turn()["tool_calls"].append(event)
            elif event_type == "action.result":
                ensure_turn()["observations"].append(event)

        finish_turn()
        return turns

    @staticmethod
    def _usage_metrics(usage: Any) -> Metrics | None:
        if not isinstance(usage, dict):
            return None

        def count(name: str) -> int:
            value = usage.get(name)
            return value if isinstance(value, int) and value > 0 else 0

        input_tokens = count("input_tokens")
        cache_read = count("cache_read_input_tokens")
        cache_creation = count("cache_creation_input_tokens")
        cache_hit = count("prompt_cache_hit_tokens")
        cache_miss = count("prompt_cache_miss_tokens")
        total_input = count("total_input_tokens")
        total_output = count("total_output_tokens")

        if total_input:
            prompt_tokens = total_input
        elif cache_hit or cache_miss:
            prompt_tokens = input_tokens or cache_hit + cache_miss
        else:
            prompt_tokens = input_tokens + cache_read + cache_creation
        completion_tokens = total_output or count("output_tokens")
        cached_tokens = cache_hit + cache_read

        if not prompt_tokens and not completion_tokens and not cached_tokens:
            return None
        return Metrics(
            prompt_tokens=prompt_tokens or None,
            completion_tokens=completion_tokens or None,
            cached_tokens=cached_tokens or None,
        )

    @staticmethod
    def _stringify_output(value: Any) -> str:
        if isinstance(value, str):
            return value
        return json.dumps(value, ensure_ascii=False, sort_keys=True)

    def _turn_to_step(self, turn: dict[str, Any], step_id: int) -> Step:
        tool_calls: list[ToolCall] = []
        known_call_ids: set[str] = set()
        for event in turn["tool_calls"]:
            call_id = event.get("callId")
            name = event.get("name")
            if not isinstance(call_id, str) or not isinstance(name, str):
                continue
            arguments = event.get("input")
            if not isinstance(arguments, dict):
                arguments = {"value": arguments}
            known_call_ids.add(call_id)
            tool_calls.append(
                ToolCall(
                    tool_call_id=call_id,
                    function_name=name,
                    arguments=arguments,
                )
            )

        observations: list[ObservationResult] = []
        for event in turn["observations"]:
            call_id = event.get("callId")
            if not isinstance(call_id, str) or call_id not in known_call_ids:
                continue
            status = event.get("status")
            observations.append(
                ObservationResult(
                    source_call_id=call_id,
                    content=self._stringify_output(event.get("output")),
                    extra={"status": status} if isinstance(status, str) else None,
                )
            )

        kwargs: dict[str, Any] = {
            "step_id": step_id,
            "source": "agent",
            "message": "\n\n".join(turn["messages"]),
            "model_name": turn.get("model") or self.model_name,
            "reasoning_effort": self._resolved_flags.get("reasoning_effort"),
            "llm_call_count": 1,
        }
        if turn["reasoning"]:
            kwargs["reasoning_content"] = "\n\n".join(turn["reasoning"])
        if tool_calls:
            kwargs["tool_calls"] = tool_calls
        if observations:
            kwargs["observation"] = Observation(results=observations)
        if metrics := self._usage_metrics(turn.get("usage")):
            kwargs["metrics"] = metrics
        if turn.get("iteration") is not None or turn.get("stop_reason") is not None:
            kwargs["extra"] = {
                key: value
                for key, value in {
                    "iteration": turn.get("iteration"),
                    "stop_reason": turn.get("stop_reason"),
                }.items()
                if value is not None
            }
        return Step(**kwargs)

    def _convert_events_to_trajectory(
        self, events: list[dict[str, Any]]
    ) -> Trajectory | None:
        if not events:
            return None

        has_boundaries = any(event.get("type") == "turn.completed" for event in events)
        turns = (
            self._group_bounded_events(events)
            if has_boundaries
            else self._group_legacy_events(events)
        )
        if not turns:
            return None

        steps = [
            self._turn_to_step(turn, index + 1) for index, turn in enumerate(turns)
        ]
        step_metrics = [step.metrics for step in steps if step.metrics is not None]
        if step_metrics:
            total_prompt = sum(metric.prompt_tokens or 0 for metric in step_metrics)
            total_completion = sum(
                metric.completion_tokens or 0 for metric in step_metrics
            )
            total_cached = sum(metric.cached_tokens or 0 for metric in step_metrics)
        else:
            result = next(
                (event for event in reversed(events) if event.get("type") == "result"),
                {},
            )
            fallback = self._usage_metrics(result.get("usage"))
            total_prompt = fallback.prompt_tokens if fallback else 0
            total_completion = fallback.completion_tokens if fallback else 0
            total_cached = fallback.cached_tokens if fallback else 0

        peak_context = max(
            (metric.prompt_tokens or 0 for metric in step_metrics), default=0
        )
        compaction_starts = sum(
            1
            for event in events
            if event.get("type") == "compaction" and event.get("phase") == "started"
        )
        legacy_compactions = sum(
            1
            for event in events
            if event.get("type") == "compaction" and event.get("phase") is None
        )
        summarization_count = compaction_starts + (legacy_compactions + 1) // 2
        session = next(
            (
                event.get("sessionId")
                for event in events
                if event.get("type") == "session"
            ),
            None,
        )
        errors = [
            event.get("message")
            for event in events
            if event.get("type") == "error" and isinstance(event.get("message"), str)
        ]
        final_extra = extra_with_context_metrics(
            {"llm_call_count": len(steps)},
            peak_context_tokens=peak_context or None,
            summarization_count=summarization_count or None,
        )

        return Trajectory(
            schema_version="ATIF-v1.7",
            session_id=session if isinstance(session, str) else None,
            agent=Agent(
                name=self.name(),
                version=self.version() or "unknown",
                model_name=self.model_name,
            ),
            steps=steps,
            notes=None
            if has_boundaries
            else "Parsed from legacy Rebon JSONL without turn boundaries.",
            final_metrics=FinalMetrics(
                total_prompt_tokens=total_prompt or None,
                total_completion_tokens=total_completion or None,
                total_cached_tokens=total_cached or None,
                total_steps=len(steps),
                extra=final_extra,
            ),
            extra={"errors": errors} if errors else None,
        )

    def populate_context_post_run(self, context: AgentContext) -> None:
        events = self._parse_events()
        if not events:
            return
        try:
            trajectory = self._convert_events_to_trajectory(events)
        except Exception:
            self.logger.exception("Failed to convert Rebon events to ATIF")
            return
        if trajectory is None:
            return

        path = self.logs_dir / "trajectory.json"
        path.write_text(
            format_trajectory_json(trajectory.to_json_dict()), encoding="utf-8"
        )
        if trajectory.final_metrics:
            populate_context_from_final_metrics(context, trajectory.final_metrics)
            context.n_agent_steps = len(trajectory.steps)

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        config, api_key_env, command_model = self._runtime_config()
        oauth_credentials = (
            self._openai_oauth_credentials()
            if self._auth_mode == "openai_oauth"
            else None
        )
        base_env = {
            "REBON_CONFIG_DIR": self._REMOTE_CONFIG_DIR,
            "REBON_PROVIDER_CONFIG": json.dumps(config, separators=(",", ":")),
            "REBON_API_KEY_ENV": api_key_env,
            "REBON_DENY_TOOLS": self._DEFAULT_DENIED_TOOLS,
        }
        if api_key_env is not None:
            base_env[api_key_env] = self._get_env(api_key_env)
        env = self.build_process_env(base_env)
        config_script = (
            "const fs=require('fs');"
            "const cfg=JSON.parse(process.env.REBON_PROVIDER_CONFIG);"
            "const keyEnv=process.env.REBON_API_KEY_ENV;"
            "if(keyEnv){"
            "const key=process.env[keyEnv];"
            "if(!key){throw new Error('missing Rebon API key');}"
            "cfg.customProviders[0].apiKey=key;"
            "}"
            "fs.mkdirSync(process.env.REBON_CONFIG_DIR,{recursive:true,mode:0o700});"
            "fs.chmodSync(process.env.REBON_CONFIG_DIR,0o700);"
            "const path=process.env.REBON_CONFIG_DIR+'/config.json';"
            "fs.writeFileSync(path,JSON.stringify(cfg),{mode:0o600});"
            "fs.chmodSync(path,0o600);"
        )
        await self.exec_as_agent(
            environment,
            command=(
                'git -C /app config user.name "Rebon Eval" && '
                'git -C /app config user.email "rebon-eval@localhost" && '
                '{ [ ! -s "$HOME/.nvm/nvm.sh" ] || . "$HOME/.nvm/nvm.sh"; } && '
                f"node -e {shlex.quote(config_script)}"
            ),
            env=env,
        )

        if oauth_credentials is not None:
            temporary_path: Path | None = None
            try:
                with tempfile.NamedTemporaryFile(
                    mode="w", encoding="utf-8", suffix=".json", delete=False
                ) as temporary:
                    json.dump(oauth_credentials, temporary, separators=(",", ":"))
                    temporary_path = Path(temporary.name)
                await environment.upload_file(
                    temporary_path, self._REMOTE_CREDENTIALS_PATH
                )
                ownership = ""
                if environment.default_user is not None:
                    ownership = (
                        f"chown {shlex.quote(str(environment.default_user))} "
                        f"{shlex.quote(self._REMOTE_CREDENTIALS_PATH)} && "
                    )
                await self.exec_as_root(
                    environment,
                    command=(
                        f"{ownership}chmod 600 "
                        f"{shlex.quote(self._REMOTE_CREDENTIALS_PATH)}"
                    ),
                )
            finally:
                if temporary_path is not None:
                    temporary_path.unlink(missing_ok=True)

        flags = self.build_cli_flags()
        flags_arg = f" {flags}" if flags else ""
        await self.exec_as_agent(
            environment,
            command=(
                '[ ! -s "$HOME/.nvm/nvm.sh" ] || . "$HOME/.nvm/nvm.sh"; '
                f"rebon --model {shlex.quote(command_model)}{flags_arg} "
                f"exec --json --max-iterations {self._max_iterations} -- "
                f"{shlex.quote(instruction)} "
                f"2> /logs/agent/rebon.stderr | tee /logs/agent/{self._OUTPUT_FILENAME}"
            ),
            env=env,
        )

        errors = [
            event.get("message")
            for event in self._parse_events()
            if event.get("type") == "error" and isinstance(event.get("message"), str)
        ]
        if errors:
            raise NonZeroAgentExitCodeError(
                "Rebon emitted error event(s): " + "; ".join(errors[:3])
            )
