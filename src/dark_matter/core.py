from pathlib import Path
from typing import Any

import pandas as pd

from dark_matter import homebrew


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
    """Construct the primary DataFrame and execute all mathematical bloat models.

    Args:
        metadata: The parsed JSON payload from the Homebrew API.
        prefix: The base physical installation path for Homebrew.

    Returns:
        pd.DataFrame: A highly structured dataframe containing calculated bloat metrics.
    """
    formulae = metadata.get("formulae", [])
    casks = metadata.get("casks", [])
    packages = formulae + casks

    # Initialize edge list for DAG
    dependency_graph: dict[str, list[str]] = {}
    size_map: dict[str, int] = {}

    for pkg in packages:
        name = pkg["name"]
        if isinstance(name, list):
            name = name[0]

        deps = pkg.get("dependencies", [])
        dependency_graph[name] = deps

        # Calculate Isolated Core Size (Sc) via os.scandir
        cellar_path = prefix / "Cellar" / name
        cask_path = prefix / "Caskroom" / name

        if cellar_path.exists():
            size_map[name] = homebrew.get_directory_size(cellar_path)
        elif cask_path.exists():
            size_map[name] = homebrew.get_directory_size(cask_path)
        else:
            size_map[name] = 0

    # First Pass: Pre-compute the full transitive dependency set for every package
    package_transitive_deps: dict[str, set[str]] = {}
    for pkg in dependency_graph:
        package_transitive_deps[pkg] = get_all_dependencies(pkg, dependency_graph)

    # Calculate |P(d)|: In-degree frequency map for shared dependencies
    dependency_parent_count: dict[str, int] = dict.fromkeys(dependency_graph, 0)
    for deps in package_transitive_deps.values():
        for dep in deps:
            if dep in dependency_parent_count:
                dependency_parent_count[dep] += 1
            else:
                dependency_parent_count[dep] = 1

    results = []

    # Second Pass: Compute St(p) and Swt(p)
    for pkg in dependency_graph:
        core_size = size_map.get(pkg, 0)
        transitive_deps = package_transitive_deps[pkg]

        standard_recursive_size = core_size + sum(
            size_map.get(d, 0) for d in transitive_deps
        )

        fractional_dep_size = 0.0
        for dep in transitive_deps:
            dep_core_size = size_map.get(dep, 0)
            p_d = dependency_parent_count.get(dep, 1)

            if p_d > 0:
                fractional_dep_size += dep_core_size / p_d

        weighted_recursive_size = core_size + fractional_dep_size

        ratio = (weighted_recursive_size / core_size) if core_size > 0 else 1.0

        results.append(
            {
                "Package": pkg,
                "Core_Bytes": core_size,
                "Standard_Bytes": standard_recursive_size,
                "Weighted_Bytes": weighted_recursive_size,
                "Bloat_Ratio": ratio,
                "Dep_Count": len(transitive_deps),
                "Is_Leaf": dependency_parent_count.get(pkg, 0) == 0,
            }
        )

    return pd.DataFrame(results)


