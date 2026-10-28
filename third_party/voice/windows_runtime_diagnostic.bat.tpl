@echo off
setlocal EnableExtensions EnableDelayedExpansion

rem Choose one existing runfiles source for both bootstrap and Python resolution.
set "diag_manifest="
set "diag_root="
if defined RUNFILES_MANIFEST_FILE if exist "%RUNFILES_MANIFEST_FILE%" (
  set "diag_manifest=%RUNFILES_MANIFEST_FILE%"
  set "diag_reason=existing_environment_manifest"
)
if not defined diag_manifest for %%M in ("%~f0.runfiles_manifest" "%~dpn0.runfiles_manifest" "%~f0.runfiles\MANIFEST" "%~dpn0.runfiles\MANIFEST") do (
  if not defined diag_manifest if exist "%%~M" (
    set "diag_manifest=%%~M"
    set "diag_reason=existing_adjacent_manifest"
  )
)
if not defined diag_manifest for %%R in ("%RUNFILES_DIR%" "%TEST_SRCDIR%") do (
  if not defined diag_manifest if not "%%~R"=="" if exist "%%~R\MANIFEST" (
    set "diag_manifest=%%~R\MANIFEST"
    set "diag_reason=existing_directory_manifest"
  )
)
if not defined diag_manifest for %%R in ("%RUNFILES_DIR%" "%TEST_SRCDIR%" "%~f0.runfiles" "%~dpn0.runfiles") do (
  if not defined diag_root if not "%%~R"=="" if exist "%%~R\." (
    set "diag_root=%%~R"
    set "diag_reason=existing_runfiles_directory"
  )
)
set "RUNFILES_MANIFEST_FILE=!diag_manifest!"
set "RUNFILES_DIR=!diag_root!"
set "TEST_SRCDIR=!diag_root!"
set "VOICE_DIAGNOSTIC_RUNFILES_REASON=!diag_reason!"
if not defined diag_manifest if not defined diag_root (
  >&2 echo voice diagnostic bootstrap: no existing manifest or runfiles directory
  exit /b 1
)

call :resolve "python" "__PYTHON__"
if errorlevel 1 exit /b 1
set "diag_python=!diag_result!"
call :resolve "driver" "__DRIVER__"
if errorlevel 1 exit /b 1
set "diag_driver=!diag_result!"
call :resolve "config" "__CONFIG__"
if errorlevel 1 exit /b 1
set "diag_config=!diag_result!"
"!diag_python!" "!diag_driver!" "!diag_config!"
exit /b !ERRORLEVEL!

:resolve
set "diag_result="
set "diag_role=%~1"
set "diag_key=%~2"
if defined diag_manifest (
  rem Read the file directly: nested type/findstr commands break on Windows CI.
  for /f "usebackq tokens=1,*" %%A in ("!diag_manifest!") do (
    if "%%A"=="!diag_key!" (
      set "diag_result=%%B"
      if not defined diag_result set "diag_result=%%A"
      goto :resolved
    )
  )
) else (
  set "diag_native_key=!diag_key:/=\!"
  set "diag_result=!diag_root!\!diag_native_key!"
)
:resolved
if not defined diag_result (
  >&2 echo voice diagnostic bootstrap: missing !diag_role! runfile !diag_key!
  exit /b 1
)
if not exist "!diag_result!" (
  >&2 echo voice diagnostic bootstrap: !diag_role! runfile does not exist: !diag_key!
  exit /b 1
)
exit /b 0
