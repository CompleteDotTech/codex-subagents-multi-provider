#!/usr/bin/env bash

set -euo pipefail

print_failed_bazel_test_logs=0
print_failed_bazel_action_summary=0
remote_download_toplevel=0
windows_msvc_host_platform=0
windows_cross_compile=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --print-failed-test-logs)
      print_failed_bazel_test_logs=1
      shift
      ;;
    --print-failed-action-summary)
      print_failed_bazel_action_summary=1
      shift
      ;;
    --remote-download-toplevel)
      remote_download_toplevel=1
      shift
      ;;
    --windows-msvc-host-platform)
      windows_msvc_host_platform=1
      shift
      ;;
    --windows-cross-compile)
      windows_cross_compile=1
      shift
      ;;
    --)
      shift
      break
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

if [[ $# -eq 0 ]]; then
  echo "Usage: $0 [--print-failed-test-logs] [--print-failed-action-summary] [--remote-download-toplevel] [--windows-msvc-host-platform] [--windows-cross-compile] -- <bazel args> -- <targets>" >&2
  exit 1
fi

bazel_startup_args=()
if [[ -n "${BAZEL_OUTPUT_USER_ROOT:-}" ]]; then
  bazel_startup_args+=("--output_user_root=${BAZEL_OUTPUT_USER_ROOT}")
fi

run_bazel() {
  if [[ "${RUNNER_OS:-}" == "Windows" ]]; then
    MSYS2_ARG_CONV_EXCL='*' "$(dirname "${BASH_SOURCE[0]}")/run_bazel_with_buildbuddy.py" "$@"
    return
  fi

  "$(dirname "${BASH_SOURCE[0]}")/run_bazel_with_buildbuddy.py" "$@"
}

run_bazel_with_startup_args() {
  if (( ${#bazel_startup_args[@]} > 0 )); then
    run_bazel "${bazel_startup_args[@]}" "$@"
    return
  fi

  run_bazel "$@"
}

ci_config=ci-linux
case "${RUNNER_OS:-}" in
  macOS)
    ci_config=ci-macos
    ;;
  Windows)
    if [[ $windows_cross_compile -eq 1 ]]; then
      ci_config=ci-windows-cross
    else
      ci_config=ci-windows
    fi
    ;;
esac

print_bazel_test_log_tails() {
  local console_log="$1"
  local testlogs_dir

  local -a bazel_info_args=(info)
  if [[ -n "${BUILDBUDDY_API_KEY:-}" ]]; then
    # `bazel info` needs the same CI config as the failed test invocation so
    # platform-specific output roots match. On Windows, omitting `ci-windows`
    # would point at `local_windows-fastbuild` even when the test ran with the
    # MSVC host platform under `local_windows_msvc-fastbuild`.
    bazel_info_args+=("--config=${ci_config}")
  fi
  if [[ $windows_native_fallback -eq 1 ]]; then
    if [[ -n "$requested_target_platform" ]]; then
      bazel_info_args+=("--platforms=${requested_target_platform}")
    fi
    if [[ -n "$requested_host_platform" ]]; then
      bazel_info_args+=("--host_platform=${requested_host_platform}")
    fi
  fi

  # Only pass flags that affect Bazel's output-root selection or repository
  # lookup. Test/build-only flags such as execution logs or remote download
  # mode can make `bazel info` fail, which would hide the real test log path.
  for arg in "${post_config_bazel_args[@]}"; do
    case "$arg" in
      --host_platform=* | --platforms=* | --repo_contents_cache=* | --repository_cache=*)
        bazel_info_args+=("$arg")
        ;;
    esac
  done

  testlogs_dir="$(run_bazel_with_startup_args \
    --noexperimental_remote_repo_contents_cache \
    "${bazel_info_args[@]}" \
    bazel-testlogs 2>/dev/null || echo bazel-testlogs)"

  local failed_targets=()
  while IFS= read -r target; do
    failed_targets+=("$target")
  done < <(
    grep -E '^(FAIL: //|ERROR: .* Testing //)' "$console_log" \
      | sed -E 's#^FAIL: (//[^ ]+).*#\1#; s#^ERROR: .* Testing (//[^ ]+) failed:.*#\1#' \
      | sort -u
  )

  if [[ ${#failed_targets[@]} -eq 0 ]]; then
    echo "No failed Bazel test targets were found in console output."
    return
  fi

  for target in "${failed_targets[@]}"; do
    local rel_path="${target#//}"
    rel_path="${rel_path/://}"
    local test_log="${testlogs_dir}/${rel_path}/test.log"
    local reported_test_log
    reported_test_log="$(grep -F "FAIL: ${target} " "$console_log" | sed -nE 's#.* \(see (.*[\\/]test\.log)\).*#\1#p' | head -n 1 || true)"
    if [[ -n "$reported_test_log" ]]; then
      reported_test_log="${reported_test_log//\\//}"
      test_log="$reported_test_log"
    fi

    echo "::group::Bazel test log tail for ${target}"
    if [[ -f "$test_log" ]]; then
      tail -n 200 "$test_log"
    else
      echo "Missing test log: $test_log"
    fi
    echo "::endgroup::"
  done
}

print_bazel_action_failure_summary() {
  local console_log="$1"
  local escaped_summary
  local summary

  summary="$(
    awk '
      function clean(line) {
        gsub(sprintf("%c", 27) "\\[[0-9;]*m", "", line)
        sub(/^.*\t[^\t]*\t[0-9TZ:._-]+ /, "", line)
        return line
      }

      function is_diagnostic(line) {
        return line ~ /^(error(\[[^]]+\])?:|warning:|note:|help:)/ ||
          line ~ /^[[:space:]]+-->/ ||
          line ~ /^[[:space:]]*[0-9]+[[:space:]]+\|/ ||
          line ~ /^[[:space:]]*\|/ ||
          line ~ /^[[:space:]]+= (note|help):/ ||
          line ~ /^[[:space:]]*\^[[:space:]^~-]*$/ ||
          line ~ /^For more information/ ||
          line ~ /^error: aborting/
      }

      {
        line = clean($0)
      }

      line ~ /^ERROR: .* failed:/ {
        if (printed) {
          print ""
        }
        print line
        in_failure = 1
        seen_diagnostic = 0
        printed = 1
        next
      }

      in_failure && is_diagnostic(line) {
        print line
        seen_diagnostic = 1
        next
      }

      in_failure && seen_diagnostic && line == "" {
        print ""
        next
      }

      in_failure && seen_diagnostic {
        in_failure = 0
        seen_diagnostic = 0
        next
      }
    ' "$console_log"
  )"

  if [[ -z "$summary" ]]; then
    summary="$(grep -E '^ERROR: |^FAILED: ' "$console_log" | tail -n 50 || true)"
  fi

  if [[ -z "$summary" ]]; then
    echo "No Bazel action failures were found in the captured console output."
    return
  fi

  if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
    escaped_summary="$(
      printf '%s' "$summary" \
        | awk 'BEGIN { ORS = "" } {
            gsub(/%/, "%25")
            gsub(/\r/, "%0D")
            print sep $0
            sep = "%0A"
          }'
    )"
    echo "::error title=Bazel failed action diagnostics::${escaped_summary}"
  fi

  echo
  echo "Bazel failed action diagnostics:"
  echo "--------------------------------"
  printf '%s\n' "$summary"
  echo "--------------------------------"
}

bazel_args=()
bazel_targets=()
found_target_separator=0
for arg in "$@"; do
  if [[ "$arg" == "--" && $found_target_separator -eq 0 ]]; then
    found_target_separator=1
    continue
  fi

  if [[ $found_target_separator -eq 0 ]]; then
    bazel_args+=("$arg")
  else
    bazel_targets+=("$arg")
  fi
done

if [[ ${#bazel_args[@]} -eq 0 || ${#bazel_targets[@]} -eq 0 ]]; then
  echo "Expected Bazel args and targets separated by --" >&2
  exit 1
fi

# Only the owned, bounded metadata route may replace repeated PATH values with
# named client bindings. Ordinary runtime/RBE calls keep their existing bytes.
metadata_path_mode=0
metadata_context=0
if [[ "${RUNNER_OS:-}" == Windows && "${GITHUB_ACTIONS:-}" == true &&
      "${GITHUB_REPOSITORY:-}" == CompleteDotTech/codex-subagents-multi-provider &&
      "${GITHUB_EVENT_NAME:-}" == workflow_dispatch && "${GITHUB_JOB:-}" == voice-runtime-diagnostic ]]; then
  metadata_context=1
fi
metadata_query=0
metadata_batch=0
metadata_command=""
for arg in "${bazel_args[@]}"; do
  if [[ -z "$metadata_command" && "$arg" == --batch ]]; then metadata_batch=$((metadata_batch + 1)); fi
  if [[ -z "$metadata_command" && "$arg" != -* ]]; then metadata_command="$arg"; fi
  if [[ "$arg" == --nobatch ]]; then metadata_batch=-100; fi
done
if [[ "$metadata_command" == cquery || "$metadata_command" == aquery ]]; then metadata_query=1; fi
metadata_private_root=0
if [[ -n "${VOICE_CONFIGURED_METADATA_MODE+x}" || ( $metadata_context -eq 1 && $metadata_query -eq 1 && $metadata_batch -eq 1 ) ]]; then
  metadata_root="$(cygpath -am -- "${BAZEL_OUTPUT_USER_ROOT:-}" 2>/dev/null)" || metadata_root=""
  metadata_temp="$(cygpath -am -- "${RUNNER_TEMP:-}" 2>/dev/null)" || metadata_temp=""
  metadata_root_lower="${metadata_root,,}"
  metadata_temp_lower="${metadata_temp,,}"
  metadata_temp_lower="${metadata_temp_lower%/}"
  metadata_leaf="${metadata_root_lower#"${metadata_temp_lower}/"}"
  if [[ -n "$metadata_temp_lower" && "$metadata_root_lower" == "${metadata_temp_lower}/"* &&
        "$metadata_leaf" == voice-metadata-bazel-?* && "$metadata_leaf" != */* &&
        -d "$metadata_root" && -d "$metadata_temp" ]]; then metadata_private_root=1; fi
fi
if [[ -z "${VOICE_CONFIGURED_METADATA_MODE+x}" ]]; then
  if [[ $metadata_context -eq 1 && $metadata_query -eq 1 && $metadata_batch -eq 1 && $metadata_private_root -eq 1 ]]; then
    echo "Configured metadata PATH mode is required." >&2
    exit 1
  fi
else
  if [[ "${VOICE_CONFIGURED_METADATA_MODE}" != bounded-v1 || $metadata_context -ne 1 ||
        $metadata_query -ne 1 || $metadata_batch -ne 1 || $metadata_private_root -ne 1 ||
        $windows_cross_compile -ne 1 || -n "${BUILDBUDDY_API_KEY:-}" ]]; then
    echo "Invalid configured metadata PATH context." >&2
    exit 1
  fi
  for metadata_sdk_name in PATH INCLUDE LIB LIBPATH UCRTVersion UniversalCRTSdkDir VCINSTALLDIR VCToolsInstallDir WindowsLibPath WindowsSdkBinPath WindowsSdkDir WindowsSDKLibVersion WindowsSDKVersion CODEX_BAZEL_WINDOWS_PATH; do
    if [[ -z "${!metadata_sdk_name:-}" ]]; then
      echo "Missing configured metadata SDK input." >&2
      exit 1
    fi
  done
  # Input PATH overrides are not caller-owned in this route. The exact three
  # named bindings below are added after SDK/native argument validation.
  for ((metadata_i = 0; metadata_i < ${#bazel_args[@]}; metadata_i++)); do
    case "${bazel_args[$metadata_i]}" in
      --action_env=*|--host_action_env=*|--test_env=*) metadata_env_value="${bazel_args[$metadata_i]#*=}" ;;
      --action_env|--host_action_env|--test_env) metadata_env_value="${bazel_args[$((metadata_i + 1))]:-}" ;;
      *) continue ;;
    esac
    metadata_env_value="${metadata_env_value#=}"
    metadata_env_name="${metadata_env_value%%=*}"
    if [[ "${metadata_env_name^^}" == PATH ]]; then
      echo "Configured metadata owns its named PATH bindings." >&2
      exit 1
    fi
  done
  metadata_path_mode=1
fi

windows_native_fallback=0
if [[ "${RUNNER_OS:-}" == "Windows" && $windows_cross_compile -eq 1 && -z "${BUILDBUDDY_API_KEY:-}" ]]; then
  # Without authenticated RBE this is a native MSVC build, not GNU cross coverage.
  ci_config=ci-windows
  windows_msvc_host_platform=1
  windows_native_fallback=1
fi

post_config_bazel_args=()
if [[ $windows_native_fallback -eq 1 && "${GITHUB_ACTIONS:-}" == true && "${GITHUB_REPOSITORY:-}" == CompleteDotTech/codex-subagents-multi-provider ]]; then
  # Native runtime receipts require the actual checkout's workspace status.
  # Bind the real producer explicitly; preserve an operator's nonempty choice.
  # Only Bazel options before our target/payload separator participate.
  requested_workspace_status=0
  for ((i = 0; i < ${#bazel_args[@]}; i++)); do
    case "${bazel_args[$i]}" in
      --workspace_status_command=*) status_value="${bazel_args[$i]#*=}" ;;
      --workspace_status_command)
        i=$((i + 1))
        status_value="${bazel_args[$i]:-}"
        ;;
      *) continue ;;
    esac
    if [[ -z "$status_value" || "$status_value" == -* ]]; then
      echo "Native Windows CI requires a nonempty --workspace_status_command producer for runtime receipt provenance." >&2
      exit 1
    fi
    requested_workspace_status=1
  done
  if [[ $requested_workspace_status -eq 0 ]]; then
    post_config_bazel_args+=(--workspace_status_command=scripts/workspace-status.cmd)
  fi
fi
if [[ $windows_native_fallback -eq 1 ]]; then
  requested_target_platform=""
  requested_host_platform=""
  for ((i = 0; i < ${#bazel_args[@]}; i++)); do
    case "${bazel_args[$i]}" in
      --platforms=* | --host_platform=*)
        platform_option="${bazel_args[$i]%%=*}"
        platform_value="${bazel_args[$i]#*=}"
        ;;
      --platforms | --host_platform)
        platform_option="${bazel_args[$i]}"
        if ((i + 1 >= ${#bazel_args[@]})); then
          echo "${platform_option} requires a platform value." >&2
          exit 1
        fi
        i=$((i + 1))
        platform_value="${bazel_args[$i]}"
        ;;
      *) continue ;;
    esac
    case "$platform_value" in
      //:local_windows_msvc | //:windows_x86_64_msvc) ;;
      *)
        echo "Windows without authenticated RBE supports only the native MSVC fallback; unsupported ${platform_option}=${platform_value}." >&2
        exit 1
        ;;
    esac
    if [[ "$platform_option" == --platforms ]]; then
      requested_target_platform="$platform_value"
    else
      requested_host_platform="$platform_value"
    fi
  done
  for required_input in VOICE_WINDOWS_SYSTEM_ROOT VOICE_WINDOWS_HOST_ARCH VOICE_WINDOWS_BAZEL_REPOSITORY; do
    if [[ -z "${!required_input:-}" ]]; then
      echo "Native Windows fallback requires ${required_input} from the pinned Windows build-input setup." >&2
      exit 1
    fi
  done
  actual_system_root="${SystemRoot:-${SYSTEMROOT:-}}"
  actual_host_arch="${PROCESSOR_ARCHITEW6432:-${PROCESSOR_ARCHITECTURE:-}}"
  if [[ -z "$actual_system_root" || "$VOICE_WINDOWS_SYSTEM_ROOT" != "$actual_system_root" || "$actual_host_arch" != AMD64 || "$VOICE_WINDOWS_HOST_ARCH" != "$actual_host_arch" ]]; then
    echo "Native Windows fallback inputs must match the actual SystemRoot and AMD64 host architecture." >&2
    exit 1
  fi
  if [[ ! -f "$VOICE_WINDOWS_BAZEL_REPOSITORY/voice-tools.json" || ! -d "$VOICE_WINDOWS_BAZEL_REPOSITORY/cygwin" || ! -d "$VOICE_WINDOWS_BAZEL_REPOSITORY/pkgconf-image" ]]; then
    echo "Native Windows fallback requires the complete provisioned Cygwin/pkgconf repository." >&2
    exit 1
  fi
  for ((i = 0; i < ${#bazel_args[@]}; i++)); do
    case "${bazel_args[$i]}" in
      --action_env=* | --host_action_env=* | --inject_repository=* | --//third_party/voice:windows_installed_tools=*)
        native_option="${bazel_args[$i]%%=*}"
        native_value="${bazel_args[$i]#*=}"
        ;;
      --action_env | --host_action_env | --inject_repository | --//third_party/voice:windows_installed_tools)
        native_option="${bazel_args[$i]}"
        native_value="${bazel_args[$((i + 1))]:-}"
        ;;
      *) continue ;;
    esac
    native_input_override=0
    case "$native_option" in
      --action_env | --host_action_env)
        native_env_name="${native_value%%=*}"
        case "${native_env_name^^}" in
          SYSTEMROOT | PROCESSOR_ARCHITECTURE) native_input_override=1 ;;
        esac
        ;;
      --inject_repository)
        case "$native_value" in
          voice_windows_tools=*) native_input_override=1 ;;
        esac
        ;;
      --//third_party/voice:windows_installed_tools) native_input_override=1 ;;
    esac
    if [[ $native_input_override -eq 1 ]]; then
      echo "Native Windows fallback owns ${native_option} for its verified build inputs; use the pinned setup's VOICE_WINDOWS values instead of overriding them." >&2
      exit 1
    fi
  done
  # An MSVC Rust platform alone still selects hermetic GNU C/C++ by default.
  # Reuse the native SDK toolchain recipe for both target and host dependencies.
  post_config_bazel_args+=(
    --config=ci-windows-msvc
    --extra_toolchains=//third_party/voice:windows_pkg_config_toolchain,//third_party/voice:windows_cmake_toolchain
    "--inject_repository=voice_windows_tools=${VOICE_WINDOWS_BAZEL_REPOSITORY}"
    --//third_party/voice:windows_installed_tools=@voice_windows_tools//:tools
    "--action_env=SystemRoot=${VOICE_WINDOWS_SYSTEM_ROOT}"
    "--host_action_env=SystemRoot=${VOICE_WINDOWS_SYSTEM_ROOT}"
    "--action_env=PROCESSOR_ARCHITECTURE=${VOICE_WINDOWS_HOST_ARCH}"
    "--host_action_env=PROCESSOR_ARCHITECTURE=${VOICE_WINDOWS_HOST_ARCH}"
  )
  if [[ -z "$requested_target_platform" ]]; then
    post_config_bazel_args+=(--platforms=//:windows_x86_64_msvc)
  fi
  if [[ -z "$requested_host_platform" ]]; then
    post_config_bazel_args+=(--host_platform=//:local_windows_msvc)
  fi
  echo "Windows fallback: native x86_64-pc-windows-msvc target and SDK; GNU cross compilation is not exercised."
fi

if [[ "${RUNNER_OS:-}" == "Windows" && $windows_msvc_host_platform -eq 1 ]]; then
  has_host_platform_override=0
  for arg in "${bazel_args[@]}"; do
    if [[ "$arg" == --host_platform=* ]]; then
      has_host_platform_override=1
      break
    fi
  done

  if [[ $has_host_platform_override -eq 0 && $windows_native_fallback -eq 0 ]]; then
    # Use the MSVC Windows platform for jobs that need helper binaries like
    # Rust test wrappers and V8 generators to resolve a compatible toolchain.
    # Callers that need a different Windows target platform should pass an
    # explicit `--platforms=...` flag.
    post_config_bazel_args+=("--host_platform=//:local_windows_msvc")
  fi
fi

if [[ "${RUNNER_OS:-}" == "Windows" && "${GITHUB_ACTIONS:-}" == "true" && "${GITHUB_REPOSITORY:-}" == "CompleteDotTech/codex-subagents-multi-provider" ]]; then
  # Authorized for this fork's Windows CI only, never a machine-wide declaration.
  # An explicit caller declaration (including an opt-out) remains authoritative.
  has_msvc_license_declaration=0
  for ((i = 0; i < ${#bazel_args[@]}; i++)); do
    case "${bazel_args[$i]}" in
      --repo_env=BAZEL_MSVC_RUNTIME_VISUAL_STUDIO_EULA | --repo_env=BAZEL_MSVC_RUNTIME_VISUAL_STUDIO_EULA=*)
        has_msvc_license_declaration=1
        ;;
      --repo_env)
        case "${bazel_args[$((i + 1))]:-}" in
          BAZEL_MSVC_RUNTIME_VISUAL_STUDIO_EULA | BAZEL_MSVC_RUNTIME_VISUAL_STUDIO_EULA=*)
            has_msvc_license_declaration=1
            ;;
        esac
        ;;
    esac
  done
  if [[ $has_msvc_license_declaration -eq 0 ]]; then
    post_config_bazel_args+=(--repo_env=BAZEL_MSVC_RUNTIME_VISUAL_STUDIO_EULA=1)
  fi
fi

if [[ $remote_download_toplevel -eq 1 ]]; then
  # Override the CI config's remote_download_minimal setting when callers need
  # the built artifact to exist on disk after the command completes.
  post_config_bazel_args+=(--remote_download_toplevel)
fi

if [[ "${RUNNER_OS:-}" == "Windows" && $windows_cross_compile -eq 1 && -n "${BUILDBUDDY_API_KEY:-}" ]]; then
  # `--enable_platform_specific_config` expands `common:windows` on Windows
  # hosts after ordinary rc configs, which can override `ci-windows-cross`'s
  # RBE host platform. Repeat the host platform on the command line so V8 and
  # other genrules execute on Linux RBE workers instead of Git Bash locally.
  #
  # Bazel also derives the default genrule shell from the client host. Without
  # an explicit shell executable, remote Linux actions can be asked to run
  # `C:\Program Files\Git\usr\bin\bash.exe`.
  post_config_bazel_args+=(--host_platform=//:rbe --shell_executable=/bin/bash)
fi

if [[ "${RUNNER_OS:-}" == "Windows" && $windows_cross_compile -eq 1 && -z "${BUILDBUDDY_API_KEY:-}" ]]; then
  # The Windows cross-compile config depends on authenticated remote
  # execution. When credentials are unavailable, keep the local build shape
  # and its lower concurrency cap.
  post_config_bazel_args+=(--jobs=8)
fi

if [[ -n "${BAZEL_REPO_CONTENTS_CACHE:-}" ]]; then
  # Windows self-hosted runners can run multiple Bazel jobs concurrently. Give
  # each job its own repo contents cache so they do not fight over the shared
  # path configured in `ci-windows`.
  post_config_bazel_args+=("--repo_contents_cache=${BAZEL_REPO_CONTENTS_CACHE}")
fi

if [[ -n "${BAZEL_REPOSITORY_CACHE:-}" ]]; then
  post_config_bazel_args+=("--repository_cache=${BAZEL_REPOSITORY_CACHE}")
fi

if [[ -n "${CODEX_BAZEL_EXECUTION_LOG_COMPACT_DIR:-}" ]]; then
  post_config_bazel_args+=(
    "--execution_log_compact_file=${CODEX_BAZEL_EXECUTION_LOG_COMPACT_DIR}/execution-log-${bazel_args[0]}-${GITHUB_JOB:-local}-$$.zst"
  )
fi

if [[ "${RUNNER_OS:-}" == "Windows" ]]; then
  pass_windows_build_env=1
  if [[ $windows_cross_compile -eq 1 && -n "${BUILDBUDDY_API_KEY:-}" ]]; then
    # Remote build actions execute on Linux RBE workers. Passing the Windows
    # runner's build environment there makes Bazel genrules try to execute
    # C:\Program Files\Git\usr\bin\bash.exe on Linux.
    pass_windows_build_env=0
  fi

  if [[ $pass_windows_build_env -eq 1 ]]; then
    windows_action_env_vars=(
      INCLUDE
      LIB
      LIBPATH
      UCRTVersion
      UniversalCRTSdkDir
      VCINSTALLDIR
      VCToolsInstallDir
      WindowsLibPath
      WindowsSdkBinPath
      WindowsSdkDir
      WindowsSDKLibVersion
      WindowsSDKVersion
    )

    for env_var in "${windows_action_env_vars[@]}"; do
      if [[ -n "${!env_var:-}" ]]; then
        post_config_bazel_args+=("--action_env=${env_var}" "--host_action_env=${env_var}")
      fi
    done
  fi

  if [[ -z "${CODEX_BAZEL_WINDOWS_PATH:-}" ]]; then
    echo "CODEX_BAZEL_WINDOWS_PATH must be set for Windows Bazel CI." >&2
    exit 1
  fi

  if [[ $metadata_path_mode -eq 1 ]]; then
    post_config_bazel_args+=(--action_env=PATH --host_action_env=PATH)
  elif [[ $pass_windows_build_env -eq 1 ]]; then
    post_config_bazel_args+=(
      "--action_env=PATH=${CODEX_BAZEL_WINDOWS_PATH}"
      "--host_action_env=PATH=${CODEX_BAZEL_WINDOWS_PATH}"
    )
  elif [[ $windows_cross_compile -eq 1 ]]; then
    # Remote build actions run on Linux RBE workers. Give their shell snippets
    # a Linux PATH while preserving CODEX_BAZEL_WINDOWS_PATH below for local
    # Windows test execution.
    post_config_bazel_args+=(
      "--action_env=PATH=/usr/bin:/bin"
      "--host_action_env=PATH=/usr/bin:/bin"
    )
  fi
  if [[ $metadata_path_mode -eq 1 ]]; then
    post_config_bazel_args+=(--test_env=PATH)
  else
    post_config_bazel_args+=("--test_env=PATH=${CODEX_BAZEL_WINDOWS_PATH}")
  fi
fi

bazel_console_log="$(mktemp)"
trap 'rm -f "$bazel_console_log"' EXIT

bazel_run_args=(
  "${bazel_args[@]}"
)
if [[ -n "${BUILDBUDDY_API_KEY:-}" ]]; then
  echo "BuildBuddy API key is available; using remote Bazel configuration."
  bazel_run_args+=("--config=${ci_config}")
else
  echo "BuildBuddy API key is not available; using local Bazel configuration."
fi
if (( ${#post_config_bazel_args[@]} > 0 )); then
  bazel_run_args+=("${post_config_bazel_args[@]}")
fi
set +e
# Work around Bazel 9 remote repo contents cache / overlay materialization
# failures seen in CI (for example "is not a symlink" or permission errors
# while materializing external repos such as rules_perl). This only disables
# the startup-level repo contents cache; keyed runs still use BuildBuddy.
run_bazel_with_startup_args \
  --noexperimental_remote_repo_contents_cache \
  "${bazel_run_args[@]}" \
  -- \
  "${bazel_targets[@]}" \
  2>&1 | tee "$bazel_console_log"
bazel_status=${PIPESTATUS[0]}
set -e

if [[ ${bazel_status:-0} -ne 0 ]]; then
  # Observe the failed build outputs before any diagnostic Bazel info call.
  if [[ "${RUNNER_OS:-}" == Windows && "${GITHUB_ACTIONS:-}" == true && "${GITHUB_REPOSITORY:-}" == CompleteDotTech/codex-subagents-multi-provider && -n "${CODEX_BAZEL_EXECUTION_LOG_COMPACT_DIR:-}" ]]; then
    python "$(dirname "${BASH_SOURCE[0]}")/capture-windows-workspace-provenance.py" "${bazel_run_args[@]}" || echo "Workspace provenance capture failed; original Bazel failure retained." >&2
  fi
  if [[ $print_failed_bazel_action_summary -eq 1 ]]; then
    print_bazel_action_failure_summary "$bazel_console_log"
  fi
  if [[ $print_failed_bazel_test_logs -eq 1 ]]; then
    print_bazel_test_log_tails "$bazel_console_log"
  fi
  exit "$bazel_status"
fi
