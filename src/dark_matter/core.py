from pathlib import Path
from typing import Any

import pandas as pd

from dark_matter import homebrew


def _build_global_topology(
    metadata: dict[str, Any],
) -> tuple[dict[str, list[str]], dict[str, set[str]], dict[str, int], dict[str, Any]]:
    """Parse metadata and pre-compute the immutable DAG properties."""
    packages = metadata.get("formulae", []) + metadata.get("casks", [])

    dependency_graph = {}
    pkg_data_map = {}

    for pkg in packages:
        name = pkg["name"][0] if isinstance(pkg["name"], list) else pkg["name"]
        dependency_graph[name] = pkg.get("dependencies", [])
        pkg_data_map[name] = pkg

    package_transitive_deps = {
        pkg: get_all_dependencies(pkg, dependency_graph) for pkg in dependency_graph
    }

    # Initialize with 0 for known packages, but handle missing edge nodes dynamically
    dependency_parent_count = dict.fromkeys(dependency_graph, 0)
    for deps in package_transitive_deps.values():
        for dep in deps:
            dependency_parent_count[dep] = dependency_parent_count.get(dep, 0) + 1

    return (
        dependency_graph,
        package_transitive_deps,
        dependency_parent_count,
        pkg_data_map,
    )


def _resolve_physical_sizes(
    dependency_graph: dict[str, list[str]], prefix: Path
) -> dict[str, int]:
    """Calculate disk footprint for installed packages."""
    size_map = {}
    for name in dependency_graph:
        cellar_path = prefix / "Cellar" / name
        cask_path = prefix / "Caskroom" / name

        if cellar_path.exists():
            size_map[name] = homebrew.get_directory_size(cellar_path)
        elif cask_path.exists():
            size_map[name] = homebrew.get_directory_size(cask_path)
        else:
            size_map[name] = 0
    return size_map


def _resolve_theoretical_sizes(
    resolution_set: set[str], pkg_data_map: dict[str, Any], arch: str
) -> dict[str, int]:
    """Resolve compressed bottle sizes via ghcr.io for a specific subset of packages."""
    package_digest = {}
    bottles = {}

    for name in resolution_set:
        pkg = pkg_data_map.get(name, {})
        files = pkg.get("bottle", {}).get("stable", {}).get("files", {})
        file_info = files.get(arch) or next(iter(files.values()), None)

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


def _compute_bloat_metrics(
    targets: list[str],
    size_map: dict[str, int],
    package_transitive_deps: dict[str, set[str]],
    dependency_parent_count: dict[str, int],
) -> pd.DataFrame:
    """Execute the fractional attribution math and compile the DataFrame."""
    results = []
    for target in targets:
        core_size = size_map.get(target, 0)
        target_deps = package_transitive_deps.get(target, set())

        standard_recursive_size = core_size + sum(
            size_map.get(d, 0) for d in target_deps
        )
        fractional_dep_size = sum(
            size_map.get(d, 0) / dependency_parent_count.get(d, 1) for d in target_deps
        )

        weighted_recursive_size = core_size + fractional_dep_size
        ratio = (weighted_recursive_size / core_size) if core_size > 0 else 1.0

        results.append(
            {
                "Package": target,
                "Core_Bytes": core_size,
                "Standard_Bytes": standard_recursive_size,
                "Weighted_Bytes": weighted_recursive_size,
                "Bloat_Ratio": ratio,
                "Dep_Count": len(target_deps),
                "Is_Leaf": dependency_parent_count.get(target, 0) == 0
                if len(targets) > 1
                else True,
            }
        )

    return pd.DataFrame(results)


def _compute_explain_metrics(
    target: str,
    size_map: dict[str, int],
    package_transitive_deps: dict[str, set[str]],
    dependency_parent_count: dict[str, int],
    is_theoretical: bool,
) -> pd.DataFrame:
    """Compile the fractional dependency cost breakdown for a target package."""
    target_deps = package_transitive_deps.get(target, set())
    results = []

    for dep in target_deps:
        dep_size = size_map.get(dep, 0)
        parents = dependency_parent_count.get(dep, 1)
        attributed_size = dep_size / parents if parents > 0 else float(dep_size)

        row = {
            "Dependency": dep,
            "Shared_By": parents,
            "Attributed_Bytes": attributed_size,
        }

        if is_theoretical:
            row["Archive_Bytes"] = dep_size
        else:
            row["Core_Bytes"] = dep_size

        results.append(row)

    df = pd.DataFrame(results)
    if not df.empty:
        df = df.sort_values(by="Attributed_Bytes", ascending=False)

    return df


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
    graph, trans_deps, in_degrees, _ = _build_global_topology(metadata)
    size_map = _resolve_physical_sizes(graph, prefix)
    targets = list(graph.keys())

    return _compute_bloat_metrics(targets, size_map, trans_deps, in_degrees)


