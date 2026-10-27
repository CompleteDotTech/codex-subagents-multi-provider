"""Manual, observational Windows loader diagnostic; never packages the binaries."""

load("//third_party/voice:windows_executable.bzl", "VoiceExecutablePackageInfo")
load("//third_party/voice:windows_native.bzl", "WindowsBuildToolsInfo")

def _logical(ctx, file):
    return file.short_path[3:] if file.short_path.startswith("../") else ctx.workspace_name + "/" + file.short_path

def _impl(ctx):
    tools = ctx.attr._tools[WindowsBuildToolsInfo]
    files = tools.files.to_list()
    dumpbin = [f for f in files if f.path == tools.inputs["tools"]["dumpbin"]][0]
    helper = ctx.executable.helper
    unit = ctx.executable.unit
    runtime = ctx.attr._runtime[DefaultInfo].default_runfiles
    inventory = [f for f in runtime.files.to_list() if f.basename.lower().endswith(".dll") or f.basename == "runtime.json"]
    config = ctx.actions.declare_file(ctx.label.name + ".json")
    ctx.actions.write(config, json.encode({
        "helper": _logical(ctx, helper),
        "unit": _logical(ctx, unit),
        "packages": {
            name: {
                "raw": _logical(ctx, target[VoiceExecutablePackageInfo].raw_executable),
                "receipt": _logical(ctx, target[VoiceExecutablePackageInfo].receipt),
                "dlls": [_logical(ctx, dll) for dll in target[VoiceExecutablePackageInfo].dlls],
            }
            for name, target in {"helper": ctx.attr.helper, "unit": ctx.attr.unit}.items()
        },
        "dumpbin": _logical(ctx, dumpbin),
        "runtime": [_logical(ctx, f) for f in inventory],
        "status": _logical(ctx, ctx.info_file),
        "tool_environment": tools.environment,
    }))
    launcher = ctx.actions.declare_file(ctx.label.name + ".bat")

    ctx.actions.expand_template(
        template = ctx.file._launcher_template,
        output = launcher,
        substitutions = {
            "__PYTHON__": _logical(ctx, tools.python.interpreter),
            "__DRIVER__": _logical(ctx, ctx.file._driver),
            "__CONFIG__": _logical(ctx, config),
        },
        is_executable = True,
    )
    runfiles = ctx.runfiles(files = [config, ctx.file._driver, ctx.info_file, helper, unit], transitive_files = tools.files)
    for target in [ctx.attr.helper, ctx.attr.unit, ctx.attr._runtime]:
        runfiles = runfiles.merge(target[DefaultInfo].default_runfiles)
    return [DefaultInfo(executable = launcher, runfiles = runfiles)]

windows_runtime_diagnostic_test = rule(
    implementation = _impl,
    test = True,
    attrs = {
        "helper": attr.label(mandatory = True, executable = True, cfg = "target", providers = [VoiceExecutablePackageInfo]),
        "unit": attr.label(mandatory = True, executable = True, cfg = "target", providers = [VoiceExecutablePackageInfo]),
        "_runtime": attr.label(default = "//third_party/voice:native_link_windows_x86_64"),
        "_tools": attr.label(default = "//third_party/voice:windows_tools_x86_64", cfg = "exec", providers = [WindowsBuildToolsInfo]),
        "_launcher_template": attr.label(default = "//third_party/voice:windows_runtime_diagnostic.bat.tpl", allow_single_file = True),
        "_driver": attr.label(default = "//third_party/voice:windows_runtime_diagnostic.py", allow_single_file = True),
    },
)
