import asyncio
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from pier.environments.docker.docker import DockerEnvironment
from pier.environments.modal import ModalEnvironment
from pier.models.agent.context import AgentContext
from pier.models.task.config import TaskOS

from scripts.pier.rebon_agent import (
    RebonAgent,
    RebonDockerEnvironment,
    RebonModalEnvironment,
)


class _AsyncResult:
    def __init__(self, value):
        self.aio = self._get
        self._value = value

    async def _get(self):
        return self._value


class _FakeModalProcess:
    def __init__(self, stdout: str, stderr: str = "", return_code: int = 0):
        self.stdout = SimpleNamespace(read=_AsyncResult(stdout))
        self.stderr = SimpleNamespace(read=_AsyncResult(stderr))
        self.wait = _AsyncResult(return_code)


class _FakeModalSandbox:
    def __init__(self, process: _FakeModalProcess):
        self.process = process
        self.calls = []
        self.exec = SimpleNamespace(aio=self._exec)

    async def _exec(self, *args, **kwargs):
        self.calls.append((args, kwargs))
        return self.process


class RebonDockerEnvironmentTests(unittest.TestCase):
    def test_normalizes_generated_proxy_script_to_lf(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            trial_dir = Path(directory)
            compose_path = trial_dir / "docker-compose-egress-proxy.json"
            script_path = trial_dir / "egress-proxy" / "start-squid.sh"

            def base_setup(environment) -> None:
                script_path.parent.mkdir()
                script_path.write_bytes(b"#!/usr/bin/env bash\r\nset -eu\r\n")
                environment._egress_proxy_compose_path = compose_path

            environment = object.__new__(RebonDockerEnvironment)
            with patch.object(
                DockerEnvironment, "_prepare_egress_proxy_compose", new=base_setup
            ):
                environment._prepare_egress_proxy_compose()

            self.assertEqual(
                script_path.read_bytes(), b"#!/usr/bin/env bash\nset -eu\n"
            )

    def test_handles_absent_egress_proxy(self) -> None:
        def base_setup(environment) -> None:
            environment._egress_proxy_compose_path = None

        environment = object.__new__(RebonDockerEnvironment)
        with patch.object(
            DockerEnvironment, "_prepare_egress_proxy_compose", new=base_setup
        ):
            environment._prepare_egress_proxy_compose()

        self.assertIsNone(environment._egress_proxy_compose_path)

    def test_normalizes_windows_linux_build_context_without_mutating_source(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source_dir = root / "source"
            source_dir.mkdir()
            (source_dir / "Dockerfile").write_bytes(b"FROM base\r\nRUN true\r\n")
            (source_dir / "test.patch").write_bytes(b"--- a/file\r\n+++ b/file\r\n")
            (source_dir / "fixture.bin").write_bytes(b"binary\r\npayload")

            environment = object.__new__(RebonDockerEnvironment)
            environment._normalized_environment_context = None
            environment._original_environment_dir = None
            environment.task_env_config = SimpleNamespace(os=TaskOS.LINUX)
            environment.environment_dir = source_dir
            environment._env_vars = SimpleNamespace(context_dir=str(source_dir))
            observed = {}

            async def base_setup(env, force_build) -> None:
                observed["force_build"] = force_build
                observed["context_dir"] = env.environment_dir
                observed["dockerfile"] = (
                    env.environment_dir / "Dockerfile"
                ).read_bytes()
                observed["patch"] = (env.environment_dir / "test.patch").read_bytes()
                observed["binary"] = (env.environment_dir / "fixture.bin").read_bytes()

            with (
                patch("scripts.pier.rebon_agent.sys.platform", "win32"),
                patch.object(DockerEnvironment, "start", new=base_setup),
            ):
                asyncio.run(environment.start(force_build=True))

            self.assertTrue(observed["force_build"])
            copied_context = observed["context_dir"]
            self.assertNotEqual(copied_context, source_dir)
            self.assertEqual(observed["dockerfile"], b"FROM base\nRUN true\n")
            self.assertEqual(observed["patch"], b"--- a/file\n+++ b/file\n")
            self.assertEqual(observed["binary"], b"binary\r\npayload")
            self.assertEqual(
                (source_dir / "test.patch").read_bytes(),
                b"--- a/file\r\n+++ b/file\r\n",
            )

            environment._cleanup_normalized_environment_context()
            self.assertEqual(environment.environment_dir, source_dir)
            self.assertFalse(copied_context.exists())

    def test_normalizes_windows_linux_directory_upload(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            source_dir = Path(directory) / "source"
            source_dir.mkdir()
            (source_dir / "test.sh").write_bytes(b"#!/bin/sh\r\nexit 0\r\n")
            (source_dir / "test.patch").write_bytes(b"--- a/file\r\n+++ b/file\r\n")
            observed = {}

            async def base_upload(_environment, uploaded_dir, target_dir) -> None:
                uploaded = Path(uploaded_dir)
                observed["target_dir"] = target_dir
                observed["script"] = (uploaded / "test.sh").read_bytes()
                observed["patch"] = (uploaded / "test.patch").read_bytes()

            environment = object.__new__(RebonDockerEnvironment)
            environment.task_env_config = SimpleNamespace(os=TaskOS.LINUX)
            with (
                patch("scripts.pier.rebon_agent.sys.platform", "win32"),
                patch.object(DockerEnvironment, "upload_dir", new=base_upload),
            ):
                asyncio.run(environment.upload_dir(source_dir, "/tests"))

            self.assertEqual(observed["target_dir"], "/tests")
            self.assertEqual(observed["script"], b"#!/bin/sh\nexit 0\n")
            self.assertEqual(observed["patch"], b"--- a/file\n+++ b/file\n")
            self.assertEqual(
                (source_dir / "test.patch").read_bytes(),
                b"--- a/file\r\n+++ b/file\r\n",
            )

    def test_normalizes_windows_linux_file_upload(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "change.diff"
            source.write_bytes(b"--- a/file\r\n+++ b/file\r\n")
            observed = {}

            async def base_upload(_environment, uploaded_file, target_path) -> None:
                observed["target_path"] = target_path
                observed["content"] = Path(uploaded_file).read_bytes()

            environment = object.__new__(RebonDockerEnvironment)
            environment.task_env_config = SimpleNamespace(os=TaskOS.LINUX)
            with (
                patch("scripts.pier.rebon_agent.sys.platform", "win32"),
                patch.object(DockerEnvironment, "upload_file", new=base_upload),
            ):
                asyncio.run(environment.upload_file(source, "/tmp/change.diff"))

            self.assertEqual(observed["target_path"], "/tmp/change.diff")
            self.assertEqual(observed["content"], b"--- a/file\n+++ b/file\n")
            self.assertEqual(source.read_bytes(), b"--- a/file\r\n+++ b/file\r\n")

    def test_preserves_windows_container_transfers(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "test.sh"
            source.write_bytes(b"#!/bin/sh\r\n")
            observed = {}

            async def base_upload(_environment, uploaded_file, _target_path) -> None:
                observed["path"] = Path(uploaded_file)
                observed["content"] = Path(uploaded_file).read_bytes()

            environment = object.__new__(RebonDockerEnvironment)
            environment.task_env_config = SimpleNamespace(os=TaskOS.WINDOWS)
            with (
                patch("scripts.pier.rebon_agent.sys.platform", "win32"),
                patch.object(DockerEnvironment, "upload_file", new=base_upload),
            ):
                asyncio.run(environment.upload_file(source, "C:/tests/test.sh"))

            self.assertEqual(observed["path"], source)
            self.assertEqual(observed["content"], b"#!/bin/sh\r\n")


class RebonModalEnvironmentTests(unittest.TestCase):
    def test_normalizes_windows_linux_build_context(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            source_dir = Path(directory) / "source"
            source_dir.mkdir()
            (source_dir / "Dockerfile").write_bytes(b"FROM base\r\n")
            (source_dir / "test.patch").write_bytes(b"--- a/file\r\n+++ b/file\r\n")

            environment = object.__new__(RebonModalEnvironment)
            environment._normalized_environment_context = None
            environment._original_environment_dir = None
            environment.task_env_config = SimpleNamespace(os=TaskOS.LINUX)
            environment.environment_dir = source_dir
            observed = {}

            async def base_start(env, force_build) -> None:
                observed["force_build"] = force_build
                observed["context_dir"] = env.environment_dir
                observed["dockerfile"] = (
                    env.environment_dir / "Dockerfile"
                ).read_bytes()
                observed["patch"] = (env.environment_dir / "test.patch").read_bytes()

            with (
                patch("scripts.pier.rebon_agent.sys.platform", "win32"),
                patch.object(ModalEnvironment, "start", new=base_start),
            ):
                asyncio.run(environment.start(force_build=False))

            copied_context = observed["context_dir"]
            self.assertFalse(observed["force_build"])
            self.assertNotEqual(copied_context, source_dir)
            self.assertEqual(observed["dockerfile"], b"FROM base\n")
            self.assertEqual(observed["patch"], b"--- a/file\n+++ b/file\n")
            self.assertEqual(
                (source_dir / "test.patch").read_bytes(),
                b"--- a/file\r\n+++ b/file\r\n",
            )

            environment._cleanup_normalized_environment_context()
            self.assertEqual(environment.environment_dir, source_dir)
            self.assertFalse(copied_context.exists())

    def test_resolves_proxy_from_modal_instead_of_host_fake_ip(self) -> None:
        async def base_setup(_environment) -> None:
            return None

        process = _FakeModalProcess('["63.32.89.206", "63.32.89.207"]\n')
        sandbox = _FakeModalSandbox(process)
        environment = object.__new__(RebonModalEnvironment)
        environment._egress_proxy_sandbox = sandbox
        environment._egress_proxy_env = {
            "HTTPS_PROXY": "http://agent:secret@proxy.example:1234"
        }
        environment._egress_cidr_allowlist = ["198.18.1.53/32"]

        with patch.object(ModalEnvironment, "_ensure_egress_proxy", new=base_setup):
            asyncio.run(environment._ensure_egress_proxy())

        self.assertEqual(
            environment._egress_cidr_allowlist,
            ["63.32.89.206/32", "63.32.89.207/32"],
        )
        args, kwargs = sandbox.calls[0]
        self.assertEqual(args[-1], "proxy.example")
        self.assertNotIn("secret", json.dumps([args, kwargs]))

    def test_rejects_invalid_modal_proxy_resolution(self) -> None:
        async def base_setup(_environment) -> None:
            return None

        environment = object.__new__(RebonModalEnvironment)
        environment._egress_proxy_sandbox = _FakeModalSandbox(
            _FakeModalProcess("not-json")
        )
        environment._egress_proxy_env = {
            "HTTPS_PROXY": "http://agent:secret@proxy.example:1234"
        }
        environment._egress_cidr_allowlist = ["198.18.1.53/32"]

        with patch.object(ModalEnvironment, "_ensure_egress_proxy", new=base_setup):
            with self.assertRaisesRegex(RuntimeError, "invalid IPv4"):
                asyncio.run(environment._ensure_egress_proxy())


class RebonAgentTests(unittest.TestCase):
    def make_agent(self, logs_dir: Path, **kwargs) -> RebonAgent:
        return RebonAgent(
            logs_dir=logs_dir,
            model_name="openai/gpt-5.5",
            extra_env={"OPENAI_API_KEY": "test-key"},
            **kwargs,
        )

    def make_oauth_agent(self, logs_dir: Path, **kwargs) -> RebonAgent:
        credentials_path = logs_dir / ".credentials.json"
        credentials_path.write_text(
            json.dumps(
                {
                    "openaiOAuth": {
                        "accessToken": "oauth-access-token",
                        "refreshToken": "oauth-refresh-token",
                        "expiresAt": 1_800_000_000_000,
                    },
                    "unrelatedSecret": {"token": "must-not-upload"},
                }
            ),
            encoding="utf-8",
        )
        return RebonAgent(
            logs_dir=logs_dir,
            model_name="openai/gpt-5.6-sol",
            auth_mode="openai_oauth",
            oauth_credentials_path=str(credentials_path),
            **kwargs,
        )

    def test_openai_defaults_use_responses_and_allowlist_api_host(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = self.make_agent(Path(directory))
            self.assertEqual(
                agent._provider_settings(),
                (
                    "openai-responses",
                    "https://api.openai.com",
                    "OPENAI_API_KEY",
                    "gpt-5.5",
                ),
            )
            self.assertEqual(agent.network_allowlist().domains, ["api.openai.com"])

    def test_openai_oauth_uses_codex_endpoint_and_refresh_allowlist(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = self.make_oauth_agent(Path(directory))
            self.assertEqual(
                agent._provider_settings(),
                (
                    "openai-responses",
                    "https://chatgpt.com/backend-api/codex/responses",
                    None,
                    "gpt-5.6-sol",
                ),
            )
            self.assertCountEqual(
                agent.network_allowlist().domains,
                ["chatgpt.com", "auth.openai.com"],
            )
            config, api_key_env, model = agent._runtime_config()
            self.assertIsNone(api_key_env)
            self.assertEqual(model, "gpt-5.6-sol")
            self.assertEqual(
                config["customProviders"][0]["apiKey"], "$OPENAI_OAUTH_TOKEN"
            )

    def test_fast_mode_writes_rebon_service_tier_config(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = self.make_oauth_agent(Path(directory), fast_mode=True)
            config, _, _ = agent._runtime_config()

            self.assertEqual(config["serviceTier"], "fast")
            self.assertEqual(config["features"], {"fastMode": True})

    def test_fast_mode_is_absent_by_default(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = self.make_oauth_agent(Path(directory))
            config, _, _ = agent._runtime_config()

            self.assertNotIn("serviceTier", config)
            self.assertNotIn("features", config)

    def test_fast_mode_rejects_non_boolean_values(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "fast_mode must be true or false"):
                self.make_oauth_agent(Path(directory), fast_mode="true")

    def test_max_iterations_is_configurable_and_positive(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(
                self.make_agent(Path(directory))._max_iterations,
                RebonAgent._DEFAULT_MAX_ITERATIONS,
            )
            agent = self.make_agent(Path(directory), max_iterations=100)
            self.assertEqual(agent._max_iterations, 100)

            for invalid in (0, -1, True, "128"):
                with self.subTest(invalid=invalid):
                    with self.assertRaisesRegex(
                        ValueError, "max_iterations must be a positive integer"
                    ):
                        self.make_agent(Path(directory), max_iterations=invalid)

    def test_openai_oauth_rejects_non_openai_models_and_custom_endpoints(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            credentials_path = path / ".credentials.json"
            credentials_path.write_text(
                json.dumps({"openaiOAuth": {"accessToken": "token"}}),
                encoding="utf-8",
            )
            anthropic = RebonAgent(
                logs_dir=path,
                model_name="anthropic/claude-opus-4-8",
                auth_mode="openai_oauth",
                oauth_credentials_path=str(credentials_path),
            )
            with self.assertRaisesRegex(ValueError, "requires an openai"):
                anthropic.network_allowlist()

            custom_endpoint = RebonAgent(
                logs_dir=path,
                model_name="openai/gpt-5.6-sol",
                auth_mode="openai_oauth",
                oauth_credentials_path=str(credentials_path),
                base_url="https://example.invalid",
            )
            with self.assertRaisesRegex(ValueError, "custom base_url"):
                custom_endpoint.network_allowlist()

    def test_openai_oauth_uploads_only_openai_credentials_without_env_leak(
        self,
    ) -> None:
        class FakeEnvironment:
            default_user = None

            def __init__(self) -> None:
                self.calls = []
                self.uploads = []

            @staticmethod
            def agent_process_env(env):
                return env or {}

            async def exec(self, **kwargs):
                self.calls.append(kwargs)
                return SimpleNamespace(return_code=0, stdout="", stderr="")

            async def upload_file(self, source, destination):
                self.uploads.append(
                    (destination, json.loads(Path(source).read_text(encoding="utf-8")))
                )

        with tempfile.TemporaryDirectory() as directory:
            agent = self.make_oauth_agent(
                Path(directory), reasoning_effort="xhigh", fast_mode=True
            )
            environment = FakeEnvironment()
            asyncio.run(agent.run("fix the task", environment, AgentContext()))

            self.assertEqual(len(environment.calls), 3)
            self.assertEqual(len(environment.uploads), 1)
            destination, uploaded = environment.uploads[0]
            self.assertEqual(destination, "/tmp/rebon-config/.credentials.json")
            self.assertEqual(set(uploaded), {"openaiOAuth"})
            self.assertEqual(
                uploaded["openaiOAuth"]["accessToken"], "oauth-access-token"
            )
            for call in environment.calls:
                self.assertNotIn("oauth-access-token", call["command"])
                serialized_env = json.dumps(call.get("env") or {})
                self.assertNotIn("oauth-access-token", serialized_env)
                self.assertNotIn("oauth-refresh-token", serialized_env)
            self.assertIn(
                "chmod 600 /tmp/rebon-config/.credentials.json",
                environment.calls[1]["command"],
            )
            config = json.loads(environment.calls[0]["env"]["REBON_PROVIDER_CONFIG"])
            self.assertEqual(
                config["customProviders"][0]["apiKey"], "$OPENAI_OAUTH_TOKEN"
            )
            self.assertEqual(config["serviceTier"], "fast")
            self.assertEqual(config["features"], {"fastMode": True})
            self.assertNotIn("OPENAI_API_KEY", environment.calls[-1]["env"])

    def test_host_api_key_is_forwarded_without_embedding_it_in_commands(self) -> None:
        class FakeEnvironment:
            def __init__(self) -> None:
                self.calls = []

            @staticmethod
            def agent_process_env(env):
                return env or {}

            async def exec(self, **kwargs):
                self.calls.append(kwargs)
                return SimpleNamespace(return_code=0, stdout="", stderr="")

        with tempfile.TemporaryDirectory() as directory:
            with patch.dict("os.environ", {"OPENAI_API_KEY": "host-only-key"}):
                agent = RebonAgent(
                    logs_dir=Path(directory), model_name="openai/gpt-5.5"
                )
                environment = FakeEnvironment()
                asyncio.run(agent.run("fix the task", environment, AgentContext()))

            self.assertEqual(len(environment.calls), 2)
            for call in environment.calls:
                self.assertEqual(call["env"]["OPENAI_API_KEY"], "host-only-key")
                self.assertNotIn("host-only-key", call["command"])
            self.assertIn("mode:0o700", environment.calls[0]["command"])
            self.assertIn("mode:0o600", environment.calls[0]["command"])

    def test_custom_provider_requires_explicit_connection_settings(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = RebonAgent(
                logs_dir=Path(directory),
                model_name="respan/gpt-5.5",
                extra_env={"RESPAN_API_KEY": "test-key"},
            )
            with self.assertRaisesRegex(ValueError, "Unknown Rebon provider"):
                agent.network_allowlist()

    def test_install_spec_pins_requested_rebon_version(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = self.make_agent(Path(directory), version="0.5.1")
            spec = agent.install_spec()
            self.assertEqual(spec.version, "0.5.1")
            self.assertIn("@rebon/cli@0.5.1", spec.steps[1].run)

    def test_bounded_jsonl_becomes_one_atif_step_per_iteration(self) -> None:
        events = [
            {"type": "session", "sessionId": "sess-1"},
            {"type": "thinking", "iteration": 0, "text": "inspect"},
            {
                "type": "turn.completed",
                "iteration": 0,
                "model": "gpt-5.5",
                "stopReason": "tool_use",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 10,
                    "cache_read_input_tokens": 20,
                },
            },
            {
                "type": "action.called",
                "callId": "call-1",
                "name": "Read",
                "input": {"file_path": "README.md"},
            },
            {
                "type": "action.result",
                "callId": "call-1",
                "name": "Read",
                "status": "completed",
                "output": {"content": "hello"},
            },
            {"type": "message", "iteration": 1, "role": "assistant", "text": "done"},
            {
                "type": "turn.completed",
                "iteration": 1,
                "model": "gpt-5.5",
                "stopReason": "end_turn",
                "usage": {"input_tokens": 200, "output_tokens": 20},
            },
            {"type": "compaction", "phase": "started"},
            {"type": "compaction", "phase": "finished"},
            {"type": "result", "sessionId": "sess-1", "usage": {}},
        ]

        with tempfile.TemporaryDirectory() as directory:
            logs_dir = Path(directory)
            agent = self.make_agent(logs_dir)
            trajectory = agent._convert_events_to_trajectory(events)
            self.assertIsNotNone(trajectory)
            assert trajectory is not None
            self.assertEqual(trajectory.schema_version, "ATIF-v1.7")
            self.assertEqual(trajectory.session_id, "sess-1")
            self.assertEqual(len(trajectory.steps), 2)
            self.assertEqual(trajectory.steps[0].reasoning_content, "inspect")
            self.assertEqual(trajectory.steps[0].tool_calls[0].function_name, "Read")
            self.assertEqual(
                json.loads(trajectory.steps[0].observation.results[0].content),
                {"content": "hello"},
            )
            self.assertEqual(trajectory.steps[1].message, "done")
            self.assertEqual(trajectory.final_metrics.total_prompt_tokens, 320)
            self.assertEqual(trajectory.final_metrics.total_completion_tokens, 30)
            self.assertEqual(trajectory.final_metrics.total_cached_tokens, 20)
            self.assertEqual(trajectory.final_metrics.extra["peak_context_tokens"], 200)
            self.assertEqual(trajectory.final_metrics.extra["summarization_count"], 1)
            self.assertEqual(trajectory.final_metrics.extra["llm_call_count"], 2)

    def test_bounded_jsonl_preserves_repeated_iterations_after_context_reset(
        self,
    ) -> None:
        events = [
            {"type": "session", "sessionId": "sess-reset"},
            {"type": "message", "iteration": 0, "text": "planning"},
            {
                "type": "turn.completed",
                "iteration": 0,
                "usage": {"input_tokens": 100, "output_tokens": 10},
            },
            {
                "type": "action.called",
                "callId": "enter-plan",
                "name": "EnterPlanMode",
                "input": {},
            },
            {
                "type": "action.result",
                "callId": "enter-plan",
                "status": "completed",
                "output": {},
            },
            {"type": "message", "iteration": 1, "text": "plan ready"},
            {
                "type": "turn.completed",
                "iteration": 1,
                "usage": {"input_tokens": 200, "output_tokens": 20},
            },
            {
                "type": "action.called",
                "callId": "exit-plan",
                "name": "ExitPlanMode",
                "input": {"plan": "implement"},
            },
            {
                "type": "action.result",
                "callId": "exit-plan",
                "status": "completed",
                "output": {"clearContext": True},
            },
            {"type": "message", "iteration": 0, "text": "implementation"},
            {
                "type": "turn.completed",
                "iteration": 0,
                "usage": {"input_tokens": 300, "output_tokens": 30},
            },
            {
                "type": "action.called",
                "callId": "read-after-reset",
                "name": "Read",
                "input": {"file_path": "README.md"},
            },
            {
                "type": "action.result",
                "callId": "read-after-reset",
                "status": "completed",
                "output": "ok",
            },
        ]

        with tempfile.TemporaryDirectory() as directory:
            agent = self.make_agent(Path(directory))
            trajectory = agent._convert_events_to_trajectory(events)
            self.assertIsNotNone(trajectory)
            assert trajectory is not None
            self.assertEqual(len(trajectory.steps), 3)
            self.assertEqual(
                [step.message for step in trajectory.steps],
                ["planning", "plan ready", "implementation"],
            )
            self.assertEqual(
                [step.extra["iteration"] for step in trajectory.steps], [0, 1, 0]
            )
            self.assertEqual(
                [step.tool_calls[0].function_name for step in trajectory.steps],
                ["EnterPlanMode", "ExitPlanMode", "Read"],
            )
            self.assertEqual(trajectory.final_metrics.total_prompt_tokens, 600)
            self.assertEqual(trajectory.final_metrics.total_completion_tokens, 60)
            self.assertEqual(trajectory.final_metrics.extra["llm_call_count"], 3)

    def test_legacy_jsonl_is_grouped_around_tool_observations(self) -> None:
        events = [
            {"type": "session", "sessionId": "legacy"},
            {"type": "thinking", "text": "inspect"},
            {
                "type": "action.called",
                "callId": "call-1",
                "name": "Read",
                "input": {},
            },
            {
                "type": "action.result",
                "callId": "call-1",
                "status": "completed",
                "output": "ok",
            },
            {"type": "message", "role": "assistant", "text": "finished"},
            {
                "type": "result",
                "usage": {"input_tokens": 50, "output_tokens": 5},
            },
        ]

        with tempfile.TemporaryDirectory() as directory:
            agent = self.make_agent(Path(directory))
            trajectory = agent._convert_events_to_trajectory(events)
            self.assertIsNotNone(trajectory)
            assert trajectory is not None
            self.assertEqual(len(trajectory.steps), 2)
            self.assertEqual(trajectory.steps[1].message, "finished")
            self.assertIn("legacy Rebon JSONL", trajectory.notes)
            self.assertEqual(trajectory.final_metrics.total_prompt_tokens, 50)

    def test_run_builds_config_without_embedding_the_key_in_commands(self) -> None:
        class FakeEnvironment:
            def __init__(self) -> None:
                self.calls = []

            @staticmethod
            def agent_process_env(env):
                return env or {}

            async def exec(self, **kwargs):
                self.calls.append(kwargs)
                return SimpleNamespace(return_code=0, stdout="", stderr="")

        with tempfile.TemporaryDirectory() as directory:
            agent = self.make_agent(
                Path(directory), reasoning_effort="xhigh", max_iterations=100
            )
            environment = FakeEnvironment()
            asyncio.run(agent.run("fix the task", environment, AgentContext()))

            self.assertEqual(len(environment.calls), 2)
            config_call, run_call = environment.calls
            self.assertIn(
                'git -C /app config user.name "Rebon Eval"', config_call["command"]
            )
            self.assertIn(
                'git -C /app config user.email "rebon-eval@localhost"',
                config_call["command"],
            )
            self.assertIn("node -e", config_call["command"])
            self.assertNotIn("test-key", config_call["command"])
            self.assertEqual(config_call["env"]["OPENAI_API_KEY"], "test-key")
            self.assertEqual(
                run_call["env"]["REBON_DENY_TOOLS"],
                "EnterPlanMode,ExitPlanMode",
            )
            self.assertIn(
                "rebon --model gpt-5.5 --effort xhigh exec --json "
                "--max-iterations 100",
                run_call["command"],
            )
            self.assertNotIn("test-key", run_call["command"])

    def test_populate_context_writes_trajectory_and_metrics(self) -> None:
        events = [
            {"type": "session", "sessionId": "sess-2"},
            {"type": "message", "iteration": 0, "text": "done"},
            {
                "type": "turn.completed",
                "iteration": 0,
                "usage": {"input_tokens": 25, "output_tokens": 4},
            },
        ]

        with tempfile.TemporaryDirectory() as directory:
            logs_dir = Path(directory)
            (logs_dir / "rebon.jsonl").write_text(
                "\n".join(json.dumps(event) for event in events), encoding="utf-8"
            )
            agent = self.make_agent(logs_dir)
            context = AgentContext()
            agent.populate_context_post_run(context)
            self.assertTrue((logs_dir / "trajectory.json").exists())
            self.assertEqual(context.n_input_tokens, 25)
            self.assertEqual(context.n_output_tokens, 4)
            self.assertEqual(context.n_agent_steps, 1)


if __name__ == "__main__":
    unittest.main()
