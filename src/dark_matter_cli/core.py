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


def _require_catalog_targets(
    targets: list[str], graph: dict[str, list[str]], metadata: dict[str, Any]
) -> None:
    """Like _require_targets, but explain that casks can't be sized here.

    Raises:
        ValueError: If a target is a cask.
        PackageNotFoundError: If a target is missing from the catalog.
    """
    tokens = {c.get("token") for c in metadata.get("casks", [])}
    casks = [t for t in targets if t not in graph and t in tokens]
    if casks:
        names = ", ".join(f"'{c}'" for c in casks)
        raise ValueError(
            f"{names} {'is a cask' if len(casks) == 1 else 'are casks'}. "
            "Casks have no bottles, so the catalog can't size them; install "
            "and analyze them with --source installed."
        )
    _require_targets(targets, graph, CATALOG)


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
    trans_deps: dict[str, set[str]], pkg_data_map: dict[str, Any], catalog: bool
) -> tuple[frozenset[str], frozenset[str]]:
    """Return (roots, requested) for the graph.

    Packages with no dependents are always roots, which guarantees every
    package is reachable from at least one root. In installed mode, packages
    the user requested are roots too, even when something depends on them.
    """
    has_dependents = {dep for deps in trans_deps.values() for dep in deps}
    unreferenced = {pkg for pkg in trans_deps if pkg not in has_dependents}
    if catalog:
        return frozenset(unreferenced), frozenset()

    requested = frozenset(
        pkg for pkg, data in pkg_data_map.items() if _is_requested(data)
    )
    return frozenset(unreferenced | requested), requested


def _recorded_dependencies(
    formula: dict[str, Any], short_names: dict[str, str]
) -> list[str]:
    """The runtime dependencies Homebrew recorded when each keg was installed.

    A keg keeps linking against what it was built with even after the formula
    drops a dependency, which is why `brew autoremove` trusts this record over
    the current formula definition.
    """
    return [
        short_names.get(str(dep["full_name"]), str(dep["full_name"]))
        for keg in formula.get("installed") or []
        for dep in keg.get("runtime_dependencies") or []
        if isinstance(dep, dict) and "full_name" in dep
    ]


def _build_global_topology(metadata: dict[str, Any], catalog: bool) -> Topology:
    """Parse metadata and pre-compute the immutable DAG properties."""
    dependency_graph: dict[str, list[str]] = {}
    pkg_data_map: dict[str, Any] = {}

    formulae = metadata.get("formulae", [])
    short_names = {f.get("full_name", f["name"]): f["name"] for f in formulae}
    # Edges used only for closures: declared dependencies plus the runtime
    # dependencies each installed keg recorded (see _recorded_dependencies).
    closure_graph: dict[str, list[str]] = {}

    for formula in formulae:
        name = formula["name"]
        dependency_graph[name] = list(formula.get("dependencies", []))
        pkg_data_map[name] = formula
        recorded = _recorded_dependencies(formula, short_names)
        closure_graph[name] = list(dict.fromkeys(dependency_graph[name] + recorded))

    # Casks ship no bottles, so the catalog has nothing to size them by;
    # leaving them in would let unsized casks take shares of formulae.
    casks = [] if catalog else metadata.get("casks", [])
    for cask in casks:
        token = cask["token"]
        # Formulae and casks share a namespace in the graph; disambiguate
        # collisions (e.g. the `docker` formula vs. the `docker` cask).
        key = f"{token} (cask)" if token in dependency_graph else token
        depends_on = cask.get("depends_on", {})
        dependency_graph[key] = list(depends_on.get("formula", [])) + list(
            depends_on.get("cask", [])
        )
        closure_graph[key] = dependency_graph[key]
        pkg_data_map[key] = cask

    logger.debug(
        f"Dependency graph: {len(dependency_graph)} packages ({len(casks)} casks)"
    )

    package_transitive_deps = {
        pkg: get_all_dependencies(pkg, closure_graph) for pkg in closure_graph
    }
    roots, requested = _identify_roots(package_transitive_deps, pkg_data_map, catalog)

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
            # Older kegs are cleanup debris, not part of the package's cost.
            size_map[name] = homebrew.get_keg_sizes(pkg, prefix).current
    return size_map


