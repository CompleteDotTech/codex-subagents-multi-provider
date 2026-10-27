"""Copy a declared Windows executable/DLL closure without modifying any bytes."""

import hashlib
import json
from pathlib import Path
import shutil
import sys


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def _basename(name):
    if (
        not name
        or any(character in name for character in '<>:"/\\|?*\0')
        or name.rstrip(" .") != name
        or name.split(".")[0].casefold() in {"con", "prn", "aux", "nul"}
    ):
        raise ValueError("invalid package basename")
    return name.casefold()


def package(config):
    root = Path(config["directory"])
    if root.is_symlink():
        raise ValueError("package directory cannot be a symlink")
    existing = (
        {path.name.casefold() for path in root.iterdir()} if root.exists() else set()
    )
    receipt = Path(config["receipt"])
    pairs = [config["executable"], *config["dlls"]]
    if not config["dlls"]:
        raise ValueError("declared DLL closure must not be empty")
    names = set()
    validated = []
    for index, pair in enumerate(pairs):
        source, output = Path(pair["source"]), Path(pair["output"])
        key = _basename(output.name)
        if key in names:
            raise ValueError("case-insensitive package basename collision")
        names.add(key)
        suffix = ".exe" if index == 0 else ".dll"
        if source.name != output.name or output.suffix.casefold() != suffix:
            raise ValueError("package must preserve executable and DLL basenames")
        if ".." in output.parts or output.parent.resolve() != root.resolve():
            raise ValueError("package output escapes its declared directory")
        if source.resolve() == output.resolve():
            raise ValueError("package source and destination must differ")
        if key in existing or output.exists() or output.is_symlink():
            raise ValueError("package output already exists")
        if not source.is_file():
            raise ValueError("declared package input is missing")
        validated.append((source, output, digest(source)))
    if ".." in receipt.parts or receipt.parent.resolve() != root.resolve():
        raise ValueError("package receipt escapes its declared directory")
    if _basename(receipt.name) in names:
        raise ValueError("package receipt collides with a payload")
    if receipt.name.casefold() in existing or receipt.exists() or receipt.is_symlink():
        raise ValueError("package receipt already exists")
    root.mkdir(parents=True, exist_ok=True)
    records = []
    for source, output, expected in validated:
        with source.open("rb") as reader, output.open("xb") as writer:
            shutil.copyfileobj(reader, writer, 1024 * 1024)
        if digest(output) != expected:
            raise ValueError("package byte checksum mismatch")
        records.append(
            {"name": output.name, "source_sha256": expected, "output_sha256": expected}
        )
    with receipt.open("x", encoding="utf-8") as stream:
        json.dump({"executable": records[0], "dlls": records[1:]}, stream, indent=2)
        stream.write("\n")


if __name__ == "__main__":
    package(json.loads(Path(sys.argv[1]).read_text(encoding="utf-8")))