def build_targeted_analysis_dataframe(
    metadata: dict[str, Any], prefix: Path, target: str
) -> pd.DataFrame:
    """Constructs a DataFrame for a single installed target."""
    graph, trans_deps, in_degrees, _ = _build_global_topology(metadata)

    if target not in graph:
        raise ValueError(f"Package '{target}' not found in the local installation.")

    size_map = _resolve_physical_sizes(graph, prefix)
    return _compute_bloat_metrics([target], size_map, trans_deps, in_degrees)


def build_compare_analysis_dataframe(
    metadata: dict[str, Any], prefix: Path, targets: list[str]
) -> pd.DataFrame:
    """Constructs a DataFrame for comparing multiple installed targets."""
    graph, trans_deps, in_degrees, _ = _build_global_topology(metadata)

    valid_targets = [t for t in targets if t in graph]
    if not valid_targets:
        raise ValueError(
            "None of the specified packages were found in the local installation."
        )

    size_map = _resolve_physical_sizes(graph, prefix)
    return _compute_bloat_metrics(valid_targets, size_map, trans_deps, in_degrees)


def build_explain_analysis_dataframe(
    metadata: dict[str, Any], prefix: Path, target: str
) -> pd.DataFrame:
    """Constructs a dependency breakdown DataFrame for a physical target."""
    graph, trans_deps, in_degrees, _ = _build_global_topology(metadata)

    if target not in graph:
        raise ValueError(f"Package '{target}' not found in the local installation.")

    size_map = _resolve_physical_sizes(graph, prefix)
    return _compute_explain_metrics(
        target, size_map, trans_deps, in_degrees, is_theoretical=False
    )


def build_theoretical_dataframe(
    metadata: dict[str, Any], arch: str = "arm64_tahoe"
) -> pd.DataFrame:
    """Constructs the ecosystem-wide theoretical leaderboard."""
    graph, trans_deps, in_degrees, pkg_map = _build_global_topology(metadata)

    resolution_set = set(graph.keys())
    size_map = _resolve_theoretical_sizes(resolution_set, pkg_map, arch)
    targets = list(graph.keys())

    return _compute_bloat_metrics(targets, size_map, trans_deps, in_degrees)


def build_targeted_theoretical_dataframe(
    metadata: dict[str, Any], target: str, arch: str = "arm64_tahoe"
) -> pd.DataFrame:
    """Constructs a DataFrame for a single target, minimizing network resolution."""
    graph, trans_deps, in_degrees, pkg_map = _build_global_topology(metadata)

    if target not in graph:
        raise ValueError(f"Package '{target}' not found in the catalog.")

    # Isolate strictly to the target's closure to prevent O(N) network lookups
    resolution_set = {target} | trans_deps[target]
    size_map = _resolve_theoretical_sizes(resolution_set, pkg_map, arch)

    return _compute_bloat_metrics([target], size_map, trans_deps, in_degrees)


def build_compare_theoretical_dataframe(
    metadata: dict[str, Any], targets: list[str], arch: str = "arm64_tahoe"
) -> pd.DataFrame:
    """Constructs a DataFrame for multiple targets by resolving their unioned closure."""
    graph, trans_deps, in_degrees, pkg_map = _build_global_topology(metadata)

    valid_targets = [t for t in targets if t in graph]
    if not valid_targets:
        raise ValueError("None of the specified packages were found in the catalog.")

    # Generate the union of all target transitive dependencies
    resolution_set = set(valid_targets)
    for t in valid_targets:
        resolution_set.update(trans_deps[t])

    size_map = _resolve_theoretical_sizes(resolution_set, pkg_map, arch)
    return _compute_bloat_metrics(valid_targets, size_map, trans_deps, in_degrees)


def build_explain_theoretical_dataframe(
    metadata: dict[str, Any], target: str, arch: str = "arm64_tahoe"
) -> pd.DataFrame:
    """Constructs a dependency breakdown DataFrame using theoretical metrics."""
    graph, trans_deps, in_degrees, pkg_map = _build_global_topology(metadata)

    if target not in graph:
        raise ValueError(f"Package '{target}' not found in the catalog.")

    resolution_set = {target} | trans_deps[target]
    size_map = _resolve_theoretical_sizes(resolution_set, pkg_map, arch)

    return _compute_explain_metrics(
        target, size_map, trans_deps, in_degrees, is_theoretical=True
    )
