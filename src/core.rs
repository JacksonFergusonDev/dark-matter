use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use indexmap::IndexMap;
use rayon::prelude::*;

use crate::error::{CATALOG, DarkMatterError, INSTALLED, Result};
use crate::homebrew::{self, DefaultTheoreticalSizer, FsSizer, PhysicalSizer, TheoreticalSizer};
use crate::model::{
    BloatRow, BottleFile, BrewMetadata, Cask, CleanupRow, DiffRow, DiffStatus, ExplainRow, Formula,
    OrphanRow, PackageData, ReclaimResult, ReclaimRow, SnapshotRow,
};

/// Required column names for `export` snapshots consumed by `diff`.
pub const SNAPSHOT_COLUMNS: &[&str] = &["Package", "Core_Bytes", "Weighted_Bytes"];

/// Immutable DAG properties shared by every analysis mode.
#[derive(Debug, Clone)]
pub struct Topology {
    /// Each package's direct declared dependencies (formulae then casks).
    pub graph: IndexMap<String, Vec<String>>,
    /// Each package's transitive dependencies (declared + keg runtime dependencies).
    pub trans_deps: HashMap<String, HashSet<String>>,
    /// Packages that bytes are attributed to: everything requested explicitly
    /// (in installed mode), plus any package nothing else depends on.
    pub roots: HashSet<String>,
    /// Packages the user asked for (formulae installed on request, and every
    /// cask). Empty in catalog mode.
    pub requested: HashSet<String>,
    /// For each non-root dependency, how many roots transitively need it.
    pub sharer_count: HashMap<String, usize>,
    /// Each package's raw metadata, keyed identically to `graph`.
    pub pkg_data: IndexMap<String, PackageData>,
}

fn require_targets(
    targets: &[String],
    graph: &IndexMap<String, Vec<String>>,
    location: &'static str,
) -> Result<()> {
    let missing: Vec<String> = targets
        .iter()
        .filter(|t| !graph.contains_key(t.as_str()))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(DarkMatterError::PackageNotFound {
            packages: missing,
            location,
        });
    }
    Ok(())
}

fn require_catalog_targets(
    targets: &[String],
    graph: &IndexMap<String, Vec<String>>,
    metadata: &BrewMetadata,
) -> Result<()> {
    let tokens: HashSet<&str> = metadata.casks.iter().map(|c| c.token.as_str()).collect();
    let casks: Vec<String> = targets
        .iter()
        .filter(|t| !graph.contains_key(t.as_str()) && tokens.contains(t.as_str()))
        .cloned()
        .collect();
    if !casks.is_empty() {
        return Err(DarkMatterError::CaskInCatalog { casks });
    }
    require_targets(targets, graph, CATALOG)
}

fn recorded_dependencies(formula: &Formula, short_names: &HashMap<&str, &str>) -> Vec<String> {
    formula
        .installed
        .iter()
        .flat_map(|keg| keg.runtime_dependencies.iter())
        .filter_map(|dep| dep.full_name.as_deref())
        .map(|full_name| {
            short_names
                .get(full_name)
                .copied()
                .unwrap_or(full_name)
                .to_string()
        })
        .collect()
}

