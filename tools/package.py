#!/usr/bin/env python3
"""Build a deterministic source archive and optionally render the binary Formula."""

import argparse
import gzip
import hashlib
from pathlib import Path
import re
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
FILES = (
    "Cargo.toml", "Cargo.lock", "README.md", "README.zh-TW.md",
    "LICENSE", "CHANGELOG.md", "assets/scpto.svg",
)
TARGETS = {
    "X86_64_LINUX": "x86_64-unknown-linux-musl",
    "AARCH64_LINUX": "aarch64-unknown-linux-musl",
    "X86_64_MACOS": "x86_64-apple-darwin",
    "AARCH64_MACOS": "aarch64-apple-darwin",
}


def require_regular(path):
    """Reject missing inputs, symbolic links, directories and special files."""
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"Required regular file is missing or invalid: {path}")


def write_archive(destination, files):
    """Write sorted regular files with fixed tar and gzip metadata."""
    files = sorted(files, key=lambda item: item[1])
    for path, _, _ in files:
        require_regular(path)
    with destination.open("xb") as output:
        with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as tar:
                for path, name, mode in files:
                    info = tarfile.TarInfo(name)
                    info.size = path.stat().st_size
                    info.mode = mode
                    with path.open("rb") as source:
                        tar.addfile(info, source)


def sha256(path):
    """Hash a regular release asset without loading it entirely into memory."""
    require_regular(path)
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def main():
    """Validate release inputs, package source and emit checksums and a Formula."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="Output directory; must not already exist")
    parser.add_argument("--binary-dir", type=Path, help="Directory containing exactly four release binary archives")
    args = parser.parse_args()
    try:
        names = list(FILES)
        sources = sorted(ROOT.glob("src/**/*.rs"))
        if not sources:
            raise ValueError("No Rust source files matched src/**/*.rs")
        names += [path.relative_to(ROOT).as_posix() for path in sources]
        for name in names:
            require_regular(ROOT / name)
        with (ROOT / "Cargo.toml").open("rb") as manifest:
            version = tomllib.load(manifest)["package"]["version"]
        if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version):
            raise ValueError(f"Invalid release version: {version!r}")

        values = {"VERSION": version}
        checksums = {}
        if args.binary_dir is not None:
            expected = {f"scpto-{version}-{target}.tar.gz" for target in TARGETS.values()}
            actual = {path.name for path in args.binary_dir.iterdir()}
            if actual != expected:
                raise ValueError(
                    f"Expected exactly four binary archives; missing={sorted(expected - actual)}, "
                    f"unexpected={sorted(actual - expected)}"
                )
            for key, target in TARGETS.items():
                name = f"scpto-{version}-{target}.tar.gz"
                digest = sha256(args.binary_dir / name)
                checksums[name] = digest
                values[f"SHA256_{key}"] = digest

        formula_path = ROOT / "packaging/scpto.rb.in"
        require_regular(formula_path)
        formula = formula_path.read_text(encoding="utf-8")
        for key, value in values.items():
            formula = formula.replace(f"@@{key}@@", value)
        if args.binary_dir is not None and "@@" in formula:
            raise ValueError("Unresolved Formula placeholders")

        args.output.mkdir(parents=True, exist_ok=False)
        archive = args.output / f"scpto-{version}.tar.gz"
        write_archive(archive, [(ROOT / name, f"scpto-{version}/{name}", 0o644) for name in names])
        checksums[archive.name] = sha256(archive)
        (args.output / "SHA256SUMS").write_text(
            "".join(f"{checksums[name]}  {name}\n" for name in sorted(checksums)), encoding="utf-8"
        )
        (args.output / "Formula").mkdir()
        suffix = "rb" if args.binary_dir is not None else "rb.in"
        (args.output / "Formula" / f"scpto.{suffix}").write_text(formula, encoding="utf-8")
    except (OSError, ValueError) as error:
        parser.exit(1, f"Packaging failed: {error}\n")
    print(archive)
    print(f"SHA256 {checksums[archive.name]}")
    if args.binary_dir is None:
        print("No binary archives supplied: generated a Formula template, not an installable Formula.")


if __name__ == "__main__":
    main()
