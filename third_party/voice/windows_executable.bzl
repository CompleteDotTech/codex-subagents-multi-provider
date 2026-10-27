"""Declared executable-adjacent voice DLLs for Windows/MSVC only."""

load("@rules_python//python:py_runtime_info.bzl", "PyRuntimeInfo")

VoiceExecutablePackageInfo = provider(fields = ["raw_executable", "executable", "dlls", "receipt"])

def _basename_key(file):
    return file.basename.lower()

def _impl(ctx):
    raw = ctx.executable.binary
    runtime = ctx.attr.runtime[DefaultInfo]
    dlls = sorted([file for file in runtime.default_runfiles.files.to_list() if file.basename.lower().endswith(".dll")], key = _basename_key)
    if not dlls:
        fail("Windows voice executable requires its declared DLL closure")
    names = {raw.basename.lower(): raw}
    for dll in dlls:
        key = dll.basename.lower()
        if key in names:
            fail("Case-insensitive voice package basename collision: " + dll.basename)
        names[key] = dll
    executable = ctx.actions.declare_file(ctx.label.name + "/" + raw.basename)
    copied = [ctx.actions.declare_file(ctx.label.name + "/" + dll.basename) for dll in dlls]
    receipt = ctx.actions.declare_file(ctx.label.name + "/packaging.json")
    config = ctx.actions.declare_file(ctx.label.name + ".copy.json")
    ctx.actions.write(config, json.encode({
        "directory": executable.dirname,
        "executable": {"source": raw.path, "output": executable.path},
        "dlls": [{"source": source.path, "output": output.path} for source, output in zip(dlls, copied)],
        "receipt": receipt.path,
    }))
    python = ctx.attr._python[PyRuntimeInfo]
    ctx.actions.run(
        executable = python.interpreter,
        arguments = [ctx.file._copy.path, config.path],
        inputs = depset([raw, config, ctx.file._copy, python.interpreter] + dlls, transitive = [python.files]),
        outputs = [executable, receipt] + copied,
        mnemonic = "VoiceExecutablePackage",
    )
    own = ctx.runfiles(files = [raw, executable, receipt] + copied)
    binary = ctx.attr.binary[DefaultInfo]
    providers = [
        DefaultInfo(
            executable = executable,
            files = depset([executable]),
            default_runfiles = own.merge(binary.default_runfiles).merge(runtime.default_runfiles),
            data_runfiles = own.merge(binary.data_runfiles).merge(runtime.data_runfiles),
        ),
        VoiceExecutablePackageInfo(raw_executable = raw, executable = executable, dlls = copied, receipt = receipt),
    ]
    if OutputGroupInfo in ctx.attr.binary:
        providers.append(ctx.attr.binary[OutputGroupInfo])
    if RunEnvironmentInfo in ctx.attr.binary:
        providers.append(ctx.attr.binary[RunEnvironmentInfo])
    return providers

windows_voice_executable = rule(
    implementation = _impl,
    executable = True,
    attrs = {
        "binary": attr.label(mandatory = True, executable = True, cfg = "target"),
        "runtime": attr.label(mandatory = True),
        "_copy": attr.label(default = "//third_party/voice:package_windows_executable.py", allow_single_file = True),
        "_python": attr.label(default = "@python_3_12//:py3_runtime", cfg = "exec"),
    },
)
