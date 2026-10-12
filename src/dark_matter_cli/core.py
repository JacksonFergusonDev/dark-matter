import logging
import math
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import pandas as pd

from dark_matter_cli import homebrew

logger = logging.getLogger("dark_matter_cli")

INSTALLED = "local installation"
CATALOG = "catalog"


class PackageNotFoundError(ValueError):
    """Raised when requested packages are absent from the analyzed graph.

    Attributes:
        packages: The requested names that were not found.
        location: Where they were looked up (INSTALLED or CATALOG).
    """

    def __init__(self, packages: list[str], location: str) -> None:
        self.packages = packages
        self.location = location
        noun = "Package" if len(packages) == 1 else "Packages"
        names = ", ".join(f"'{p}'" for p in packages)
        super().__init__(f"{noun} {names} not found in the {location}.")


def _require_targets(
    targets: list[str], graph: dict[str, list[str]], location: str
) -> None:
    """Raise PackageNotFoundError listing every target missing from `graph`."""
    missing = [t for t in targets if t not in graph]
    if missing:
        raise PackageNotFoundError(missing, location)


@dataclass(frozen=True)
class Topology:
    """Immutable DAG properties shared by every analysis mode.

    Attributes:
        graph: Each package's direct dependencies.
        trans_deps: Each package's transitive dependencies.
        roots: Packages that bytes are attributed to: everything requested
            explicitly, plus any package nothing else depends on.
        requested: Packages the user asked for (formulae installed on
            request, and every cask). Empty in catalog mode.
        sharer_count: For each non-root dependency, how many roots need it.
        pkg_data: Each package's raw metadata.
    """

    graph: dict[str, list[str]]
    trans_deps: dict[str, set[str]]
    roots: frozenset[str]
    requested: frozenset[str]
    sharer_count: dict[str, int]
    pkg_data: dict[str, Any]


def _is_requested(pkg: dict[str, Any]) -> bool:
    """Whether the user installed this package explicitly."""
    if "token" in pkg:
        return True
    return any(i.get("installed_on_request") for i in pkg.get("installed", []))


def _identify_roots(
    graph: dict[str, list[str]], pkg_data_map: dict[str, Any], catalog: bool
) -> tuple[frozenset[str], frozenset[str]]:
    """Return (roots, requested) for the graph.

    Packages with no dependents are always roots, which guarantees every
    package is reachable from at least one root. In installed mode, packages
    the user requested are roots too, even when something depends on them.
    """
    has_dependents = {dep for deps in graph.values() for dep in deps}
    unreferenced = {pkg for pkg in graph if pkg not in has_dependents}
    if catalog:
        return frozenset(unreferenced), frozenset()

    requested = frozenset(
        pkg for pkg, data in pkg_data_map.items() if _is_requested(data)
    )
    return frozenset(unreferenced | requested), requested


def _build_global_topology(metadata: dict[str, Any], catalog: bool) -> Topology:
    """Parse metadata and pre-compute the immutable DAG properties."""
    dependency_graph: dict[str, list[str]] = {}
    pkg_data_map: dict[str, Any] = {}

    for formula in metadata.get("formulae", []):
        name = formula["name"]
        dependency_graph[name] = list(formula.get("dependencies", []))
        pkg_data_map[name] = formula

    for cask in metadata.get("casks", []):
        token = cask["token"]
        # Formulae and casks share a namespace in the graph; disambiguate
        # collisions (e.g. the `docker` formula vs. the `docker` cask).
        key = f"{token} (cask)" if token in dependency_graph else token
        depends_on = cask.get("depends_on", {})
        dependency_graph[key] = list(depends_on.get("formula", [])) + list(
            depends_on.get("cask", [])
        )
        pkg_data_map[key] = cask

    logger.debug(
        f"Dependency graph: {len(dependency_graph)} packages "
        f"({len(metadata.get('casks', []))} casks)"
    )

    package_transitive_deps = {
        pkg: get_all_dependencies(pkg, dependency_graph) for pkg in dependency_graph
    }
    roots, requested = _identify_roots(dependency_graph, pkg_data_map, catalog)

    # Only roots share costs: counting intermediate dependencies as sharers
    # would hand part of each shared package to rows that are never summed.
    sharer_count: dict[str, int] = {}
    for root in roots:
        for dep in package_transitive_deps[root]:
            if dep not in roots:
                sharer_count[dep] = sharer_count.get(dep, 0) + 1

    logger.debug(f"Roots: {len(roots)} ({len(requested)} requested)")

    return Topology(
        graph=dependency_graph,
        trans_deps=package_transitive_deps,
        roots=roots,
        requested=requested,
        sharer_count=sharer_count,
        pkg_data=pkg_data_map,
    )


