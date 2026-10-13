"""Sanity-check an `export --format json` run against a real Homebrew install.

Usage: python scripts/smoke_check.py EXPORT_JSON PACKAGE DEPENDENCY

Exits non-zero, naming the failed check, if the export is malformed, PACKAGE
is not reported as a requested package depending on DEPENDENCY, or the
roots' weighted sizes don't add up to the bytes measured.
"""

import json
import math
import sys
from pathlib import Path
from typing import Any

REQUIRED_COLUMNS = {
    "Package",
    "Core_Bytes",
    "Standard_Bytes",
    "Weighted_Bytes",
    "Bloat_Ratio",
    "Dep_Count",
    "Is_Root",
    "Is_Requested",
}


def check(condition: bool, message: str) -> None:
    """Fail the smoke test with `message` unless `condition` holds."""
    if not condition:
        raise SystemExit(f"Smoke check failed: {message}")


def main(export_path: str, package: str, dependency: str) -> None:
    """Run every check against the export."""
    rows: list[dict[str, Any]] = json.loads(Path(export_path).read_text())
    check(bool(rows), "the export is empty")

    for row in rows:
        missing = REQUIRED_COLUMNS - row.keys()
        check(not missing, f"{row.get('Package')} is missing {sorted(missing)}")

    by_name = {row["Package"]: row for row in rows}
    check(package in by_name, f"{package} is not in the export")
    check(dependency in by_name, f"{dependency} is not in the export")

    target = by_name[package]
    check(target["Is_Requested"], f"{package} is not marked as requested")
    check(target["Is_Root"], f"{package} is not a root")
    check(target["Core_Bytes"] > 0, f"{package} measured 0 bytes")
    check(target["Dep_Count"] >= 1, f"{package} has no dependencies")
    check(
        target["Standard_Bytes"]
        >= target["Core_Bytes"] + by_name[dependency]["Core_Bytes"],
        f"{package}'s recursive size doesn't include {dependency}",
    )

    measured = sum(row["Core_Bytes"] for row in rows)
    attributed = sum(row["Weighted_Bytes"] for row in rows if row["Is_Root"])
    check(
        math.isclose(measured, attributed, rel_tol=1e-9),
        f"roots account for {attributed:.0f} of {measured} bytes",
    )


if __name__ == "__main__":
    if len(sys.argv) != 4:
        raise SystemExit(__doc__)
    main(*sys.argv[1:])
