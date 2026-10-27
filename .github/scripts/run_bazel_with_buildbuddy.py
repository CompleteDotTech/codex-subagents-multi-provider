#!/usr/bin/env python3

import json
import os
import shutil
import subprocess
import sys
from collections.abc import Mapping
from collections.abc import Sequence
from pathlib import Path


OPENAI_REPOSITORY = "openai/codex"
# Remote configurations select cache/BES/download endpoints. Their -rbe forms
# also select the matching remote executor endpoint.
GENERIC_REMOTE_CONFIG = "buildbuddy-generic"
OPENAI_REMOTE_CONFIG = "buildbuddy-openai"
# These CI configurations require remote build execution. The wrapper supplies
# an RBE configuration, which also includes the common `remote` settings.
REMOTE_EXECUTION_CONFIGS = {
    "--config=ci-linux",
    "--config=ci-macos",
    "--config=ci-v8",
    "--config=ci-windows-cross",
}
# Honor either explicit setting so the wrapper never overrides the caller's
# choice when it supplies the CI default below.
REMOTE_REPO_CONTENTS_CACHE_STARTUP_OPTIONS = {
    "--experimental_remote_repo_contents_cache",
    "--noexperimental_remote_repo_contents_cache",
}


def startup_args(args: Sequence[str], env: Mapping[str, str]) -> list[str]:
    """Return shared startup options that are missing from a Bazel invocation.

    Bazel startup options must precede the command, and changing them restarts
    the server and discards its analysis cache. GitHub Actions invokes Bazel
    through several helpers, so normalize their startup options here while
    preserving any explicit choice made by the caller.
    """
    command_idx = next(
        (idx for idx, arg in enumerate(args) if not arg.startswith("-")),
        len(args),
    )
    configured_startup_args = args[:command_idx]
    injected_args = []

    output_user_root = env.get("BAZEL_OUTPUT_USER_ROOT")
    if output_user_root and not any(
        arg.startswith("--output_user_root=") for arg in configured_startup_args
    ):
        injected_args.append(f"--output_user_root={output_user_root}")

    if env.get("GITHUB_ACTIONS") == "true" and not any(
        arg in REMOTE_REPO_CONTENTS_CACHE_STARTUP_OPTIONS
        for arg in configured_startup_args
    ):
        # Work around Bazel 9 overlay materialization failures seen in CI. This
        # disables only the startup-level repo contents cache; keyed runs still
        # use BuildBuddy.
        injected_args.append("--noexperimental_remote_repo_contents_cache")

    return injected_args