def _resolve_physical_sizes(
    pkg_data_map: dict[str, Any], prefix: Path
) -> dict[str, int]:
    """Calculate disk footprint for installed packages."""
    size_map = {}
    for name, pkg in pkg_data_map.items():
        if "token" in pkg:
            size_map[name] = homebrew.get_cask_size(pkg, prefix)
        else:
            size_map[name] = homebrew.get_directory_size(prefix / "Cellar" / name)
    return size_map


def _select_bottle_file(files: dict[str, Any], tag: str) -> Any:
    """Pick the bottle matching `tag`, falling back like Homebrew would.

    Preference order: the exact tag, an arch-independent 'all' bottle, then
    the newest bottle for the same CPU and OS family. Bottles for a different
    CPU or OS are never substituted.
    """
    if tag in files:
        return files[tag]
    if "all" in files:
        return files["all"]

    def family(t: str) -> tuple[bool, bool]:
        return (t.startswith("arm64_"), t.endswith("_linux"))

    # Homebrew lists bottles newest-first.
    return next((v for k, v in files.items() if family(k) == family(tag)), None)


def _resolve_theoretical_sizes(
    resolution_set: set[str], pkg_data_map: dict[str, Any], arch: str | None
) -> dict[str, int]:
    """Resolve compressed bottle sizes via ghcr.io for a specific subset of packages."""
    tag = arch or homebrew.detect_bottle_tag()
    package_digest = {}
    bottles = {}

    for name in resolution_set:
        pkg = pkg_data_map.get(name, {})
        files = pkg.get("bottle", {}).get("stable", {}).get("files", {})
        file_info = _select_bottle_file(files, tag)

        if file_info is not None:
            digest = file_info.get("sha256", "")
            url = file_info.get("url", "")
            if digest and url:
                package_digest[name] = digest
                bottles[digest] = url

    cache = homebrew.load_bottle_size_cache()
    resolved_sizes = homebrew.resolve_bottle_sizes(bottles, cache)
    homebrew.save_bottle_size_cache(cache)

    return {
        name: resolved_sizes.get(digest, 0) for name, digest in package_digest.items()
    }


def _attributed_size(dep: str, size_map: dict[str, int], topo: Topology) -> float:
    """The share of `dep` charged to each root that needs it.

    Roots are charged nothing: they would be installed regardless.
    """
    if dep in topo.roots:
        return 0.0
    return size_map.get(dep, 0) / topo.sharer_count.get(dep, 1)


def _compute_bloat_metrics(
    targets: list[str], size_map: dict[str, int], topo: Topology
) -> pd.DataFrame:
    """Execute the fractional attribution math and compile the DataFrame."""
    results = []
    for target in targets:
        core_size = size_map.get(target, 0)
        target_deps = topo.trans_deps.get(target, set())

        standard_recursive_size = core_size + sum(
            size_map.get(d, 0) for d in target_deps
        )
        weighted_recursive_size = core_size + sum(
            _attributed_size(d, size_map, topo) for d in target_deps
        )
        # A package with no measurable core has no meaningful ratio.
        ratio = weighted_recursive_size / core_size if core_size > 0 else math.nan

        results.append(
            {
                "Package": target,
                "Core_Bytes": core_size,
                "Standard_Bytes": standard_recursive_size,
                "Weighted_Bytes": weighted_recursive_size,
                "Bloat_Ratio": ratio,
                "Dep_Count": len(target_deps),
                "Is_Root": target in topo.roots,
                "Is_Requested": target in topo.requested,
            }
        )

    return pd.DataFrame(results)


def _compute_explain_metrics(
    target: str, size_map: dict[str, int], topo: Topology, is_theoretical: bool
) -> pd.DataFrame:
    """Compile the fractional dependency cost breakdown for a target package."""
    target_deps = topo.trans_deps.get(target, set())
    results = []

    for dep in target_deps:
        dep_size = size_map.get(dep, 0)
        is_root = dep in topo.roots

        row = {
            "Dependency": dep,
            "Shared_By": 0 if is_root else topo.sharer_count.get(dep, 1),
            "Attributed_Bytes": _attributed_size(dep, size_map, topo),
            "Is_Root": is_root,
        }

        if is_theoretical:
            row["Archive_Bytes"] = dep_size
        else:
            row["Core_Bytes"] = dep_size

        results.append(row)

    df = pd.DataFrame(results)
    if not df.empty:
        # Break ties by name so output is stable across runs (set iteration
        # order depends on string hash randomization).
        df = df.sort_values(
            by=["Attributed_Bytes", "Dependency"], ascending=[False, True]
        )

    return df


def _log_conservation(df: pd.DataFrame, size_map: dict[str, int]) -> None:
    """Log whether the roots' weighted sizes account for every byte."""
    total = sum(size_map.values())
    attributed = df.loc[df["Is_Root"], "Weighted_Bytes"].sum() if not df.empty else 0
    logger.debug(
        f"Attribution: {total} bytes measured, {attributed:.0f} attributed to "
        f"roots (difference {total - attributed:.0f})"
    )