fn dedup_preserve_order(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

fn identify_roots(
    trans_deps: &HashMap<String, HashSet<String>>,
    pkg_data_map: &IndexMap<String, PackageData>,
    catalog: bool,
) -> (HashSet<String>, HashSet<String>) {
    let has_dependents: HashSet<&str> = trans_deps
        .values()
        .flat_map(|deps| deps.iter().map(String::as_str))
        .collect();

    let unreferenced: HashSet<String> = trans_deps
        .keys()
        .filter(|pkg| !has_dependents.contains(pkg.as_str()))
        .cloned()
        .collect();

    if catalog {
        return (unreferenced, HashSet::new());
    }

    let requested: HashSet<String> = pkg_data_map
        .iter()
        .filter(|(_, data)| data.is_requested())
        .map(|(pkg, _)| pkg.clone())
        .collect();

    let roots = unreferenced.union(&requested).cloned().collect();
    (roots, requested)
}

/// Recursively traverse the dependency graph to extract all transitive dependencies.
///
/// Cycles are handled safely via a visited set.
pub fn get_all_dependencies(pkg: &str, graph: &IndexMap<String, Vec<String>>) -> HashSet<String> {
    let mut visited = HashSet::new();
    collect_dependencies(pkg, graph, &mut visited);
    visited
}

fn collect_dependencies(
    pkg: &str,
    graph: &IndexMap<String, Vec<String>>,
    visited: &mut HashSet<String>,
) {
    let Some(deps) = graph.get(pkg) else {
        return;
    };
    for dep in deps {
        if visited.insert(dep.clone()) {
            collect_dependencies(dep, graph, visited);
        }
    }
}

/// Parse metadata and pre-compute the immutable DAG properties.
pub fn build_global_topology(metadata: &BrewMetadata, catalog: bool) -> Topology {
    let mut dependency_graph: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut pkg_data_map: IndexMap<String, PackageData> = IndexMap::new();
    let mut closure_graph: IndexMap<String, Vec<String>> = IndexMap::new();

    let short_names: HashMap<&str, &str> = metadata
        .formulae
        .iter()
        .map(|f| {
            let full = f.full_name.as_deref().unwrap_or(f.name.as_str());
            (full, f.name.as_str())
        })
        .collect();

    for formula in &metadata.formulae {
        let name = formula.name.clone();
        // Dependencies on other tap formulae may use the full `user/tap/name`.
        let deps: Vec<String> = formula
            .dependencies
            .iter()
            .map(|d| {
                short_names
                    .get(d.as_str())
                    .copied()
                    .unwrap_or(d.as_str())
                    .to_string()
            })
            .collect();
        let recorded = recorded_dependencies(formula, &short_names);
        let closure = dedup_preserve_order(deps.iter().cloned().chain(recorded));

        dependency_graph.insert(name.clone(), deps);
        closure_graph.insert(name.clone(), closure);
        pkg_data_map.insert(name, PackageData::Formula(formula.clone()));
    }

    // Casks ship no bottles, so the catalog has nothing to size them by;
    // leaving them in would let unsized casks take shares of formulae.
    let casks: &[Cask] = if catalog { &[] } else { &metadata.casks };
    for cask in casks {
        // Formulae and casks share a namespace in the graph; disambiguate
        // collisions (e.g. the `docker` formula vs. the `docker` cask).
        let key = if dependency_graph.contains_key(&cask.token) {
            format!("{} (cask)", cask.token)
        } else {
            cask.token.clone()
        };

        let deps: Vec<String> = cask
            .depends_on
            .formula
            .iter()
            .map(|d| {
                short_names
                    .get(d.as_str())
                    .copied()
                    .unwrap_or(d.as_str())
                    .to_string()
            })
            .chain(cask.depends_on.cask.iter().cloned())
            .collect();

        closure_graph.insert(key.clone(), deps.clone());
        dependency_graph.insert(key.clone(), deps);
        pkg_data_map.insert(key, PackageData::Cask(cask.clone()));
    }

    log::debug!(
        "Dependency graph: {} packages ({} casks)",
        dependency_graph.len(),
        casks.len()
    );

    let trans_deps: HashMap<String, HashSet<String>> = closure_graph
        .keys()
        .map(|pkg| (pkg.clone(), get_all_dependencies(pkg, &closure_graph)))
        .collect();

    let (roots, requested) = identify_roots(&trans_deps, &pkg_data_map, catalog);

    // Only roots share costs: counting intermediate dependencies as sharers
    // would hand part of each shared package to rows that are never summed.
    let mut sharer_count: HashMap<String, usize> = HashMap::new();
    for root in &roots {
        if let Some(deps) = trans_deps.get(root) {
            for dep in deps {
                if !roots.contains(dep) {
                    *sharer_count.entry(dep.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    log::debug!("Roots: {} ({} requested)", roots.len(), requested.len());

    Topology {
        graph: dependency_graph,
        trans_deps,
        roots,
        requested,
        sharer_count,
        pkg_data: pkg_data_map,
    }
}

fn resolve_physical_sizes(
    pkg_data_map: &IndexMap<String, PackageData>,
    prefix: &Path,
    sizer: &dyn PhysicalSizer,
) -> HashMap<String, u64> {
    let entries: Vec<(&String, &PackageData)> = pkg_data_map.iter().collect();
    entries
        .into_par_iter()
        .map(|(name, pkg)| (name.clone(), measure_package(pkg, prefix, sizer)))
        .collect()
}

fn resolve_physical_sizes_subset(
    pkg_data_map: &IndexMap<String, PackageData>,
    names: &[String],
    prefix: &Path,
    sizer: &dyn PhysicalSizer,
) -> HashMap<String, u64> {
    names
        .par_iter()
        .filter_map(|name| {
            pkg_data_map
                .get(name)
                .map(|pkg| (name.clone(), measure_package(pkg, prefix, sizer)))
        })
        .collect()
}

fn closure_names(targets: &[String], topo: &Topology) -> Vec<String> {
    let mut set: HashSet<String> = targets.iter().cloned().collect();
    for target in targets {
        if let Some(deps) = topo.trans_deps.get(target) {
            set.extend(deps.iter().cloned());
        }
    }
    set.into_iter().collect()
}

fn measure_package(pkg: &PackageData, prefix: &Path, sizer: &dyn PhysicalSizer) -> u64 {
    match pkg {
        PackageData::Cask(cask) => sizer.cask_size(cask, prefix),
        // Older kegs are cleanup debris, not part of the package's cost.
        PackageData::Formula(formula) => sizer.keg_sizes(formula, prefix).current,
    }
}

/// Pick the bottle Homebrew would pour for `tag`.
///
/// Preference order: the exact tag, an arch-independent `"all"` bottle, then
/// (on macOS only) the newest bottle for the same CPU built for an older
/// macOS release. Homebrew never pours a bottle built for a newer macOS, a
/// different CPU, or another Linux tag, so neither does this.
pub fn select_bottle_file<'a>(
    files: &'a IndexMap<String, BottleFile>,
    tag: &str,
) -> Option<&'a BottleFile> {
    if let Some(exact) = files.get(tag) {
        return Some(exact);
    }
    if let Some(any) = files.get("all") {
        return Some(any);
    }

    let target = homebrew::macos_bottle_version(tag)?;

    // Homebrew lists bottles newest-first.
    for (candidate, file_info) in files {
        if let Some(version) = homebrew::macos_bottle_version(candidate)
            && version.0 == target.0
            && version.1 <= target.1
        {
            return Some(file_info);
        }
    }
    None
}

fn resolve_theoretical_sizes(
    resolution_set: &HashSet<String>,
    pkg_data_map: &IndexMap<String, PackageData>,
    arch: Option<&str>,
    sizer: &dyn TheoreticalSizer,
) -> HashMap<String, u64> {
    let tag = arch
        .filter(|a| !a.is_empty())
        .map_or_else(|| sizer.detect_bottle_tag(), str::to_owned);

    let mut package_digest: HashMap<String, String> = HashMap::new();
    let mut bottles: IndexMap<String, String> = IndexMap::new();

    for (name, pkg) in pkg_data_map {
        if !resolution_set.contains(name) {
            continue;
        }
        let Some(formula) = pkg.as_formula() else {
            continue;
        };
        let Some(files) = formula
            .bottle
            .as_ref()
            .and_then(|b| b.stable.as_ref())
            .map(|s| &s.files)
        else {
            continue;
        };

        if let Some(file_info) = select_bottle_file(files, &tag)
            && !file_info.sha256.is_empty()
            && !file_info.url.is_empty()
        {
            package_digest.insert(name.clone(), file_info.sha256.clone());
            bottles.insert(file_info.sha256.clone(), file_info.url.clone());
        }
    }

    let unbottled = resolution_set.len().saturating_sub(package_digest.len());
    if unbottled > 0 {
        log::debug!("{unbottled} package(s) have no {tag} bottle; they count as 0");
    }

    let resolved_sizes = sizer.resolve_bottle_sizes(&bottles);
    package_digest
        .into_iter()
        .map(|(name, digest)| {
            let size = resolved_sizes.get(&digest).copied().unwrap_or(0);
            (name, size)
        })
        .collect()
}

fn attributed_size(dep: &str, size_map: &HashMap<String, u64>, topo: &Topology) -> f64 {
    if topo.roots.contains(dep) {
        return 0.0;
    }
    let size = size_map.get(dep).copied().unwrap_or(0);
    let sharers = topo.sharer_count.get(dep).copied().unwrap_or(1).max(1);
    size as f64 / sharers as f64
}

fn compute_bloat_metrics<S: AsRef<str>>(
    targets: &[S],
    size_map: &HashMap<String, u64>,
    topo: &Topology,
) -> Vec<BloatRow> {
    targets
        .iter()
        .map(|target| {
            let target = target.as_ref();
            let core_bytes = size_map.get(target).copied().unwrap_or(0);
            let target_deps = topo.trans_deps.get(target);
            let dep_count = target_deps.map_or(0, HashSet::len);

            let dep_standard: u64 = target_deps.map_or(0, |deps| {
                deps.iter()
                    .map(|d| size_map.get(d.as_str()).copied().unwrap_or(0))
                    .sum()
            });
            let dep_weighted: f64 = target_deps.map_or(0.0, |deps| {
                deps.iter()
                    .map(|d| attributed_size(d, size_map, topo))
                    .sum()
            });

            let standard_bytes = core_bytes + dep_standard;
            let weighted_bytes = core_bytes as f64 + dep_weighted;
            // A package with no measurable core has no meaningful ratio.
            let bloat_ratio = if core_bytes > 0 {
                Some(weighted_bytes / core_bytes as f64)
            } else {
                None
            };

            BloatRow {
                package: target.to_string(),
                core_bytes,
                standard_bytes,
                weighted_bytes,
                bloat_ratio,
                dep_count,
                is_root: topo.roots.contains(target),
                is_requested: topo.requested.contains(target),
            }
        })
        .collect()
}

fn compute_explain_metrics(
    target: &str,
    size_map: &HashMap<String, u64>,
    topo: &Topology,
) -> Vec<ExplainRow> {
    let Some(target_deps) = topo.trans_deps.get(target) else {
        return Vec::new();
    };
    if target_deps.is_empty() {
        return Vec::new();
    }

    let mut subgraph: HashSet<&str> = target_deps.iter().map(String::as_str).collect();
    subgraph.insert(target);

    let mut rows: Vec<ExplainRow> = target_deps
        .iter()
        .map(|dep| {
            let size_bytes = size_map.get(dep.as_str()).copied().unwrap_or(0);
            let is_root = topo.roots.contains(dep.as_str());
            let shared_by = if is_root {
                0
            } else {
                topo.sharer_count.get(dep.as_str()).copied().unwrap_or(1)
            };
            let attributed_bytes = attributed_size(dep, size_map, topo);

            // Direct dependents within the target's closure, for tree views. A
            // dependency known only from a keg's install record has no declared
            // parent, so it hangs off the target.
            let mut required_by: Vec<String> = subgraph
                .iter()
                .filter(|&&p| {
                    topo.graph
                        .get(p)
                        .is_some_and(|deps| deps.iter().any(|d| d == dep))
                })
                .map(|&p| p.to_string())
                .collect();
            required_by.sort();
            if required_by.is_empty() {
                required_by.push(target.to_string());
            }

            ExplainRow {
                dependency: dep.clone(),
                size_bytes,
                shared_by,
                attributed_bytes,
                is_root,
                required_by,
            }
        })
        .collect();

    // Break ties by name so output is stable across runs.
    rows.sort_by(|a, b| {
        b.attributed_bytes
            .total_cmp(&a.attributed_bytes)
            .then_with(|| a.dependency.cmp(&b.dependency))
    });
    rows
}

fn log_conservation(rows: &[BloatRow], size_map: &HashMap<String, u64>) {
    let total: u64 = size_map.values().sum();
    let attributed: f64 = rows
        .iter()
        .filter(|row| row.is_root)
        .map(|row| row.weighted_bytes)
        .sum();
    let diff = total as f64 - attributed;
    log::debug!(
        "Attribution: {total} bytes measured, {attributed:.0} attributed to \
         roots (difference {diff:.0})"
    );
}

/// Construct the primary bloat analysis utilizing physical disk metrics.
pub fn build_analysis(metadata: &BrewMetadata, prefix: &Path) -> Vec<BloatRow> {
    build_analysis_with(metadata, prefix, &FsSizer)
}

/// Construct the primary bloat analysis using a custom [`PhysicalSizer`].
pub fn build_analysis_with(
    metadata: &BrewMetadata,
    prefix: &Path,
    sizer: &dyn PhysicalSizer,
) -> Vec<BloatRow> {
    let topo = build_global_topology(metadata, false);
    let size_map = resolve_physical_sizes(&topo.pkg_data, prefix, sizer);
    let targets: Vec<&str> = topo.graph.keys().map(String::as_str).collect();

    let rows = compute_bloat_metrics(&targets, &size_map, &topo);
    log_conservation(&rows, &size_map);
    rows
}

/// Construct bloat metrics for a single installed target.
pub fn build_targeted_analysis(
    metadata: &BrewMetadata,
    prefix: &Path,
    target: &str,
) -> Result<Vec<BloatRow>> {
    build_targeted_analysis_with(metadata, prefix, target, &FsSizer)
}

/// Construct bloat metrics for a single installed target using a custom [`PhysicalSizer`].
pub fn build_targeted_analysis_with(
    metadata: &BrewMetadata,
    prefix: &Path,
    target: &str,
    sizer: &dyn PhysicalSizer,
) -> Result<Vec<BloatRow>> {
    let topo = build_global_topology(metadata, false);
    let targets = [target.to_string()];
    require_targets(&targets, &topo.graph, INSTALLED)?;

    let scope = closure_names(&targets, &topo);
    let size_map = resolve_physical_sizes_subset(&topo.pkg_data, &scope, prefix, sizer);
    Ok(compute_bloat_metrics(&targets, &size_map, &topo))
}

/// Construct bloat metrics for comparing multiple installed targets.
pub fn build_compare_analysis(
    metadata: &BrewMetadata,
    prefix: &Path,
    targets: &[String],
) -> Result<Vec<BloatRow>> {
    build_compare_analysis_with(metadata, prefix, targets, &FsSizer)
}

/// Construct bloat metrics for comparing multiple installed targets using a custom [`PhysicalSizer`].
pub fn build_compare_analysis_with(
    metadata: &BrewMetadata,
    prefix: &Path,
    targets: &[String],
    sizer: &dyn PhysicalSizer,
) -> Result<Vec<BloatRow>> {
    let topo = build_global_topology(metadata, false);
    require_targets(targets, &topo.graph, INSTALLED)?;

    let scope = closure_names(targets, &topo);
    let size_map = resolve_physical_sizes_subset(&topo.pkg_data, &scope, prefix, sizer);
    Ok(compute_bloat_metrics(targets, &size_map, &topo))
}

/// Construct a dependency attribution breakdown for a physical target.
pub fn build_explain_analysis(
    metadata: &BrewMetadata,
    prefix: &Path,
    target: &str,
) -> Result<Vec<ExplainRow>> {
    build_explain_analysis_with(metadata, prefix, target, &FsSizer)
}

/// Construct a dependency attribution breakdown for a physical target using a custom [`PhysicalSizer`].
pub fn build_explain_analysis_with(
    metadata: &BrewMetadata,
    prefix: &Path,
    target: &str,
    sizer: &dyn PhysicalSizer,
) -> Result<Vec<ExplainRow>> {
    let topo = build_global_topology(metadata, false);
    let targets = [target.to_string()];
    require_targets(&targets, &topo.graph, INSTALLED)?;

    let scope = closure_names(&targets, &topo);
    let size_map = resolve_physical_sizes_subset(&topo.pkg_data, &scope, prefix, sizer);
    Ok(compute_explain_metrics(target, &size_map, &topo))
}

/// Construct the ecosystem-wide theoretical leaderboard.
pub fn build_theoretical(metadata: &BrewMetadata, arch: Option<&str>) -> Vec<BloatRow> {
    build_theoretical_with(metadata, arch, &DefaultTheoreticalSizer)
}

/// Construct the ecosystem-wide theoretical leaderboard using a custom [`TheoreticalSizer`].
pub fn build_theoretical_with(
    metadata: &BrewMetadata,
    arch: Option<&str>,
    sizer: &dyn TheoreticalSizer,
) -> Vec<BloatRow> {
    let topo = build_global_topology(metadata, true);
    let resolution_set: HashSet<String> = topo.graph.keys().cloned().collect();
    let size_map = resolve_theoretical_sizes(&resolution_set, &topo.pkg_data, arch, sizer);
    let targets: Vec<&str> = topo.graph.keys().map(String::as_str).collect();

    let rows = compute_bloat_metrics(&targets, &size_map, &topo);
    log_conservation(&rows, &size_map);
    rows
}

/// Construct theoretical bloat metrics for a single target, minimizing network resolution.
pub fn build_targeted_theoretical(
    metadata: &BrewMetadata,
    target: &str,
    arch: Option<&str>,
) -> Result<Vec<BloatRow>> {
    build_targeted_theoretical_with(metadata, target, arch, &DefaultTheoreticalSizer)
}

/// Construct theoretical bloat metrics for a single target using a custom [`TheoreticalSizer`].
pub fn build_targeted_theoretical_with(
    metadata: &BrewMetadata,
    target: &str,
    arch: Option<&str>,
    sizer: &dyn TheoreticalSizer,
) -> Result<Vec<BloatRow>> {
    let topo = build_global_topology(metadata, true);
    let targets = [target.to_string()];
    require_catalog_targets(&targets, &topo.graph, metadata)?;

    let mut resolution_set = topo.trans_deps.get(target).cloned().unwrap_or_default();
    resolution_set.insert(target.to_string());

    let size_map = resolve_theoretical_sizes(&resolution_set, &topo.pkg_data, arch, sizer);
    Ok(compute_bloat_metrics(&targets, &size_map, &topo))
}

/// Construct theoretical bloat metrics for multiple targets by resolving their unioned closure.
pub fn build_compare_theoretical(
    metadata: &BrewMetadata,
    targets: &[String],
    arch: Option<&str>,
) -> Result<Vec<BloatRow>> {
    build_compare_theoretical_with(metadata, targets, arch, &DefaultTheoreticalSizer)
}

/// Construct theoretical bloat metrics for multiple targets using a custom [`TheoreticalSizer`].
pub fn build_compare_theoretical_with(
    metadata: &BrewMetadata,
    targets: &[String],
    arch: Option<&str>,
    sizer: &dyn TheoreticalSizer,
) -> Result<Vec<BloatRow>> {
    let topo = build_global_topology(metadata, true);
    require_catalog_targets(targets, &topo.graph, metadata)?;

    let mut resolution_set: HashSet<String> = targets.iter().cloned().collect();
    for target in targets {
        if let Some(deps) = topo.trans_deps.get(target) {
            resolution_set.extend(deps.iter().cloned());
        }
    }

    let size_map = resolve_theoretical_sizes(&resolution_set, &topo.pkg_data, arch, sizer);
    Ok(compute_bloat_metrics(targets, &size_map, &topo))
}

/// Construct a dependency attribution breakdown using theoretical bottle metrics.
pub fn build_explain_theoretical(
    metadata: &BrewMetadata,
    target: &str,
    arch: Option<&str>,
) -> Result<Vec<ExplainRow>> {
    build_explain_theoretical_with(metadata, target, arch, &DefaultTheoreticalSizer)
}

/// Construct a theoretical dependency attribution breakdown using a custom [`TheoreticalSizer`].
pub fn build_explain_theoretical_with(
    metadata: &BrewMetadata,
    target: &str,
    arch: Option<&str>,
    sizer: &dyn TheoreticalSizer,
) -> Result<Vec<ExplainRow>> {
    let topo = build_global_topology(metadata, true);
    let targets = [target.to_string()];
    require_catalog_targets(&targets, &topo.graph, metadata)?;

    let mut resolution_set = topo.trans_deps.get(target).cloned().unwrap_or_default();
    resolution_set.insert(target.to_string());

    let size_map = resolve_theoretical_sizes(&resolution_set, &topo.pkg_data, arch, sizer);
    Ok(compute_explain_metrics(target, &size_map, &topo))
}

fn retained(topo: &Topology, anchors: &HashSet<String>) -> HashSet<String> {
    let mut kept = anchors.clone();
    for anchor in anchors {
        if let Some(deps) = topo.trans_deps.get(anchor) {
            kept.extend(deps.iter().cloned());
        }
    }
    kept.retain(|pkg| topo.graph.contains_key(pkg));
    kept
}

fn dependents(topo: &Topology, pkg: &str, among: &HashSet<String>) -> Vec<String> {
    let mut direct: Vec<String> = among
        .iter()
        .filter(|p| {
            topo.graph
                .get(p.as_str())
                .is_some_and(|deps| deps.iter().any(|d| d == pkg))
        })
        .cloned()
        .collect();
    if !direct.is_empty() {
        direct.sort();
        return direct;
    }

    let mut transitive: Vec<String> = among
        .iter()
        .filter(|p| {
            topo.trans_deps
                .get(p.as_str())
                .is_some_and(|deps| deps.contains(pkg))
        })
        .cloned()
        .collect();
    transitive.sort();
    transitive
}

/// Measure what `brew uninstall <targets> && brew autoremove` would free.
///
/// Every other root stays installed, including existing orphans, so only bytes
/// the targets are responsible for are counted.
pub fn build_reclaim(
    metadata: &BrewMetadata,
    prefix: &Path,
    targets: &[String],
) -> Result<ReclaimResult> {
    build_reclaim_with(metadata, prefix, targets, &FsSizer)
}

/// Measure reclaimable packages using a custom [`PhysicalSizer`].
pub fn build_reclaim_with(
    metadata: &BrewMetadata,
    prefix: &Path,
    targets: &[String],
    sizer: &dyn PhysicalSizer,
) -> Result<ReclaimResult> {
    let topo = build_global_topology(metadata, false);
    require_targets(targets, &topo.graph, INSTALLED)?;

    let target_set: HashSet<&str> = targets.iter().map(String::as_str).collect();
    let remaining_roots: HashSet<String> = topo
        .roots
        .iter()
        .filter(|r| !target_set.contains(r.as_str()))
        .cloned()
        .collect();

    let kept = retained(&topo, &remaining_roots);
    let freed_names: Vec<String> = topo
        .graph
        .keys()
        .filter(|k| !kept.contains(k.as_str()))
        .cloned()
        .collect();

    let mut blocked = BTreeMap::new();
    for target in targets {
        if kept.contains(target) {
            blocked.insert(target.clone(), dependents(&topo, target, &kept));
        }
    }

    let size_map = resolve_physical_sizes_subset(&topo.pkg_data, &freed_names, prefix, sizer);
    let mut freed: Vec<ReclaimRow> = freed_names
        .into_iter()
        .map(|name| {
            let core_bytes = size_map.get(&name).copied().unwrap_or(0);
            let is_target = target_set.contains(name.as_str());
            ReclaimRow {
                package: name,
                core_bytes,
                is_target,
            }
        })
        .collect();
    freed.sort_by(|a, b| {
        b.core_bytes
            .cmp(&a.core_bytes)
            .then_with(|| a.package.cmp(&b.package))
    });

    Ok(ReclaimResult { freed, blocked })
}

/// List installed dependencies no requested package needs any more (`brew autoremove`).
pub fn build_orphans(metadata: &BrewMetadata, prefix: &Path) -> Vec<OrphanRow> {
    build_orphans_with(metadata, prefix, &FsSizer)
}

/// List orphaned dependencies using a custom [`PhysicalSizer`].
pub fn build_orphans_with(
    metadata: &BrewMetadata,
    prefix: &Path,
    sizer: &dyn PhysicalSizer,
) -> Vec<OrphanRow> {
    let topo = build_global_topology(metadata, false);
    let kept = retained(&topo, &topo.requested);
    let orphans: HashSet<String> = topo
        .graph
        .keys()
        .filter(|k| !kept.contains(k.as_str()))
        .cloned()
        .collect();

    let orphan_list: Vec<String> = orphans.iter().cloned().collect();
    let size_map = resolve_physical_sizes_subset(&topo.pkg_data, &orphan_list, prefix, sizer);

    let mut rows: Vec<OrphanRow> = orphan_list
        .into_iter()
        .map(|name| {
            let core_bytes = size_map.get(&name).copied().unwrap_or(0);
            let required_by = dependents(&topo, &name, &orphans);
            OrphanRow {
                package: name,
                core_bytes,
                required_by,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.core_bytes
            .cmp(&a.core_bytes)
            .then_with(|| a.package.cmp(&b.package))
    });
    rows
}

/// List the older kegs `brew cleanup` would remove from the Cellar.
pub fn build_cleanup(metadata: &BrewMetadata, prefix: &Path) -> Vec<CleanupRow> {
    build_cleanup_with(metadata, prefix, &FsSizer)
}

/// List stale Cellar kegs using a custom [`PhysicalSizer`].
pub fn build_cleanup_with(
    metadata: &BrewMetadata,
    prefix: &Path,
    sizer: &dyn PhysicalSizer,
) -> Vec<CleanupRow> {
    let mut rows: Vec<CleanupRow> = metadata
        .formulae
        .par_iter()
        .flat_map_iter(|formula| {
            let kegs = sizer.keg_sizes(formula, prefix);
            let pkg = formula.name.clone();
            kegs.stale
                .into_iter()
                .map(move |(version, bytes)| CleanupRow {
                    package: pkg.clone(),
                    version,
                    bytes,
                })
        })
        .collect();

    rows.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| a.package.cmp(&b.package))
            .then_with(|| a.version.cmp(&b.version))
    });
    rows
}

/// Compare two `export` snapshots package by package.
///
/// Returns one row per package that was added, removed, or changed size,
/// stably sorted largest absolute core change first.
pub fn diff_snapshots(old: &[SnapshotRow], new: &[SnapshotRow]) -> Vec<DiffRow> {
    let old_map: HashMap<&str, &SnapshotRow> =
        old.iter().map(|row| (row.package.as_str(), row)).collect();
    let new_map: HashMap<&str, &SnapshotRow> =
        new.iter().map(|row| (row.package.as_str(), row)).collect();

    let all_packages: BTreeSet<&str> = old_map
        .keys()
        .copied()
        .chain(new_map.keys().copied())
        .collect();

    let mut rows: Vec<DiffRow> = all_packages
        .into_iter()
        .filter_map(|pkg| {
            let (status, old_core, new_core, old_weighted, new_weighted) =
                match (old_map.get(pkg), new_map.get(pkg)) {
                    (Some(o), Some(n)) => (
                        DiffStatus::Changed,
                        o.core_bytes,
                        n.core_bytes,
                        o.weighted_bytes,
                        n.weighted_bytes,
                    ),
                    (Some(o), None) => (
                        DiffStatus::Removed,
                        o.core_bytes,
                        0.0,
                        o.weighted_bytes,
                        0.0,
                    ),
                    (None, Some(n)) => {
                        (DiffStatus::Added, 0.0, n.core_bytes, 0.0, n.weighted_bytes)
                    }
                    (None, None) => return None,
                };

            let core_delta = new_core - old_core;
            let weighted_delta = new_weighted - old_weighted;

            if status == DiffStatus::Changed
                && core_delta.abs() < 1e-6
                && weighted_delta.abs() < 1e-6
            {
                return None;
            }

            Some(DiffRow {
                package: pkg.to_string(),
                status,
                old_core,
                new_core,
                core_delta,
                weighted_delta,
            })
        })
        .collect();

    rows.sort_by(|a, b| b.core_delta.abs().total_cmp(&a.core_delta.abs()));
    rows
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use std::sync::Mutex;

    use proptest::prelude::*;

    use super::*;
    use crate::model::{Bottle, BottleStable, CaskDependsOn, InstalledKeg, KegSizes, RuntimeDep};

    struct MapPhysicalSizer {
        kegs: HashMap<String, KegSizes>,
        casks: HashMap<String, u64>,
        keg_calls: Mutex<Vec<(String, String)>>,
        cask_calls: Mutex<Vec<(String, String)>>,
    }

    impl MapPhysicalSizer {
        fn from_sizes(sizes: &[(&str, u64)]) -> Self {
            let kegs = sizes
                .iter()
                .map(|(name, size)| {
                    (
                        (*name).to_string(),
                        KegSizes {
                            current: *size,
                            stale: BTreeMap::new(),
                        },
                    )
                })
                .collect();
            Self {
                kegs,
                casks: HashMap::new(),
                keg_calls: Mutex::new(Vec::new()),
                cask_calls: Mutex::new(Vec::new()),
            }
        }

        fn from_kegs(kegs: HashMap<String, KegSizes>) -> Self {
            Self {
                kegs,
                casks: HashMap::new(),
                keg_calls: Mutex::new(Vec::new()),
                cask_calls: Mutex::new(Vec::new()),
            }
        }

        fn with_casks(mut self, casks: &[(&str, u64)]) -> Self {
            self.casks = casks
                .iter()
                .map(|(token, size)| ((*token).to_string(), *size))
                .collect();
            self
        }
    }

    impl PhysicalSizer for MapPhysicalSizer {
        fn keg_sizes(&self, formula: &Formula, prefix: &Path) -> KegSizes {
            self.keg_calls
                .lock()
                .expect("lock")
                .push((formula.name.clone(), prefix.display().to_string()));
            self.kegs.get(&formula.name).cloned().unwrap_or_default()
        }

        fn cask_size(&self, cask: &Cask, prefix: &Path) -> u64 {
            self.cask_calls
                .lock()
                .expect("lock")
                .push((cask.token.clone(), prefix.display().to_string()));
            self.casks.get(&cask.token).copied().unwrap_or(0)
        }
    }

    struct MockTheoreticalSizer {
        detected_tag: String,
        sizes: HashMap<String, u64>,
        resolved_calls: Mutex<Vec<IndexMap<String, String>>>,
    }

    impl MockTheoreticalSizer {
        fn new(detected_tag: &str, sizes: &[(&str, u64)]) -> Self {
            Self {
                detected_tag: detected_tag.to_string(),
                sizes: sizes
                    .iter()
                    .map(|(digest, size)| ((*digest).to_string(), *size))
                    .collect(),
                resolved_calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl TheoreticalSizer for MockTheoreticalSizer {
        fn detect_bottle_tag(&self) -> String {
            self.detected_tag.clone()
        }

        fn resolve_bottle_sizes(&self, bottles: &IndexMap<String, String>) -> HashMap<String, u64> {
            self.resolved_calls
                .lock()
                .expect("lock")
                .push(bottles.clone());
            bottles
                .keys()
                .filter_map(|digest| self.sizes.get(digest).map(|&size| (digest.clone(), size)))
                .collect()
        }
    }

    fn formula(name: &str, deps: &[&str], requested: Option<bool>) -> Formula {
        Formula {
            name: name.to_string(),
            dependencies: deps.iter().map(|&d| d.to_string()).collect(),
            installed: requested.map_or_else(Vec::new, |req| {
                vec![InstalledKeg {
                    version: None,
                    installed_on_request: req,
                    runtime_dependencies: Vec::new(),
                }]
            }),
            ..Formula::default()
        }
    }

    fn installed_formula(name: &str, deps: &[&str], requested: bool, recorded: &[&str]) -> Formula {
        Formula {
            name: name.to_string(),
            dependencies: deps.iter().map(|&d| d.to_string()).collect(),
            installed: vec![InstalledKeg {
                version: Some("1.0".to_string()),
                installed_on_request: requested,
                runtime_dependencies: recorded
                    .iter()
                    .map(|&d| RuntimeDep {
                        full_name: Some(d.to_string()),
                    })
                    .collect(),
            }],
            ..Formula::default()
        }
    }

    fn mock_brew_metadata() -> BrewMetadata {
        BrewMetadata {
            formulae: vec![
                formula("pkg-a", &["pkg-b", "pkg-c"], None),
                formula("pkg-b", &["pkg-d"], None),
                formula("pkg-c", &["pkg-d"], None),
                formula("pkg-d", &[], None),
            ],
            casks: Vec::new(),
        }
    }

    fn mock_brew_metadata_with_bottles(tag: &str) -> BrewMetadata {
        let mut meta = mock_brew_metadata();
        for f in &mut meta.formulae {
            let mut files = IndexMap::new();
            files.insert(
                tag.to_string(),
                BottleFile {
                    sha256: format!("sha-{}", f.name),
                    url: format!("https://ghcr.io/v2/homebrew/core/{}/blobs/sha", f.name),
                },
            );
            f.bottle = Some(Bottle {
                stable: Some(BottleStable { files }),
            });
        }
        meta
    }

    fn mock_brew_sizes() -> [(&'static str, u64); 4] {
        [
            ("pkg-a", 100),
            ("pkg-b", 200),
            ("pkg-c", 300),
            ("pkg-d", 600),
        ]
    }

    fn attribute(
        formulae: Vec<Formula>,
        sizes: &[(&str, u64)],
        catalog: bool,
    ) -> HashMap<String, BloatRow> {
        let meta = BrewMetadata {
            formulae,
            casks: Vec::new(),
        };
        let topo = build_global_topology(&meta, catalog);
        let size_map: HashMap<String, u64> =
            sizes.iter().map(|(k, v)| ((*k).to_string(), *v)).collect();
        let targets: Vec<&str> = topo.graph.keys().map(String::as_str).collect();
        compute_bloat_metrics(&targets, &size_map, &topo)
            .into_iter()
            .map(|row| (row.package.clone(), row))
            .collect()
    }

    fn catalog_with_cask() -> BrewMetadata {
        BrewMetadata {
            formulae: vec![formula("ghostscript", &[], None)],
            casks: vec![Cask {
                token: "mactex".to_string(),
                depends_on: CaskDependsOn {
                    formula: vec!["ghostscript".to_string()],
                    cask: Vec::new(),
                },
                ..Cask::default()
            }],
        }
    }

    fn s(values: &[&str]) -> Vec<String> {
        values.iter().map(|&v| v.to_string()).collect()
    }

    fn set(values: &[&str]) -> HashSet<String> {
        values.iter().map(|&v| v.to_string()).collect()
    }

    #[test]
    fn test_get_all_dependencies() {
        let mut graph = IndexMap::new();
        graph.insert("pkg-a".to_string(), s(&["pkg-b", "pkg-c"]));
        graph.insert("pkg-b".to_string(), s(&["pkg-d"]));
        graph.insert("pkg-c".to_string(), s(&["pkg-d"]));
        graph.insert("pkg-d".to_string(), Vec::new());

        assert_eq!(
            get_all_dependencies("pkg-a", &graph),
            set(&["pkg-b", "pkg-c", "pkg-d"])
        );
        assert_eq!(get_all_dependencies("pkg-d", &graph), HashSet::new());
    }

    #[test]
    fn test_get_all_dependencies_handles_cycles() {
        let mut graph = IndexMap::new();
        graph.insert("a".to_string(), s(&["b"]));
        graph.insert("b".to_string(), s(&["a"]));

        assert_eq!(get_all_dependencies("a", &graph), set(&["a", "b"]));
    }

    #[test]
    fn test_build_analysis() {
        let meta = mock_brew_metadata();
        let sizer = MapPhysicalSizer::from_sizes(&mock_brew_sizes());
        let rows = build_analysis_with(&meta, Path::new("/mock/prefix"), &sizer);

        assert_eq!(rows.len(), 4);
        let row_a = rows.iter().find(|r| r.package == "pkg-a").expect("pkg-a");

        assert_eq!(row_a.standard_bytes, 1200);
        assert_eq!(row_a.weighted_bytes, 1200.0);
        assert_eq!(row_a.bloat_ratio, Some(12.0));
        assert!(row_a.is_root);
        assert!(!row_a.is_requested);
    }

    #[test]
    fn test_build_theoretical() {
        let mut meta = mock_brew_metadata();
        let mut files = IndexMap::new();
        files.insert(
            "arm64_tahoe".to_string(),
            BottleFile {
                sha256: "mock-digest".to_string(),
                url: "https://ghcr.io/v2/homebrew/core/pkg-a/blobs/sha256:mock-digest".to_string(),
            },
        );
        meta.formulae[0].bottle = Some(Bottle {
            stable: Some(BottleStable { files }),
        });

        let sizer = MockTheoreticalSizer::new("arm64_tahoe", &[("mock-digest", 500)]);
        let rows = build_theoretical_with(&meta, Some("arm64_tahoe"), &sizer);

        assert_eq!(rows.len(), 4);
        let row_a = rows.iter().find(|r| r.package == "pkg-a").expect("pkg-a");
        assert_eq!(row_a.core_bytes, 500);
    }

    #[test]
    fn test_build_targeted_analysis_missing() {
        let meta = mock_brew_metadata();
        let sizer = MapPhysicalSizer::from_sizes(&[]);
        let err = build_targeted_analysis_with(&meta, Path::new("/mock"), "missing-pkg", &sizer)
            .expect_err("expected missing error");
        assert!(
            err.to_string()
                .contains("not found in the local installation")
        );
    }

    #[test]
    fn test_build_compare_analysis_reports_all_missing() {
        let meta = mock_brew_metadata();
        let sizer = MapPhysicalSizer::from_sizes(&[]);
        let err = build_compare_analysis_with(
            &meta,
            Path::new("/mock"),
            &s(&["pkg-b", "nope-1", "nope-2"]),
            &sizer,
        )
        .expect_err("expected missing error");

        match &err {
            DarkMatterError::PackageNotFound { packages, location } => {
                assert_eq!(packages, &s(&["nope-1", "nope-2"]));
                assert_eq!(*location, INSTALLED);
            }
            other => panic!("expected PackageNotFound, got {other:?}"),
        }
        assert!(
            err.to_string()
                .contains("Packages 'nope-1', 'nope-2' not found")
        );
    }

    #[test]
    fn test_build_compare_theoretical_missing() {
        let meta = mock_brew_metadata();
        let sizer = MockTheoreticalSizer::new("arm64_tahoe", &[]);
        let err = build_compare_theoretical_with(&meta, &s(&["pkg-a", "nope"]), None, &sizer)
            .expect_err("expected missing error");

        assert!(matches!(err, DarkMatterError::PackageNotFound { .. }));
        assert!(err.to_string().contains("not found in the catalog"));
    }

    #[test]
    fn test_build_explain_analysis_valid() {
        let meta = mock_brew_metadata();
        let sizer = MapPhysicalSizer::from_sizes(&mock_brew_sizes());
        let rows = build_explain_analysis_with(&meta, Path::new("/mock"), "pkg-a", &sizer)
            .expect("explain");

        let deps: HashSet<&str> = rows.iter().map(|r| r.dependency.as_str()).collect();
        assert!(deps.contains("pkg-b"));
        assert!(deps.contains("pkg-c"));
        assert!(deps.contains("pkg-d"));
    }

    #[test]
    fn test_build_targeted_theoretical_bounds() {
        let meta = mock_brew_metadata();
        let sizer = MockTheoreticalSizer::new("arm64_tahoe", &[]);
        let err =
            build_targeted_theoretical_with(&meta, "non-existent", Some("arm64_tahoe"), &sizer)
                .expect_err("expected missing error");
        assert!(err.to_string().contains("not found in the catalog"));
    }

    #[test]
    fn test_compare_keeps_accurate_root_flags() {
        let meta = mock_brew_metadata();
        let sizer = MapPhysicalSizer::from_sizes(&[
            ("pkg-a", 10),
            ("pkg-b", 10),
            ("pkg-c", 10),
            ("pkg-d", 10),
        ]);

        let rows =
            build_compare_analysis_with(&meta, Path::new("/mock"), &s(&["pkg-a", "pkg-d"]), &sizer)
                .expect("compare");
        let flags: HashMap<&str, bool> = rows
            .iter()
            .map(|r| (r.package.as_str(), r.is_root))
            .collect();
        assert_eq!(flags.get("pkg-a"), Some(&true));
        assert_eq!(flags.get("pkg-d"), Some(&false));

        let single =
            build_targeted_analysis_with(&meta, Path::new("/mock"), "pkg-d", &sizer).expect("one");
        assert!(!single[0].is_root);
    }

    #[test]
    fn test_select_bottle_file() {
        type BottleCase<'a> = (&'a [(&'a str, &'a str)], &'a str, Option<&'a str>);

        let make_files = |entries: &[(&str, &str)]| -> IndexMap<String, BottleFile> {
            entries
                .iter()
                .map(|(tag, label)| {
                    (
                        (*tag).to_string(),
                        BottleFile {
                            sha256: (*label).to_string(),
                            url: format!("https://example.invalid/{label}"),
                        },
                    )
                })
                .collect()
        };

        let cases: &[BottleCase<'_>] = &[
            (
                &[("arm64_tahoe", "exact"), ("all", "any")],
                "arm64_tahoe",
                Some("exact"),
            ),
            (
                &[("all", "any"), ("arm64_sequoia", "older")],
                "arm64_tahoe",
                Some("any"),
            ),
            (
                &[("sequoia", "intel"), ("arm64_sequoia", "older")],
                "arm64_tahoe",
                Some("older"),
            ),
            (&[("x86_64_linux", "linux")], "arm64_tahoe", None),
            (&[("arm64_sequoia", "mac-arm")], "sequoia", None),
            // Bottles built for a newer macOS are never poured on an older one.
            (&[("arm64_tahoe", "newer")], "arm64_sequoia", None),
            (
                &[
                    ("arm64_tahoe", "newer"),
                    ("arm64_sonoma", "older"),
                    ("arm64_ventura", "old"),
                ],
                "arm64_sequoia",
                Some("older"),
            ),
            (
                &[("sonoma", "newer"), ("monterey", "older")],
                "ventura",
                Some("older"),
            ),
            // Linux has no fallback beyond 'all'.
            (&[("arm64_linux", "linux-arm")], "x86_64_linux", None),
            (
                &[("arm64_linux", "linux-arm"), ("x86_64_linux", "linux")],
                "x86_64_linux",
                Some("linux"),
            ),
        ];

        for &(entries, tag, expected) in cases {
            let files = make_files(entries);
            let actual = select_bottle_file(&files, tag).map(|f| f.sha256.as_str());
            assert_eq!(actual, expected, "tag={tag}, entries={entries:?}");
        }
    }

    #[test]
    fn test_theoretical_defaults_to_detected_tag() {
        let mut meta = mock_brew_metadata();
        let mut files = IndexMap::new();
        files.insert(
            "arm64_tahoe".to_string(),
            BottleFile {
                sha256: "mac".to_string(),
                url: "u-mac".to_string(),
            },
        );
        files.insert(
            "x86_64_linux".to_string(),
            BottleFile {
                sha256: "linux".to_string(),
                url: "u-linux".to_string(),
            },
        );
        meta.formulae[0].bottle = Some(Bottle {
            stable: Some(BottleStable { files }),
        });

        let sizer = MockTheoreticalSizer::new("x86_64_linux", &[]);
        build_targeted_theoretical_with(&meta, "pkg-a", None, &sizer).expect("targeted");

        let calls = sizer.resolved_calls.lock().expect("lock");
        assert_eq!(calls.len(), 1);
        let mut expected = IndexMap::new();
        expected.insert("linux".to_string(), "u-linux".to_string());
        assert_eq!(calls[0], expected);
    }

    #[test]
    fn test_topology_keys_casks_by_token_with_dependencies() {
        let metadata = BrewMetadata {
            formulae: vec![
                formula("docker", &[], None),
                formula("ghostscript", &[], None),
            ],
            casks: vec![
                Cask {
                    token: "docker".to_string(),
                    name: s(&["Docker Desktop"]),
                    ..Cask::default()
                },
                Cask {
                    token: "mactex-no-gui".to_string(),
                    name: s(&["MacTeX"]),
                    depends_on: CaskDependsOn {
                        formula: s(&["ghostscript"]),
                        cask: s(&["docker"]),
                    },
                    ..Cask::default()
                },
            ],
        };

        let topo = build_global_topology(&metadata, false);
        let keys: HashSet<String> = topo.graph.keys().cloned().collect();
        assert_eq!(
            keys,
            set(&["docker", "ghostscript", "docker (cask)", "mactex-no-gui"])
        );
        assert_eq!(
            topo.graph.get("mactex-no-gui"),
            Some(&s(&["ghostscript", "docker"]))
        );
        assert!(matches!(
            topo.pkg_data.get("docker (cask)"),
            Some(PackageData::Cask(c)) if c.token == "docker"
        ));
        assert_eq!(topo.requested, set(&["docker (cask)", "mactex-no-gui"]));
    }

    #[test]
    fn test_topology_resolves_tap_qualified_dependencies() {
        let mut app = formula("app", &["user/tap/lib"], None);
        app.full_name = Some("user/tap/app".to_string());
        let mut lib = formula("lib", &[], None);
        lib.full_name = Some("user/tap/lib".to_string());

        let metadata = BrewMetadata {
            formulae: vec![app, lib],
            casks: vec![Cask {
                token: "gui".to_string(),
                depends_on: CaskDependsOn {
                    formula: s(&["user/tap/lib"]),
                    cask: Vec::new(),
                },
                ..Cask::default()
            }],
        };

        let topo = build_global_topology(&metadata, true);
        assert_eq!(topo.graph.get("app"), Some(&s(&["lib"])));
        assert_eq!(topo.trans_deps.get("app"), Some(&set(&["lib"])));
        assert_eq!(topo.roots, set(&["app"]));

        let installed = build_global_topology(&metadata, false);
        assert_eq!(installed.graph.get("gui"), Some(&s(&["lib"])));
    }

    #[test]
    fn test_physical_sizes_route_casks_and_formulae() {
        let sizer =
            MapPhysicalSizer::from_sizes(&[("wget", 10)]).with_casks(&[("brave-browser", 99)]);
        let mut pkg_map = IndexMap::new();
        pkg_map.insert(
            "wget".to_string(),
            PackageData::Formula(formula("wget", &[], None)),
        );
        pkg_map.insert(
            "brave-browser".to_string(),
            PackageData::Cask(Cask {
                token: "brave-browser".to_string(),
                ..Cask::default()
            }),
        );

        let size_map = resolve_physical_sizes(&pkg_map, Path::new("/brew"), &sizer);
        assert_eq!(size_map.get("wget"), Some(&10));
        assert_eq!(size_map.get("brave-browser"), Some(&99));
        assert_eq!(
            *sizer.keg_calls.lock().expect("lock"),
            vec![("wget".to_string(), "/brew".to_string())]
        );
        assert_eq!(
            *sizer.cask_calls.lock().expect("lock"),
            vec![("brave-browser".to_string(), "/brew".to_string())]
        );
    }

    #[test]
    fn test_build_compare_theoretical_resolves_union() {
        let meta = mock_brew_metadata_with_bottles("arm64_tahoe");
        let sizer = MockTheoreticalSizer::new(
            "arm64_tahoe",
            &[
                ("sha-pkg-a", 100),
                ("sha-pkg-b", 200),
                ("sha-pkg-c", 300),
                ("sha-pkg-d", 600),
            ],
        );

        let rows = build_compare_theoretical_with(&meta, &s(&["pkg-b", "pkg-c"]), None, &sizer)
            .expect("compare");

        let calls = sizer.resolved_calls.lock().expect("lock");
        let resolved_keys: HashSet<String> = calls[0].keys().cloned().collect();
        assert_eq!(resolved_keys, set(&["sha-pkg-b", "sha-pkg-c", "sha-pkg-d"]));
        let pkgs: HashSet<&str> = rows.iter().map(|r| r.package.as_str()).collect();
        assert_eq!(pkgs, HashSet::from(["pkg-b", "pkg-c"]));
    }

    #[test]
    fn test_build_explain_theoretical_sorted() {
        let meta = mock_brew_metadata_with_bottles("arm64_tahoe");
        let sizer = MockTheoreticalSizer::new(
            "arm64_tahoe",
            &[
                ("sha-pkg-a", 100),
                ("sha-pkg-b", 200),
                ("sha-pkg-c", 300),
                ("sha-pkg-d", 600),
            ],
        );

        let rows = build_explain_theoretical_with(&meta, "pkg-a", None, &sizer).expect("explain");

        let deps: Vec<&str> = rows.iter().map(|r| r.dependency.as_str()).collect();
        let archive_bytes: Vec<u64> = rows.iter().map(|r| r.size_bytes).collect();
        let attributed: Vec<f64> = rows.iter().map(|r| r.attributed_bytes).collect();
        let shared: Vec<usize> = rows.iter().map(|r| r.shared_by).collect();

        assert_eq!(deps, vec!["pkg-d", "pkg-c", "pkg-b"]);
        assert_eq!(archive_bytes, vec![600, 300, 200]);
        assert_eq!(attributed, vec![600.0, 300.0, 200.0]);
        assert_eq!(shared, vec![1, 1, 1]);
    }

    #[test]
    fn test_build_explain_leaf_has_no_rows() {
        let meta = mock_brew_metadata_with_bottles("arm64_tahoe");
        let sizer = MockTheoreticalSizer::new("arm64_tahoe", &[("sha-pkg-d", 600)]);
        let rows = build_explain_theoretical_with(&meta, "pkg-d", None, &sizer).expect("explain");
        assert_eq!(rows, Vec::new());
    }

    #[test]
    fn test_attribution_splits_diamond_between_roots() {
        let rows = attribute(
            vec![
                formula("a", &["c"], None),
                formula("b", &["c"], None),
                formula("c", &[], None),
            ],
            &[("a", 10), ("b", 10), ("c", 100)],
            false,
        );
        assert_eq!(rows["a"].weighted_bytes, 60.0);
        assert_eq!(rows["b"].weighted_bytes, 60.0);
    }

    #[test]
    fn test_attribution_ignores_intermediate_sharers() {
        let rows = attribute(
            vec![
                formula("a", &["b"], None),
                formula("b", &["c"], None),
                formula("d", &["c"], None),
                formula("c", &[], None),
            ],
            &[("a", 0), ("b", 0), ("d", 0), ("c", 100)],
            false,
        );
        assert_eq!(rows["a"].weighted_bytes, 50.0);
        assert_eq!(rows["d"].weighted_bytes, 50.0);
        assert!(!rows["b"].is_root);
    }

    #[test]
    fn test_attribution_requested_dependency_pays_for_itself() {
        let rows = attribute(
            vec![
                formula("x", &["python"], Some(true)),
                formula("python", &["openssl"], Some(true)),
                formula("openssl", &[], Some(false)),
            ],
            &[("x", 10), ("python", 400), ("openssl", 100)],
            false,
        );
        assert!(rows["python"].is_root);
        assert!(rows["python"].is_requested);
        assert_eq!(rows["x"].weighted_bytes, 60.0);
        assert_eq!(rows["python"].weighted_bytes, 450.0);
    }

    #[test]
    fn test_attribution_orphans_become_roots() {
        let rows = attribute(
            vec![
                formula("app", &[], Some(true)),
                formula("o", &["p"], Some(false)),
                formula("p", &[], Some(false)),
            ],
            &[("app", 5), ("o", 10), ("p", 20)],
            false,
        );
        assert!(rows["o"].is_root);
        assert!(!rows["o"].is_requested);
        assert_eq!(rows["o"].weighted_bytes, 30.0);
        assert!(!rows["p"].is_root);
    }

    #[test]
    fn test_catalog_topology_excludes_casks() {
        let topo = build_global_topology(&catalog_with_cask(), true);
        let keys: HashSet<String> = topo.graph.keys().cloned().collect();
        assert_eq!(keys, set(&["ghostscript"]));
        assert_eq!(topo.roots, set(&["ghostscript"]));
    }

    #[test]
    fn test_catalog_targets_reject_casks() {
        let meta = catalog_with_cask();
        let sizer = MockTheoreticalSizer::new("arm64_tahoe", &[]);

        let errs = [
            build_targeted_theoretical_with(&meta, "mactex", None, &sizer).unwrap_err(),
            build_compare_theoretical_with(&meta, &s(&["ghostscript", "mactex"]), None, &sizer)
                .unwrap_err(),
            build_explain_theoretical_with(&meta, "mactex", None, &sizer).unwrap_err(),
        ];

        for err in errs {
            assert!(
                matches!(err, DarkMatterError::CaskInCatalog { .. }),
                "expected CaskInCatalog, got {err:?}"
            );
            assert!(err.to_string().contains("'mactex' is a cask"));
        }
    }

    #[test]
    fn test_catalog_mode_ignores_request_flags() {
        let rows = attribute(
            vec![
                formula("x", &["lib"], Some(true)),
                formula("lib", &[], Some(true)),
            ],
            &[("x", 1), ("lib", 10)],
            true,
        );
        assert!(!rows["lib"].is_root);
        assert_eq!(rows["x"].weighted_bytes, 11.0);
    }

    #[test]
    fn test_zero_core_ratio_is_undefined() {
        let rows = attribute(
            vec![formula("meta", &["lib"], None), formula("lib", &[], None)],
            &[("meta", 0), ("lib", 5)],
            false,
        );
        assert_eq!(rows["meta"].bloat_ratio, None);
        assert_eq!(rows["lib"].bloat_ratio, Some(1.0));
    }

    #[test]
    fn test_explain_sums_to_weighted_size() {
        let formulae = vec![
            formula("x", &["python", "zlib"], Some(true)),
            formula("y", &["zlib"], Some(true)),
            formula("python", &["openssl", "zlib"], Some(true)),
            formula("openssl", &[], Some(false)),
            formula("zlib", &[], Some(false)),
        ];
        let sizes: HashMap<String, u64> = [
            ("x", 7),
            ("y", 3),
            ("python", 400),
            ("openssl", 90),
            ("zlib", 30),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

        let meta = BrewMetadata {
            formulae,
            casks: Vec::new(),
        };
        let topo = build_global_topology(&meta, false);
        let weighted = &compute_bloat_metrics(&["x"], &sizes, &topo)[0];
        let explained = compute_explain_metrics("x", &sizes, &topo);

        let sum_explained: f64 = explained.iter().map(|r| r.attributed_bytes).sum();
        assert!(((sizes["x"] as f64 + sum_explained) - weighted.weighted_bytes).abs() < 1e-9);

        let python = explained
            .iter()
            .find(|r| r.dependency == "python")
            .expect("python");
        assert!(python.is_root);
        assert_eq!(python.shared_by, 0);
        assert_eq!(python.attributed_bytes, 0.0);
    }

    fn random_installation() -> impl Strategy<Value = (Vec<Formula>, HashMap<String, u64>, bool)> {
        (1_usize..=12).prop_flat_map(|n| {
            let edges_strat: Vec<_> = (0..n)
                .map(|i| {
                    if i + 1 < n {
                        proptest::collection::hash_set((i + 1)..n, 0..(n - i)).boxed()
                    } else {
                        Just(HashSet::new()).boxed()
                    }
                })
                .collect();
            let reqs_strat = proptest::collection::vec(any::<bool>(), n);
            let sizes_strat = proptest::collection::vec(0_u64..=1_000_000_000, n);
            (edges_strat, reqs_strat, sizes_strat, any::<bool>()).prop_map(
                move |(edge_sets, reqs, raw_sizes, catalog)| {
                    let mut formulae = Vec::with_capacity(n);
                    let mut sizes = HashMap::with_capacity(n);
                    for i in 0..n {
                        let name = format!("p{i}");
                        let mut dep_indices: Vec<usize> = edge_sets[i].iter().copied().collect();
                        dep_indices.sort_unstable();
                        let deps: Vec<String> =
                            dep_indices.into_iter().map(|j| format!("p{j}")).collect();
                        formulae.push(Formula {
                            name: name.clone(),
                            dependencies: deps,
                            installed: vec![InstalledKeg {
                                version: None,
                                installed_on_request: reqs[i],
                                runtime_dependencies: Vec::new(),
                            }],
                            ..Formula::default()
                        });
                        sizes.insert(name, raw_sizes[i]);
                    }
                    (formulae, sizes, catalog)
                },
            )
        })
    }

    proptest! {
        #[test]
        fn test_attribution_conserves_mass((formulae, sizes, catalog) in random_installation()) {
            let meta = BrewMetadata {
                formulae,
                casks: Vec::new(),
            };
            let topo = build_global_topology(&meta, catalog);
            let targets: Vec<&str> = topo.graph.keys().map(String::as_str).collect();
            let rows = compute_bloat_metrics(&targets, &sizes, &topo);

            let total_measured: u64 = sizes.values().sum();
            let root_weighted_sum: f64 = rows
                .iter()
                .filter(|r| r.is_root)
                .map(|r| r.weighted_bytes)
                .sum();

            let expected = total_measured as f64;
            let tol = (expected.abs() * 1e-9).max(1e-9);
            prop_assert!(
                (root_weighted_sum - expected).abs() <= tol,
                "root_weighted_sum={root_weighted_sum}, expected={expected}"
            );
            for row in rows.iter().filter(|r| r.is_root) {
                prop_assert!(row.weighted_bytes >= row.core_bytes as f64);
            }
        }
    }

    #[test]
    fn test_recorded_dependencies_extend_closures() {
        let mut old_lib = formula("old-lib", &[], Some(false));
        old_lib.full_name = Some("homebrew/core/old-lib".to_string());
        let metadata = BrewMetadata {
            formulae: vec![
                installed_formula("app", &[], true, &["homebrew/core/old-lib"]),
                old_lib,
            ],
            casks: Vec::new(),
        };

        let topo = build_global_topology(&metadata, false);
        assert_eq!(topo.graph.get("app"), Some(&Vec::new()));
        assert_eq!(topo.trans_deps.get("app"), Some(&set(&["old-lib"])));
        assert_eq!(topo.roots, set(&["app"]));
    }

    #[test]
    fn test_reclaim_frees_exclusive_dependencies() {
        let sizer = MapPhysicalSizer::from_sizes(&[("app", 10), ("only", 20), ("shared", 40)]);
        let metadata = BrewMetadata {
            formulae: vec![
                installed_formula("app", &["only", "shared"], true, &[]),
                installed_formula("other", &["shared"], true, &[]),
                installed_formula("only", &[], false, &[]),
                installed_formula("shared", &[], false, &[]),
            ],
            casks: Vec::new(),
        };

        let result = build_reclaim_with(&metadata, Path::new("/brew"), &s(&["app"]), &sizer)
            .expect("reclaim");

        let pkgs: Vec<&str> = result.freed.iter().map(|r| r.package.as_str()).collect();
        let flags: Vec<bool> = result.freed.iter().map(|r| r.is_target).collect();
        assert_eq!(pkgs, vec!["only", "app"]);
        assert_eq!(flags, vec![false, true]);
        assert_eq!(result.blocked, BTreeMap::new());
    }

    #[test]
    fn test_reclaim_reports_blocked_targets() {
        let sizer = MapPhysicalSizer::from_sizes(&[("app", 10)]);
        let metadata = BrewMetadata {
            formulae: vec![
                installed_formula("app", &[], true, &[]),
                installed_formula("x", &["lib"], true, &[]),
                installed_formula("y", &[], true, &["lib"]),
                installed_formula("lib", &[], true, &[]),
            ],
            casks: Vec::new(),
        };

        let result = build_reclaim_with(&metadata, Path::new("/brew"), &s(&["lib", "app"]), &sizer)
            .expect("reclaim");

        let mut expected_blocked = BTreeMap::new();
        expected_blocked.insert("lib".to_string(), s(&["x"]));
        assert_eq!(result.blocked, expected_blocked);
        let pkgs: Vec<&str> = result.freed.iter().map(|r| r.package.as_str()).collect();
        assert_eq!(pkgs, vec!["app"]);
    }

    #[test]
    fn test_reclaim_unknown_package() {
        let meta = mock_brew_metadata();
        let sizer = MapPhysicalSizer::from_sizes(&[]);
        let err = build_reclaim_with(&meta, Path::new("/brew"), &s(&["nope"]), &sizer)
            .expect_err("expected missing error");
        assert!(matches!(err, DarkMatterError::PackageNotFound { .. }));
    }

    #[test]
    fn test_reclaim_nothing_freed_is_empty() {
        let sizer = MapPhysicalSizer::from_sizes(&[]);
        let metadata = BrewMetadata {
            formulae: vec![
                installed_formula("x", &["lib"], true, &[]),
                installed_formula("lib", &[], true, &[]),
            ],
            casks: Vec::new(),
        };

        let result = build_reclaim_with(&metadata, Path::new("/brew"), &s(&["lib"]), &sizer)
            .expect("reclaim");
        assert_eq!(result.freed, Vec::new());
    }

    #[test]
    fn test_orphans_match_autoremove() {
        let sizer = MapPhysicalSizer::from_sizes(&[("o", 5), ("p", 7)]);
        let metadata = BrewMetadata {
            formulae: vec![
                installed_formula("app", &[], true, &["kept-by-record"]),
                installed_formula("kept-by-record", &[], false, &[]),
                installed_formula("o", &["p"], false, &[]),
                installed_formula("p", &[], false, &[]),
                installed_formula("cask-dep", &[], false, &[]),
            ],
            casks: vec![Cask {
                token: "some-app".to_string(),
                depends_on: CaskDependsOn {
                    formula: s(&["cask-dep"]),
                    cask: Vec::new(),
                },
                ..Cask::default()
            }],
        };

        let rows = build_orphans_with(&metadata, Path::new("/brew"), &sizer);
        let pkgs: Vec<&str> = rows.iter().map(|r| r.package.as_str()).collect();
        let reqs: Vec<&[String]> = rows.iter().map(|r| r.required_by.as_slice()).collect();

        assert_eq!(pkgs, vec!["p", "o"]);
        assert_eq!(reqs, vec![s(&["o"]).as_slice(), &[]]);
    }

    #[test]
    fn test_cleanup_lists_stale_kegs() {
        let mut kegs = HashMap::new();
        kegs.insert(
            "a".to_string(),
            KegSizes {
                current: 1,
                stale: BTreeMap::from([("1.0".to_string(), 5)]),
            },
        );
        kegs.insert(
            "b".to_string(),
            KegSizes {
                current: 1,
                stale: BTreeMap::from([("0.9".to_string(), 50), ("0.8".to_string(), 5)]),
            },
        );
        kegs.insert(
            "c".to_string(),
            KegSizes {
                current: 1,
                stale: BTreeMap::new(),
            },
        );

        let sizer = MapPhysicalSizer::from_kegs(kegs);
        let metadata = BrewMetadata {
            formulae: vec![
                formula("a", &[], None),
                formula("b", &[], None),
                formula("c", &[], None),
            ],
            casks: Vec::new(),
        };

        let rows = build_cleanup_with(&metadata, Path::new("/brew"), &sizer);
        assert_eq!(
            rows,
            vec![
                CleanupRow {
                    package: "b".to_string(),
                    version: "0.9".to_string(),
                    bytes: 50,
                },
                CleanupRow {
                    package: "a".to_string(),
                    version: "1.0".to_string(),
                    bytes: 5,
                },
                CleanupRow {
                    package: "b".to_string(),
                    version: "0.8".to_string(),
                    bytes: 5,
                },
            ]
        );
    }

    #[test]
    fn test_cleanup_none() {
        let mut kegs = HashMap::new();
        kegs.insert(
            "a".to_string(),
            KegSizes {
                current: 1,
                stale: BTreeMap::new(),
            },
        );
        let sizer = MapPhysicalSizer::from_kegs(kegs);
        let metadata = BrewMetadata {
            formulae: vec![formula("a", &[], None)],
            casks: Vec::new(),
        };

        let rows = build_cleanup_with(&metadata, Path::new("/b"), &sizer);
        assert_eq!(rows, Vec::new());
    }

    #[test]
    fn test_physical_sizes_exclude_stale_kegs() {
        let mut kegs = HashMap::new();
        kegs.insert(
            "a".to_string(),
            KegSizes {
                current: 10,
                stale: BTreeMap::from([("0.9".to_string(), 99)]),
            },
        );
        let sizer = MapPhysicalSizer::from_kegs(kegs);
        let mut pkg_map = IndexMap::new();
        pkg_map.insert(
            "a".to_string(),
            PackageData::Formula(formula("a", &[], None)),
        );

        let size_map = resolve_physical_sizes(&pkg_map, Path::new("/b"), &sizer);
        assert_eq!(size_map.get("a"), Some(&10));
    }

    fn snap(pkg: &str, core: f64, weighted: f64) -> SnapshotRow {
        SnapshotRow {
            package: pkg.to_string(),
            core_bytes: core,
            weighted_bytes: weighted,
        }
    }

    #[test]
    fn test_diff_snapshots() {
        let old = vec![
            snap("same", 5.0, 9.0),
            snap("grew", 10.0, 20.0),
            snap("gone", 3.0, 4.0),
        ];
        let new = vec![
            snap("same", 5.0, 9.0),
            snap("grew", 30.0, 45.0),
            snap("new", 7.0, 8.0),
        ];

        let rows = diff_snapshots(&old, &new);
        assert_eq!(
            rows,
            vec![
                DiffRow {
                    package: "grew".to_string(),
                    status: DiffStatus::Changed,
                    old_core: 10.0,
                    new_core: 30.0,
                    core_delta: 20.0,
                    weighted_delta: 25.0,
                },
                DiffRow {
                    package: "new".to_string(),
                    status: DiffStatus::Added,
                    old_core: 0.0,
                    new_core: 7.0,
                    core_delta: 7.0,
                    weighted_delta: 8.0,
                },
                DiffRow {
                    package: "gone".to_string(),
                    status: DiffStatus::Removed,
                    old_core: 3.0,
                    new_core: 0.0,
                    core_delta: -3.0,
                    weighted_delta: -4.0,
                },
            ]
        );
    }

    #[test]
    fn test_diff_snapshots_keeps_weighted_only_change() {
        let old = vec![snap("pkg", 10.0, 20.0)];
        let new = vec![snap("pkg", 10.0, 35.0)];

        let rows = diff_snapshots(&old, &new);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, DiffStatus::Changed);
        assert_eq!(rows[0].core_delta, 0.0);
        assert_eq!(rows[0].weighted_delta, 15.0);
    }

    #[test]
    fn test_explain_required_by_falls_back_to_target() {
        let metadata = BrewMetadata {
            formulae: vec![
                installed_formula("app", &["lib"], true, &["lib", "ghost"]),
                installed_formula("lib", &[], false, &[]),
                installed_formula("ghost", &[], false, &[]),
            ],
            casks: Vec::new(),
        };

        let topo = build_global_topology(&metadata, false);
        let rows = compute_explain_metrics("app", &HashMap::new(), &topo);
        let required_by: HashMap<&str, &[String]> = rows
            .iter()
            .map(|r| (r.dependency.as_str(), r.required_by.as_slice()))
            .collect();

        assert_eq!(required_by.get("lib"), Some(&s(&["app"]).as_slice()));
        assert_eq!(required_by.get("ghost"), Some(&s(&["app"]).as_slice()));
    }
}