def _select_bottle_file(files: dict[str, Any], tag: str) -> Any:
    """Pick the bottle Homebrew would pour for `tag`.

    Preference order: the exact tag, an arch-independent 'all' bottle, then
    (on macOS only) the newest bottle for the same CPU built for an older
    macOS release. Homebrew never pours a bottle built for a newer macOS, a
    different CPU, or another Linux tag, so neither does this.
    """
    if tag in files:
        return files[tag]
    if "all" in files:
        return files["all"]

    target = homebrew.macos_bottle_version(tag)
    if target is None:
        return None

    # Homebrew lists bottles newest-first.
    for candidate, file_info in files.items():
        version = homebrew.macos_bottle_version(candidate)
        if version is not None and version[0] == target[0] and version[1] <= target[1]:
            return file_info
    return None


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

    unbottled = len(resolution_set) - len(package_digest)
    if unbottled:
        logger.debug(f"{unbottled} package(s) have no {tag} bottle; they count as 0")

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
    subgraph = {target} | target_deps
    results = []

    for dep in target_deps:
        dep_size = size_map.get(dep, 0)
        is_root = dep in topo.roots

        row = {
            "Dependency": dep,
            "Shared_By": 0 if is_root else topo.sharer_count.get(dep, 1),
            "Attributed_Bytes": _attributed_size(dep, size_map, topo),
            "Is_Root": is_root,
            # Direct dependents within the target's closure, for tree views. A
            # dependency known only from a keg's install record has no declared
            # parent, so it hangs off the target.
            "Required_By": sorted(p for p in subgraph if dep in topo.graph.get(p, []))
            or [target],
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

    _require_catalog_targets([target], topo.graph, metadata)

    # Isolate strictly to the target's closure to prevent O(N) network lookups
    resolution_set = {target} | topo.trans_deps[target]
    size_map = _resolve_theoretical_sizes(resolution_set, topo.pkg_data, arch)

    return _compute_bloat_metrics([target], size_map, topo)


def build_compare_theoretical_dataframe(
    metadata: dict[str, Any], targets: list[str], arch: str | None = None
) -> pd.DataFrame:
    """Constructs a DataFrame for multiple targets by resolving their unioned closure."""
    topo = _build_global_topology(metadata, catalog=True)

    _require_catalog_targets(targets, topo.graph, metadata)

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

    _require_catalog_targets([target], topo.graph, metadata)

    resolution_set = {target} | topo.trans_deps[target]
    size_map = _resolve_theoretical_sizes(resolution_set, topo.pkg_data, arch)

    return _compute_explain_metrics(target, size_map, topo, is_theoretical=True)


def _retained(topo: Topology, anchors: set[str]) -> set[str]:
    """Every package that stays installed while `anchors` are kept."""
    kept = set(anchors)
    for anchor in anchors:
        kept |= topo.trans_deps[anchor]
    return kept & topo.graph.keys()


def _dependents(topo: Topology, pkg: str, among: set[str]) -> list[str]:
    """Packages in `among` that depend on `pkg`.

    Prefers declared (direct) dependents, falling back to any package whose
    closure includes `pkg` when the edge is only known from a keg's record.
    """
    direct = sorted(p for p in among if pkg in topo.graph[p])
    return direct or sorted(p for p in among if pkg in topo.trans_deps[p])


@dataclass(frozen=True)
class Reclaim:
    """What uninstalling a set of packages would free.

    Attributes:
        freed: One row per package that would be removed (Package,
            Core_Bytes, Is_Target), largest first.
        blocked: Targets Homebrew would refuse to uninstall, mapped to the
            installed packages that still depend on them.
    """

    freed: pd.DataFrame
    blocked: dict[str, list[str]]


def build_reclaim_dataframe(
    metadata: dict[str, Any], prefix: Path, targets: list[str]
) -> Reclaim:
    """Measure what `brew uninstall <targets> && brew autoremove` would free.

    Every other root stays installed, including existing orphans, so only
    bytes the targets are responsible for are counted.
    """
    topo = _build_global_topology(metadata, catalog=False)
    _require_targets(targets, topo.graph, INSTALLED)

    kept = _retained(topo, set(topo.roots) - set(targets))
    freed = topo.graph.keys() - kept
    blocked = {t: _dependents(topo, t, kept) for t in targets if t in kept}

    sizes = _resolve_physical_sizes({n: topo.pkg_data[n] for n in freed}, prefix)
    df = pd.DataFrame(
        [
            {"Package": n, "Core_Bytes": sizes[n], "Is_Target": n in targets}
            for n in freed
        ],
        columns=["Package", "Core_Bytes", "Is_Target"],
    )
    if not df.empty:
        df = df.sort_values(by=["Core_Bytes", "Package"], ascending=[False, True])
    return Reclaim(freed=df, blocked=blocked)


def build_orphans_dataframe(metadata: dict[str, Any], prefix: Path) -> pd.DataFrame:
    """List installed dependencies no requested package needs any more.

    This is the set `brew autoremove` would remove.
    """
    topo = _build_global_topology(metadata, catalog=False)
    orphans = topo.graph.keys() - _retained(topo, set(topo.requested))

    sizes = _resolve_physical_sizes({n: topo.pkg_data[n] for n in orphans}, prefix)
    df = pd.DataFrame(
        [
            {
                "Package": n,
                "Core_Bytes": sizes[n],
                "Required_By": _dependents(topo, n, orphans),
            }
            for n in orphans
        ],
        columns=["Package", "Core_Bytes", "Required_By"],
    )
    if not df.empty:
        df = df.sort_values(by=["Core_Bytes", "Package"], ascending=[False, True])
    return df


def build_cleanup_dataframe(metadata: dict[str, Any], prefix: Path) -> pd.DataFrame:
    """List the older kegs `brew cleanup` would remove from the Cellar."""
    rows: list[dict[str, Any]] = []
    for formula in metadata.get("formulae", []):
        kegs = homebrew.get_keg_sizes(formula, prefix)
        rows.extend(
            {"Package": formula["name"], "Version": version, "Bytes": size}
            for version, size in kegs.stale.items()
        )

    df = pd.DataFrame(rows, columns=["Package", "Version", "Bytes"])
    if not df.empty:
        df = df.sort_values(by=["Bytes", "Package"], ascending=[False, True])
    return df


SNAPSHOT_COLUMNS = ("Package", "Core_Bytes", "Weighted_Bytes")


def diff_snapshots(old: pd.DataFrame, new: pd.DataFrame) -> pd.DataFrame:
    """Compare two `export` snapshots package by package.

    Args:
        old: The earlier export.
        new: The later export.

    Returns:
        pd.DataFrame: One row per package that was added, removed, or changed
        size (Package, Status, Old_Core, New_Core, Core_Delta,
        Weighted_Delta), largest absolute core change first.

    Raises:
        ValueError: If either snapshot lacks the columns needed to compare.
    """
    for label, df in (("old", old), ("new", new)):
        missing = [c for c in SNAPSHOT_COLUMNS if c not in df.columns]
        if missing:
            raise ValueError(
                f"The {label} snapshot is missing {', '.join(missing)}; "
                "create snapshots with `dark-matter export`."
            )

    merged = old[list(SNAPSHOT_COLUMNS)].merge(
        new[list(SNAPSHOT_COLUMNS)],
        on="Package",
        how="outer",
        suffixes=("_old", "_new"),
        indicator=True,
    )
    status = merged["_merge"].map(
        {"left_only": "removed", "right_only": "added", "both": "changed"}
    )
    old_core = merged["Core_Bytes_old"].fillna(0)
    new_core = merged["Core_Bytes_new"].fillna(0)
    weighted_delta = merged["Weighted_Bytes_new"].fillna(0) - merged[
        "Weighted_Bytes_old"
    ].fillna(0)

    df = pd.DataFrame(
        {
            "Package": merged["Package"],
            "Status": status.astype(str),
            "Old_Core": old_core,
            "New_Core": new_core,
            "Core_Delta": new_core - old_core,
            "Weighted_Delta": weighted_delta,
        }
    )
    changed = (
        (df["Status"] != "changed")
        | (df["Core_Delta"] != 0)
        | (df["Weighted_Delta"] != 0)
    )
    df = df[changed]
    order = df["Core_Delta"].abs().sort_values(ascending=False, kind="stable").index
    return df.loc[order].reset_index(drop=True)