# Only authenticated workflow runs executing trusted upstream code may use the
# OpenAI BuildBuddy host. A pull request event without proof that its head is
# in the upstream repository fails closed to the generic host.
def is_trusted_upstream_run(env: Mapping[str, str]) -> bool:
    # `GITHUB_REPOSITORY` is easy to set locally. Requiring GitHub's workflow
    # marker prevents a local command from opting itself into the OpenAI host.
    if (
        env.get("GITHUB_ACTIONS") != "true"
        or env.get("GITHUB_REPOSITORY") != OPENAI_REPOSITORY
    ):
        return False
    # Non-PR workflow runs in `openai/codex` execute upstream refs, so they are
    # trusted. Fork code reaches these workflows only through pull requests.
    if env.get("GITHUB_EVENT_NAME") != "pull_request":
        return True

    event_path = env.get("GITHUB_EVENT_PATH")
    if not event_path:
        return False
    try:
        event = json.loads(Path(event_path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return False

    try:
        return event["pull_request"]["head"]["repo"]["fork"] is False
    except (KeyError, TypeError):
        return False


def uses_openai_host(env: Mapping[str, str]) -> bool:
    return bool(env.get("BUILDBUDDY_API_KEY")) and is_trusted_upstream_run(env)


def uses_remote_execution(args: Sequence[str]) -> bool:
    try:
        separator_idx = args.index("--")
    except ValueError:
        separator_idx = len(args)
    return any(arg in REMOTE_EXECUTION_CONFIGS for arg in args[:separator_idx])


def remote_config(args: Sequence[str], env: Mapping[str, str]) -> str | None:
    if not env.get("BUILDBUDDY_API_KEY"):
        return None

    config = OPENAI_REMOTE_CONFIG if uses_openai_host(env) else GENERIC_REMOTE_CONFIG
    if uses_remote_execution(args):
        config += "-rbe"
    return config


def bazel_args_without_remote_execution(args: Sequence[str]) -> list[str]:
    # Remote CI configs require BuildBuddy credentials. Removing them preserves
    # the local fallback used for fork pull requests.
    try:
        separator_idx = args.index("--")
    except ValueError:
        separator_idx = len(args)
    return [
        *(arg for arg in args[:separator_idx] if arg not in REMOTE_EXECUTION_CONFIGS),
        *args[separator_idx:],
    ]


def bazel_args_with_remote_config(
    args: Sequence[str], env: Mapping[str, str]
) -> list[str]:
    command_idx = next(
        (idx for idx, arg in enumerate(args) if not arg.startswith("-")),
        None,
    )
    if command_idx is None:
        raise ValueError("expected a Bazel command")

    config = remote_config(args, env)
    if config is None:
        configured_args = bazel_args_without_remote_execution(args)
    else:
        # `remote_config()` returns a configuration only when this key is present.
        api_key = env["BUILDBUDDY_API_KEY"]
        remote_args = [
            f"--config={config}",
            f"--remote_header=x-buildbuddy-api-key={api_key}",
        ]

        # Insert immediately after the Bazel command. This keeps wrapper-added
        # options out of positional payloads and lets later CI configs override
        # shared RBE defaults such as the Windows cross-compilation exec platforms.
        configured_args = [
            *args[: command_idx + 1],
            *remote_args,
            *args[command_idx + 1 :],
        ]

    try:
        separator_idx = configured_args.index("--")
    except ValueError:
        separator_idx = len(configured_args)

    cache_args = [
        f"{option_prefix}{env[env_name]}"
        for env_name, option_prefix in (
            ("BAZEL_REPO_CONTENTS_CACHE", "--repo_contents_cache="),
            ("BAZEL_REPOSITORY_CACHE", "--repository_cache="),
        )
        if env.get(env_name)
        and not any(
            arg.startswith(option_prefix) for arg in configured_args[:separator_idx]
        )
    ]
    return [
        *configured_args[:separator_idx],
        *cache_args,
        *configured_args[separator_idx:],
    ]


def bazel_command(*args: str, env: Mapping[str, str] | None = None) -> list[str]:
    env = os.environ if env is None else env
    bazel = env.get("CODEX_BAZEL_BIN", "bazel")
    return [bazel, *startup_args(args, env), *bazel_args_with_remote_config(args, env)]


METADATA_MODE_NAME = "VOICE_CONFIGURED_METADATA_MODE"
METADATA_MODE_VALUE = "bounded-v1"
METADATA_SDK_NAMES = """PATH INCLUDE LIB LIBPATH UCRTVersion UniversalCRTSdkDir
VCINSTALLDIR VCToolsInstallDir WindowsLibPath WindowsSdkBinPath WindowsSdkDir
WindowsSDKLibVersion WindowsSDKVersion""".upper().split()
METADATA_PATH_OPTIONS = ("--action_env", "--host_action_env", "--test_env")


def metadata_path_mode(args: Sequence[str], env: Mapping[str, str]) -> bool:
    """Validate the explicitly owned diagnostic route; never report input text."""
    indexed = {name.upper(): value for name, value in env.items()}
    mode = indexed.get(METADATA_MODE_NAME)
    context = (
        indexed.get("RUNNER_OS") == "Windows"
        and indexed.get("GITHUB_ACTIONS") == "true"
        and indexed.get("GITHUB_REPOSITORY")
        == "CompleteDotTech/codex-subagents-multi-provider"
        and indexed.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
        and indexed.get("GITHUB_JOB") == "voice-runtime-diagnostic"
    )
    before = list(args[: args.index("--")] if "--" in args else args)
    command_index = next(
        (i for i, arg in enumerate(before) if not arg.startswith("-")), None
    )
    query = command_index is not None and before[command_index] in ("cquery", "aquery")
    batch = (
        command_index is not None
        and before[:command_index].count("--batch") == 1
        and "--nobatch" not in before[:command_index]
    )
    if mode is None and not (context and query and batch):
        return False
    private_root = False
    try:
        root = Path(indexed.get("BAZEL_OUTPUT_USER_ROOT", ""))
        temp = Path(indexed.get("RUNNER_TEMP", ""))
        private_root = (
            root.is_absolute()
            and temp.is_absolute()
            and root.is_dir()
            and temp.is_dir()
            and root.resolve(strict=True).parent == temp.resolve(strict=True)
            and root.resolve(strict=True).name.startswith("voice-metadata-bazel-")
            and len(root.resolve(strict=True).name) > len("voice-metadata-bazel-")
        )
    except (OSError, RuntimeError, ValueError):
        private_root = False
    if mode is None:
        if context and query and batch and private_root:
            raise ValueError("Metadata PATH mode is required")
        return False
    if (
        mode != METADATA_MODE_VALUE
        or os.name != "nt"
        or not context
        or not query
        or not batch
        or not private_root
        or indexed.get("BUILDBUDDY_API_KEY")
        or not all(indexed.get(name) for name in METADATA_SDK_NAMES)
        or not indexed.get("CODEX_BAZEL_WINDOWS_PATH")
        or indexed.get("PROCESSOR_ARCHITECTURE") != "AMD64"
        or indexed.get("VOICE_WINDOWS_HOST_ARCH") != "AMD64"
        or not indexed.get("VOICE_WINDOWS_BAZEL_REPOSITORY")
        or not indexed.get("SYSTEMROOT")
        or indexed.get("VOICE_WINDOWS_SYSTEM_ROOT") != indexed.get("SYSTEMROOT")
    ):
        raise ValueError("Invalid metadata PATH mode context")
    # Only the exact three owned named bindings are permitted; never accept a
    # value assignment, unset form, mixed-case alias or duplicate PATH binding.
    found = {option: 0 for option in METADATA_PATH_OPTIONS}
    for i, arg in enumerate(before):
        for option in METADATA_PATH_OPTIONS:
            value = (
                (before[i + 1] if i + 1 < len(before) else "")
                if arg == option
                else arg[len(option) + 1 :]
                if arg.startswith(option + "=")
                else None
            )
            if (
                value is not None
                and value.lstrip("=").split("=", 1)[0].upper() == "PATH"
            ):
                if arg != option + "=PATH":
                    raise ValueError("Invalid metadata PATH binding")
                found[option] += 1
    if any(count != 1 for count in found.values()):
        raise ValueError("Metadata named PATH bindings are required")
    # The shell normally supplies this exact startup option. Reject an override
    # instead of silently attaching metadata to another output user root.
    if any(
        arg.startswith("--output_user_root=")
        and arg != "--output_user_root=" + indexed["BAZEL_OUTPUT_USER_ROOT"]
        for arg in before[:command_index]
    ):
        raise ValueError("Invalid metadata output root binding")
    return True


def metadata_bazel_launch(command: list[str], env: Mapping[str, str]):
    """Freeze original tool selection, then normalize only the spawned PATH."""
    executable = shutil.which(command[0], path=env.get("PATH"))
    if executable is None:
        raise ValueError("Metadata Bazel executable is unavailable")
    child = dict(env)
    child["PATH"] = env["CODEX_BAZEL_WINDOWS_PATH"]
    return [str(Path(executable).resolve(strict=True)), *command[1:]], child


def main() -> None:
    try:
        metadata_mode = metadata_path_mode(sys.argv[1:], os.environ)
    except ValueError:
        print("Invalid configured metadata PATH context or bindings.", file=sys.stderr)
        raise SystemExit(1) from None
    config = remote_config(sys.argv[1:], os.environ)
    if config is None:
        print(
            "BuildBuddy key unavailable; using local Bazel configuration.",
            file=sys.stderr,
        )
    else:
        host_description = (
            "OpenAI tenant" if uses_openai_host(os.environ) else "generic"
        )
        print(
            f"Using {host_description} BuildBuddy configuration: {config}.",
            file=sys.stderr,
        )

    command = bazel_command(*sys.argv[1:])
    if os.name == "nt":
        # Windows CRT exec can split arguments containing spaces and lose the
        # eventual child exit status. Wait for Bazel and propagate its status.
        if metadata_mode:
            try:
                command, child_environment = metadata_bazel_launch(command, os.environ)
            except (OSError, ValueError):
                print(
                    "Configured metadata Bazel executable unavailable.", file=sys.stderr
                )
                raise SystemExit(1) from None
            result = subprocess.run(command, check=False, env=child_environment)
        else:
            result = subprocess.run(command, check=False)
        raise SystemExit(result.returncode)

    os.execvp(command[0], command)


if __name__ == "__main__":
    main()
