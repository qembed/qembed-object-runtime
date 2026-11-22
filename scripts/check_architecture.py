#!/usr/bin/env python3
"""Enforce the workspace crate dependency boundaries."""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# Runtime depends on domain ports, never adapters. qembed-kmcp is an isolated
# administrative policy core and must not depend on runtime or persistence.
# qembed-api remains the sole application composition root.
ALLOWED = {
    "qembed-domain": set(),
    "qembed-kmcp": set(),
    "qembed-store": {"qembed-domain"},
    "qembed-llm": {"qembed-domain"},
    "qembed-tools": {"qembed-domain"},
    "qembed-runtime": {"qembed-domain"},
    "qembed-api": {
        "qembed-domain",
        "qembed-runtime",
        "qembed-store",
        "qembed-llm",
        "qembed-tools",
    },
}

FORBIDDEN_HANDLER_IMPORT = re.compile(
    r"(?:::)?\bqembed_(?:store|llm|tools)\b"
)


def fail(message: str) -> None:
    print(f"architecture check failed: {message}", file=sys.stderr)
    raise SystemExit(1)


def main() -> None:
    metadata = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    packages = json.loads(metadata.stdout)["packages"]
    actual: dict[str, set[str]] = {}

    for package in packages:
        name = package["name"]
        if name not in ALLOWED:
            continue
        actual[name] = {
            dependency["name"]
            for dependency in package["dependencies"]
            if dependency["name"].startswith("qembed-")
        }

    if set(actual) != set(ALLOWED):
        fail(f"workspace members differ: expected {sorted(ALLOWED)}, got {sorted(actual)}")

    for package, allowed in ALLOWED.items():
        unexpected = actual[package] - allowed
        missing = allowed - actual[package]
        if unexpected:
            fail(f"{package} has forbidden dependencies: {sorted(unexpected)}")
        if missing:
            fail(f"{package} is missing declared architecture edges: {sorted(missing)}")

    # Cargo rejects dependency cycles; this check also verifies the intended
    # direct dependency graph rather than relying on compilation alone.
    for handler in (ROOT / "crates/qembed-api/src/handlers").rglob("*.rs"):
        source = handler.read_text(encoding="utf-8")
        match = FORBIDDEN_HANDLER_IMPORT.search(source)
        if match:
            fail(f"{handler.relative_to(ROOT)} imports a concrete adapter ({match.group(0)})")

    composition = ROOT / "crates/qembed-api/src/composition.rs"
    if not composition.is_file():
        fail("qembed-api composition root is missing")

    print("architecture check passed")


if __name__ == "__main__":
    main()
