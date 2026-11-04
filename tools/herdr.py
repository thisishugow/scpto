#!/usr/bin/env python3
"""Prepare local Herdr desktop-mod and terminal-plugin packages."""

import argparse
from pathlib import Path
import re
import shutil
import tomllib

ROOT = Path(__file__).resolve().parents[1]
DESKTOP_FILES = ("main.js", "session.js", "mod.json")


def require_regular(path):
    """Reject missing inputs, symlinks, directories and special files."""
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"Required regular file is missing or invalid: {path}")


def prepare(output):
    """Validate inputs before creating a new package directory."""
    manifest = ROOT / "Cargo.toml"
    template = ROOT / "integrations/herdr/tui/herdr-plugin.toml.in"
    launcher = ROOT / "integrations/herdr/tui/launch.py"
    desktop = ROOT / "integrations/herdr/desktop/scpto"
    license_file = ROOT / "LICENSE"
    sources = [manifest, template, launcher, license_file, *(desktop / name for name in DESKTOP_FILES)]
    for source in sources:
        require_regular(source)
    with manifest.open("rb") as source:
        version = tomllib.load(source)["package"]["version"]
    if not isinstance(version, str) or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version):
        raise ValueError(f"Invalid release version: {version!r}")
    rendered = template.read_text(encoding="utf-8").replace("@@VERSION@@", version)
    if "@@" in rendered:
        raise ValueError("Unresolved plugin manifest placeholders")
    tomllib.loads(rendered)
    output.mkdir(parents=True, exist_ok=False)
    (output / "tui").mkdir()
    (output / "desktop/scpto").mkdir(parents=True)
    (output / "tui/herdr-plugin.toml").write_text(rendered, encoding="utf-8")
    shutil.copyfile(launcher, output / "tui/launch.py")
    shutil.copyfile(license_file, output / "tui/LICENSE")
    shutil.copyfile(license_file, output / "desktop/scpto/LICENSE")
    for name in DESKTOP_FILES:
        shutil.copyfile(desktop / name, output / "desktop/scpto" / name)


def main():
    """Prepare packages without installing, building or downloading anything."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="Output directory; must not already exist")
    args = parser.parse_args()
    try:
        prepare(args.output)
    except (OSError, ValueError, KeyError) as error:
        parser.exit(1, f"Herdr preparation failed: {error}\n")
    print(f"Desktop mod: {args.output / 'desktop/scpto'}")
    print(f"Terminal plugin: {args.output / 'tui'}")
    print("These packages use the scpto executable already installed on your PATH.")


if __name__ == "__main__":
    main()