def build_theoretical_dataframe(
    metadata: dict[str, Any], arch: str = "arm64_tahoe"
) -> pd.DataFrame:
    """Constructs a graph and DataFrame using compressed bottle mass resolved from ghcr.io.

    Args:
        metadata: The parsed JSON payload from the Homebrew API cache.
        arch: The target bottle architecture tag (e.g. 'arm64_tahoe').

    Returns:
        pd.DataFrame: A structured dataframe containing calculated bloat metrics.
    """
    formulae = metadata.get("formulae", [])
    casks = metadata.get("casks", [])
    packages = formulae + casks

    dependency_graph: dict[str, list[str]] = {}
    package_digest: dict[str, str] = {}
    bottles: dict[str, str] = {}

    for pkg in packages:
        name = pkg["name"]
        if isinstance(name, list):
            name = name[0]

        # Homebrew API formats dependencies as flat strings list
        dependency_graph[name] = pkg.get("dependencies", [])

        # Casks have no "bottle" key today (Phase 3 roadmap item); this
        # naturally yields an empty files dict and a 0-byte size for them.
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

    size_map: dict[str, int] = {
        name: resolved_sizes.get(digest, 0) for name, digest in package_digest.items()
    }

    # The rest of the DAG mathematics maps identically to Phase 1
    package_transitive_deps = {
        pkg: get_all_dependencies(pkg, dependency_graph) for pkg in dependency_graph
    }

    dependency_parent_count: dict[str, int] = dict.fromkeys(dependency_graph, 0)
    for deps in package_transitive_deps.values():
        for dep in deps:
            dependency_parent_count[dep] = dependency_parent_count.get(dep, 0) + 1

    results = []
    for pkg in dependency_graph:
        core_size = size_map.get(pkg, 0)
        transitive_deps = package_transitive_deps[pkg]

        standard_recursive_size = core_size + sum(
            size_map.get(d, 0) for d in transitive_deps
        )

        fractional_dep_size = sum(
            size_map.get(d, 0) / dependency_parent_count.get(d, 1)
            for d in transitive_deps
        )
        weighted_recursive_size = core_size + fractional_dep_size
        ratio = (weighted_recursive_size / core_size) if core_size > 0 else 1.0

        results.append(
            {
                "Package": pkg,
                "Core_Bytes": core_size,
                "Standard_Bytes": standard_recursive_size,
                "Weighted_Bytes": weighted_recursive_size,
                "Bloat_Ratio": ratio,
                "Dep_Count": len(transitive_deps),
                "Is_Leaf": dependency_parent_count.get(pkg, 0) == 0,
            }
        )

    return pd.DataFrame(results)


def build_targeted_theoretical_dataframe(
    metadata: dict[str, Any], target: str, arch: str = "arm64_tahoe"
) -> pd.DataFrame:
    """Constructs a DataFrame for a single target, minimizing network resolution."""
    formulae = metadata.get("formulae", [])
    casks = metadata.get("casks", [])
    packages = formulae + casks

    dependency_graph: dict[str, list[str]] = {}
    pkg_data_map = {}

    # 1. Build the global topology
    for pkg in packages:
        name = pkg["name"]
        if isinstance(name, list):
            name = name[0]
        dependency_graph[name] = pkg.get("dependencies", [])
        pkg_data_map[name] = pkg

    if target not in dependency_graph:
        raise ValueError(f"Package '{target}' not found in the catalog.")

    # 2. Compute global in-degrees for accurate fractional attribution (Fast)
    package_transitive_deps = {
        pkg: get_all_dependencies(pkg, dependency_graph) for pkg in dependency_graph
    }

    dependency_parent_count: dict[str, int] = dict.fromkeys(dependency_graph, 0)
    for deps in package_transitive_deps.values():
        for dep in deps:
            dependency_parent_count[dep] += 1

    # 3. Isolate the target's closure
    target_deps = package_transitive_deps[target]
    resolution_set = {target} | target_deps

    # 4. Resolve bottle sizes ONLY for the target's closure (Slow/Network bounded)
    package_digest: dict[str, str] = {}
    bottles: dict[str, str] = {}

    for name in resolution_set:
        pkg = pkg_data_map[name]
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

    size_map: dict[str, int] = {
        name: resolved_sizes.get(digest, 0) for name, digest in package_digest.items()
    }

    # 5. Apply the mathematical models
    core_size = size_map.get(target, 0)
    standard_recursive_size = core_size + sum(size_map.get(d, 0) for d in target_deps)

    fractional_dep_size = sum(
        size_map.get(d, 0) / dependency_parent_count.get(d, 1) for d in target_deps
    )
    weighted_recursive_size = core_size + fractional_dep_size
    ratio = (weighted_recursive_size / core_size) if core_size > 0 else 1.0

    return pd.DataFrame(
        [
            {
                "Package": target,
                "Core_Bytes": core_size,
                "Standard_Bytes": standard_recursive_size,
                "Weighted_Bytes": weighted_recursive_size,
                "Bloat_Ratio": ratio,
                "Dep_Count": len(target_deps),
                "Is_Leaf": True,  # Forced true so display.py renders it
            }
        ]
    )