def get_all_dependencies(
    pkg: str, graph: dict[str, list[str]], visited: set[str] | None = None
) -> set[str]:
    """Recursively traverse the DAG to extract all transitive dependencies.

    Args:
        pkg: The target package identifier.
        graph: The complete directed acyclic graph of explicitly declared dependencies.
        visited: The mathematical set of dependencies already encountered.

    Returns:
        set[str]: A set containing all deeply nested transitive dependencies.
    """
    if visited is None:
        visited = set()

    if pkg not in graph:
        return visited

    for dep in graph[pkg]:
        if dep not in visited:
            visited.add(dep)
            get_all_dependencies(dep, graph, visited)

    return visited


def build_analysis_dataframe(metadata: dict[str, Any], prefix: Path) -> pd.DataFrame:
    """Construct the primary DataFrame utilizing physical disk metrics."""
    topo = _build_global_topology(metadata, catalog=False)
    size_map = _resolve_physical_sizes(topo.pkg_data, prefix)

    df = _compute_bloat_metrics(list(topo.graph), size_map, topo)
    _log_conservation(df, size_map)
    return df


def build_targeted_analysis_dataframe(
    metadata: dict[str, Any], prefix: Path, target: str
) -> pd.DataFrame:
    """Constructs a DataFrame for a single installed target."""
    topo = _build_global_topology(metadata, catalog=False)

    _require_targets([target], topo.graph, INSTALLED)

    size_map = _resolve_physical_sizes(topo.pkg_data, prefix)
    return _compute_bloat_metrics([target], size_map, topo)


def build_compare_analysis_dataframe(
    metadata: dict[str, Any], prefix: Path, targets: list[str]
) -> pd.DataFrame:
    """Constructs a DataFrame for comparing multiple installed targets."""
    topo = _build_global_topology(metadata, catalog=False)

    _require_targets(targets, topo.graph, INSTALLED)

    size_map = _resolve_physical_sizes(topo.pkg_data, prefix)
    return _compute_bloat_metrics(targets, size_map, topo)


def build_explain_analysis_dataframe(
    metadata: dict[str, Any], prefix: Path, target: str
) -> pd.DataFrame:
    """Constructs a dependency breakdown DataFrame for a physical target."""
    topo = _build_global_topology(metadata, catalog=False)

    _require_targets([target], topo.graph, INSTALLED)

    size_map = _resolve_physical_sizes(topo.pkg_data, prefix)
    return _compute_explain_metrics(target, size_map, topo, is_theoretical=False)


def build_theoretical_dataframe(
    metadata: dict[str, Any], arch: str | None = None
) -> pd.DataFrame:
    """Constructs the ecosystem-wide theoretical leaderboard."""
    topo = _build_global_topology(metadata, catalog=True)
    size_map = _resolve_theoretical_sizes(set(topo.graph), topo.pkg_data, arch)

    df = _compute_bloat_metrics(list(topo.graph), size_map, topo)
    _log_conservation(df, size_map)
    return df


def build_targeted_theoretical_dataframe(
    metadata: dict[str, Any], target: str, arch: str | None = None
) -> pd.DataFrame:
    """Constructs a DataFrame for a single target, minimizing network resolution."""
    topo = _build_global_topology(metadata, catalog=True)

    _require_targets([target], topo.graph, CATALOG)

    # Isolate strictly to the target's closure to prevent O(N) network lookups
    resolution_set = {target} | topo.trans_deps[target]
    size_map = _resolve_theoretical_sizes(resolution_set, topo.pkg_data, arch)

    return _compute_bloat_metrics([target], size_map, topo)


def build_compare_theoretical_dataframe(
    metadata: dict[str, Any], targets: list[str], arch: str | None = None
) -> pd.DataFrame:
    """Constructs a DataFrame for multiple targets by resolving their unioned closure."""
    topo = _build_global_topology(metadata, catalog=True)

    _require_targets(targets, topo.graph, CATALOG)

    # Generate the union of all target transitive dependencies
    resolution_set = set(targets)
    for t in targets:
        resolution_set.update(topo.trans_deps[t])

    size_map = _resolve_theoretical_sizes(resolution_set, topo.pkg_data, arch)
    return _compute_bloat_metrics(targets, size_map, topo)


def build_explain_theoretical_dataframe(
    metadata: dict[str, Any], target: str, arch: str | None = None
) -> pd.DataFrame:
    """Constructs a dependency breakdown DataFrame using theoretical metrics."""
    topo = _build_global_topology(metadata, catalog=True)

    _require_targets([target], topo.graph, CATALOG)

    resolution_set = {target} | topo.trans_deps[target]
    size_map = _resolve_theoretical_sizes(resolution_set, topo.pkg_data, arch)

    return _compute_explain_metrics(target, size_map, topo, is_theoretical=True)
