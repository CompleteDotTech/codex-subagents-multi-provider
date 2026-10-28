"""Package-wildcard Linux shards; weights are estimates, never capacity proof."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

from hosted_storage import bounded_query

FORK = "CompleteDotTech/codex-subagents-multi-provider"
COUNT = 4
SCOPES = {"test": "//...", "release": "//codex-rs/..."}
WEIGHTS = Path(__file__).with_name("linux_bazel_package_output_bytes.tsv")
LABEL = re.compile(r"^//([A-Za-z0-9_./+\-]*):([A-Za-z0-9_./+@=,~\-]+)$")


def digest(value):
    return hashlib.sha256(value).hexdigest()


def plan(env):
    # Secret availability is evaluated by a step, never a job-if/matrix context.
    availability = env.get("HAS_REMOTE_CREDENTIALS")
    if availability not in ("true", "false"):
        raise ValueError("explicit credential availability required")
    split = (
        env.get("GITHUB_REPOSITORY") == FORK
        and env.get("GITHUB_ACTIONS") == "true"
        and env.get("RUNNER_ENVIRONMENT") == "github-hosted"
        and env.get("RUNNER_OS") == "Linux"
        and availability == "false"
    )
    tests = [
        dict(os="macos-15-xlarge", target=target)
        for target in ("aarch64-apple-darwin", "x86_64-apple-darwin")
    ]
    tests += [
        dict(os="ubuntu-24.04", target=target)
        for target in ("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl")
    ]
    release = [
        dict(os="ubuntu-24.04", target="x86_64-unknown-linux-gnu"),
        dict(os="macos-15-xlarge", target="aarch64-apple-darwin"),
        dict(os="windows-latest", target="x86_64-pc-windows-gnullvm"),
    ]

    def rows(original):
        result = []
        for row in original:
            count = COUNT if split and row["os"] == "ubuntu-24.04" else 1
            for index in range(1, count + 1):
                result.append(
                    dict(
                        row,
                        package_shard=index,
                        package_shard_count=count,
                        shard_suffix=f"-package-{index}-of-{count}"
                        if count > 1
                        else "",
                    )
                )
        return {"include": result}

    # Keep the original upstream Windows runner group/label derivation.
    repository = env.get("GITHUB_EVENT_REPOSITORY_NAME", "codex")
    release[-1]["runs_on"] = {
        "group": repository + "-runners",
        "labels": repository + "-windows-x64",
    }
    return split, rows(tests), rows(release)


def labels(raw):
    if len(raw) > 8 * 1024 * 1024:
        raise ValueError("query output bound exceeded")
    values = raw.decode("utf-8").splitlines()
    if not values or len(values) > 250000 or len(values) != len(set(values)):
        raise ValueError("empty, duplicated or oversized target universe")
    for label in values:
        match = LABEL.fullmatch(label)
        if not match or any(p in (".", "..") for p in match[1].split("/")):
            raise ValueError("invalid main-repository label")
        if match[2] == "all":
            raise ValueError("literal all target would shadow package wildcard")
    return sorted(values)


def check_scope(mode, targets):
    if mode == "release" and any(
        not (x.startswith("//codex-rs/") or x.startswith("//codex-rs:"))
        for x in targets
    ):
        raise ValueError("release package outside original scope")


def package_roots(targets):
    return sorted({label.rsplit(":", 1)[0] + ":all" for label in targets})


def read_weights(path=WEIGHTS):
    result = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        package, value = line.split("\t")
        if package in result or not value.isdecimal() or int(value) < 1:
            raise ValueError("invalid or duplicate package weight")
        result["//" + package + ":all"] = int(value)
    return result


def assign(roots, weights):
    if len(roots) < COUNT or len(roots) != len(set(roots)):
        raise ValueError("four nonempty disjoint package shards required")
    shards = [[] for _ in range(COUNT)]
    loads = [0] * COUNT
    for root in sorted(roots, key=lambda x: (-weights.get(x, 1), x)):
        index = min(range(COUNT), key=lambda i: (loads[i], i))
        shards[index].append(root)
        loads[index] += weights.get(root, 1)
    return [sorted(s) for s in shards], loads


def git_identity():
    # A query may update tracked lockfiles. Do not bind a changed checkout to
    # HEAD's tree or accept package labels produced from untracked source files.
    if subprocess.check_output(
        ["git", "status", "--porcelain", "--untracked-files=normal"], timeout=5
    ).strip():
        raise ValueError("package manifest requires a clean source checkout")

    def rev(ref):
        value = subprocess.check_output(
            ["git", "rev-parse", ref], text=True, timeout=5
        ).strip()
        if not re.fullmatch("[a-f0-9]{40}", value):
            raise ValueError("invalid source identity")
        return value

    return rev("HEAD"), rev("HEAD^{tree}")


def query(expression, env):
    state, data = bounded_query(
        [
            "bash",
            str(Path(__file__).with_name("run-bazel-query-ci.sh")),
            "--output=label",
            "--",
            expression,
        ],
        env,
        timeout=240,
        limit=8 * 1024 * 1024,
    )
    if state != "ok":
        raise ValueError("bounded package query unavailable: " + state)
    return labels(data)


def create(mode, expected_head, env):
    if not (
        env.get("GITHUB_ACTIONS") == "true"
        and env.get("GITHUB_REPOSITORY") == FORK
        and env.get("RUNNER_ENVIRONMENT") == "github-hosted"
        and env.get("RUNNER_OS") == "Linux"
        and not env.get("BUILDBUDDY_API_KEY")
    ):
        raise ValueError("package discovery is only for keyless hosted Linux fork")
    head, tree = git_identity()
    if head != expected_head:
        raise ValueError("checkout does not match requested source")
    original = query(SCOPES[mode], env)
    check_scope(mode, original)
    roots = package_roots(original)
    expanded = query("set(" + " ".join(roots) + ")", env)
    if original != expanded:
        raise ValueError("package wildcard expansion changed original target universe")
    if git_identity() != (head, tree):
        raise ValueError("source changed during discovery")
    assignments, loads = assign(roots, read_weights())
    return dict(
        schema=1,
        mode=mode,
        head=head,
        tree=tree,
        scope=SCOPES[mode],
        run_id=env["GITHUB_RUN_ID"],
        attempt=env["GITHUB_RUN_ATTEMPT"],
        original_labels=original,
        expanded_labels=expanded,
        labels_sha256=digest(("\n".join(original) + "\n").encode()),
        weights_sha256=digest(WEIGHTS.read_bytes()),
        assignments=assignments,
        estimated_recorded_output_bytes=loads,
    )


def select(manifest, mode, expected_head, index, env):
    if not 1 <= index <= COUNT:
        raise ValueError("invalid package shard index")
    head, tree = git_identity()
    if (
        manifest.get("schema"),
        manifest.get("mode"),
        manifest.get("scope"),
        manifest.get("head"),
        manifest.get("tree"),
        manifest.get("run_id"),
        manifest.get("attempt"),
    ) != (
        1,
        mode,
        SCOPES[mode],
        expected_head,
        tree,
        env["GITHUB_RUN_ID"],
        env["GITHUB_RUN_ATTEMPT"],
    ):
        raise ValueError("manifest source, run or mode mismatch")
    if head != expected_head or manifest.get("weights_sha256") != digest(
        WEIGHTS.read_bytes()
    ):
        raise ValueError("consumer source or weights mismatch")
    original = labels(("\n".join(manifest["original_labels"]) + "\n").encode())
    check_scope(mode, original)
    expanded = labels(("\n".join(manifest["expanded_labels"]) + "\n").encode())
    if original != expanded or manifest["labels_sha256"] != digest(
        ("\n".join(original) + "\n").encode()
    ):
        raise ValueError("manifest target conservation mismatch")
    assignments, loads = assign(package_roots(original), read_weights())
    if (
        manifest["assignments"] != assignments
        or manifest["estimated_recorded_output_bytes"] != loads
    ):
        raise ValueError("manifest assignment mismatch")
    return assignments[index - 1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("plan", "create", "select"))
    parser.add_argument("--mode", choices=SCOPES)
    parser.add_argument("--expected-head")
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--shard", type=int)
    args = parser.parse_args()
    if args.operation == "plan":
        split, tests, release = plan(os.environ)
        with open(
            os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8", newline="\n"
        ) as out:
            out.write("package-shards=" + str(split).lower() + "\n")
            for key, value in (("test-matrix", tests), ("release-matrix", release)):
                out.write(key + "=" + json.dumps(value, separators=(",", ":")) + "\n")
    elif args.operation == "create":
        if not args.mode or not args.expected_head or not args.manifest:
            parser.error("create requires mode, source and manifest destination")
        args.manifest.write_text(
            json.dumps(
                create(args.mode, args.expected_head, os.environ), sort_keys=True
            )
            + "\n",
            encoding="utf-8",
        )
    else:
        if (
            not args.mode
            or not args.expected_head
            or not args.manifest
            or args.shard is None
        ):
            parser.error("select requires mode, source, manifest and shard")
        if args.manifest.stat().st_size > 20 * 1024 * 1024:
            raise ValueError("manifest file bound exceeded")
        chosen = select(
            json.loads(args.manifest.read_text(encoding="utf-8")),
            args.mode,
            args.expected_head,
            args.shard,
            os.environ,
        )
        sys.stdout.buffer.write(("\n".join(chosen) + "\n").encode("utf-8"))


if __name__ == "__main__":
    main()
