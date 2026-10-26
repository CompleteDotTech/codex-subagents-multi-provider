"""Exercise the actual subprocess boundary without changing host kernel limits."""

import os
import subprocess
import unittest
from unittest.mock import patch

import provision_macos_ci_descriptors as provision


class ProvisionTest(unittest.TestCase):
    def test_invalid_numeric_readback_is_rejected(self):
        for value in ("unknown", "0", "-1", "16384\nsecret", "１６３８４"):
            with (
                self.subTest(value=value),
                patch.object(
                    provision.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess([], 0, value, ""),
                ),
                self.assertRaises(ValueError),
            ):
                provision.read_limit("kern.maxfilesperproc")

    def invoke(self, values, *, reject=False, write_error=False, environment=None):
        commands = []
        env = {
            "RUNNER_OS": "macOS",
            "RUNNER_ENVIRONMENT": "github-hosted",
            "GITHUB_ACTIONS": "true",
            "GITHUB_REPOSITORY": provision.FORK,
        }
        if environment:
            env.update(environment)

        def run(argv, **kwargs):
            commands.append(argv)
            if argv[0] == "sudo":
                if write_error:
                    raise subprocess.CalledProcessError(1, argv)
                name, value = argv[-1].split("=")
                if not reject:
                    values[name] = int(value)
                return subprocess.CompletedProcess(argv, 0)
            return subprocess.CompletedProcess(
                argv, 0, str(values[argv[-1]]) + "\n", ""
            )

        with (
            patch.dict(os.environ, env, clear=True),
            patch.object(provision.sys, "platform", "darwin"),
            patch.object(provision.subprocess, "run", side_effect=run),
        ):
            provision.main()
        return commands

    def test_confirmed_host_ceiling_is_raised_and_read_back(self):
        values = {"kern.maxfiles": 30720, "kern.maxfilesperproc": 10240}
        commands = self.invoke(values)
        self.assertEqual(
            [c for c in commands if c[0] == "sudo"],
            [["sudo", "-n", "/usr/sbin/sysctl", "-w", "kern.maxfilesperproc=16384"]],
        )
        self.assertEqual(values["kern.maxfilesperproc"], 16384)

    def test_global_ceiling_first_and_existing_higher_limits_retained(self):
        commands = self.invoke({"kern.maxfiles": 8192, "kern.maxfilesperproc": 8192})
        self.assertEqual(
            [c[-1] for c in commands if c[0] == "sudo"],
            ["kern.maxfiles=16384", "kern.maxfilesperproc=16384"],
        )
        commands = self.invoke({"kern.maxfiles": 65536, "kern.maxfilesperproc": 32768})
        self.assertFalse(any(c[0] == "sudo" for c in commands))

    def test_failed_write_and_insufficient_readback_fail_setup(self):
        for kwargs, error in (
            ({"reject": True}, RuntimeError),
            ({"write_error": True}, subprocess.CalledProcessError),
        ):
            with self.subTest(kwargs=kwargs), self.assertRaises(error):
                self.invoke(
                    {"kern.maxfiles": 30720, "kern.maxfilesperproc": 10240}, **kwargs
                )

    def test_scope_excludes_other_repositories_personal_hosts_and_non_actions(self):
        for name, value in (
            ("GITHUB_REPOSITORY", "openai/codex"),
            ("GITHUB_ACTIONS", "false"),
            ("RUNNER_ENVIRONMENT", "self-hosted"),
            ("RUNNER_OS", "Windows"),
        ):
            self.assertEqual(self.invoke({}, environment={name: value}), [])
        with (
            patch.object(provision.sys, "platform", "win32"),
            patch.object(provision.subprocess, "run") as run,
        ):
            provision.main()
            run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
