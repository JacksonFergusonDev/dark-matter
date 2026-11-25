use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use ahash::{AHashMap, AHashSet};
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

#[derive(Clone, Copy)]
enum PkgRef<'a> {
    Formula(&'a Formula),
    Cask(&'a Cask),
}

impl<'a> PkgRef<'a> {
    fn is_requested(self) -> bool {
        match self {
            Self::Cask(_) => true,
            Self::Formula(formula) => formula.installed.iter().any(|k| k.installed_on_request),
        }
    }

    fn as_formula(self) -> Option<&'a Formula> {
        match self {
            Self::Formula(formula) => Some(formula),
            Self::Cask(_) => None,
        }
    }

    fn to_owned_data(self) -> PackageData {
        match self {
            Self::Formula(formula) => PackageData::Formula(formula.clone()),
            Self::Cask(cask) => PackageData::Cask(cask.clone()),
        }
    }
}

#[inline]
fn set_bit(bits: &mut [u64], id: usize) {
    bits[id >> 6] |= 1u64 << (id & 63);
}

#[inline]
fn test_bit(bits: &[u64], id: usize) -> bool {
    (bits[id >> 6] & (1u64 << (id & 63))) != 0
}

#[inline]
fn or_bits(dst: &mut [u64], src: &[u64]) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d |= s;
    }
}

fn count_bits(bits: &[u64]) -> usize {
    bits.iter().map(|w| w.count_ones() as usize).sum()
}

fn iter_bits(bits: &[u64]) -> impl Iterator<Item = usize> + '_ {
    bits.iter().enumerate().flat_map(|(word_idx, &word)| {
        let base = word_idx * 64;
        let mut remaining = word;
        std::iter::from_fn(move || {
            if remaining == 0 {
                return None;
            }
            let tz = remaining.trailing_zeros() as usize;
            remaining &= remaining - 1;
            Some(base + tz)
        })
    })
}

fn intern_name(
    name: &str,
    names: &mut Vec<String>,
    name_to_id: &mut AHashMap<String, usize>,
) -> usize {
    if let Some(&id) = name_to_id.get(name) {
        return id;
    }
    let id = names.len();
    let owned = name.to_string();
    names.push(owned.clone());
    name_to_id.insert(owned, id);
    id
}

/// Compute transitive closure bitsets for `0..num_pkgs` via iterative Tarjan SCC.
///
/// Tarjan emits SCCs in reverse topological order, so every outgoing edge into
/// an earlier SCC can OR its already-completed bitset directly into the current
/// SCC's accumulator.
fn compute_closure_bits(num_pkgs: usize, words: usize, closure_adj: &[Vec<usize>]) -> Vec<u64> {
    let mut closure_bits = vec![0u64; num_pkgs * words];
    if num_pkgs == 0 || words == 0 {
        return closure_bits;
    }

    let mut index = vec![usize::MAX; num_pkgs];
    let mut lowlink = vec![0usize; num_pkgs];
    let mut on_stack = vec![false; num_pkgs];
    let mut completed = vec![false; num_pkgs];
    let mut tarjan_stack: Vec<usize> = Vec::with_capacity(num_pkgs);
    let mut call_stack: Vec<(usize, usize)> = Vec::new();
    let mut scc: Vec<usize> = Vec::new();
    let mut acc = vec![0u64; words];
    let mut next_index = 0usize;

    for start in 0..num_pkgs {
        if index[start] != usize::MAX {
            continue;
        }
        index[start] = next_index;
        lowlink[start] = next_index;
        next_index += 1;
        tarjan_stack.push(start);
        on_stack[start] = true;
        call_stack.push((start, 0));

        while let Some(&mut (u, ref mut edge_idx)) = call_stack.last_mut() {
            if *edge_idx < closure_adj[u].len() {
                let v = closure_adj[u][*edge_idx];
                *edge_idx += 1;
                if v >= num_pkgs {
                    continue;
                }
                if index[v] == usize::MAX {
                    index[v] = next_index;
                    lowlink[v] = next_index;
                    next_index += 1;
                    tarjan_stack.push(v);
                    on_stack[v] = true;
                    call_stack.push((v, 0));
                } else if on_stack[v] {
                    lowlink[u] = lowlink[u].min(index[v]);
                }
            } else {
                call_stack.pop();
                if lowlink[u] == index[u] {
                    scc.clear();
                    while let Some(w) = tarjan_stack.pop() {
                        on_stack[w] = false;
                        scc.push(w);
                        if w == u {
                            break;
                        }
                    }

                    acc.fill(0);
                    for &node in &scc {
                        for &v in &closure_adj[node] {
                            set_bit(&mut acc, v);
                            if v < num_pkgs && completed[v] {
                                let src = &closure_bits[v * words..(v + 1) * words];
                                or_bits(&mut acc, src);
                            }
                        }
                    }
                    for &node in &scc {
                        closure_bits[node * words..(node + 1) * words].copy_from_slice(&acc);
                        completed[node] = true;
                    }
                }
                if let Some(&(parent, _)) = call_stack.last() {
                    lowlink[parent] = lowlink[parent].min(lowlink[u]);
                }
            }
        }
    }

    closure_bits
}

/// Compact integer-indexed DAG topology borrowing directly from [`BrewMetadata`].
struct IndexedTopology<'a> {
    num_pkgs: usize,
    num_all: usize,
    words: usize,
    names: Vec<String>,
    name_to_id: AHashMap<String, usize>,
    pkg_data: Vec<PkgRef<'a>>,
    graph: Vec<Vec<usize>>,
    closure_bits: Vec<u64>,
    is_root: Vec<bool>,
    is_requested: Vec<bool>,
    sharer_count: Vec<usize>,
}

fn compute_root_stats(
    num_pkgs: usize,
    num_all: usize,
    words: usize,
    pkg_data: &[PkgRef<'_>],
    closure_adj: &[Vec<usize>],
    closure_bits: &[u64],
    catalog: bool,
) -> (Vec<bool>, Vec<bool>, Vec<usize>) {
    let mut has_dependents = vec![false; num_pkgs];
    for adj in closure_adj {
        for &v in adj {
            if v < num_pkgs {
                has_dependents[v] = true;
            }
        }
    }

    let is_requested: Vec<bool> = if catalog {
        vec![false; num_pkgs]
    } else {
        pkg_data.iter().map(|p| p.is_requested()).collect()
    };

    let is_root: Vec<bool> = (0..num_pkgs)
        .map(|i| !has_dependents[i] || is_requested[i])
        .collect();

    // Only roots share costs: counting intermediate dependencies as sharers
    // would hand part of each shared package to rows that are never summed.
    let mut sharer_count = vec![0usize; num_all];
    let mut root_count = 0usize;
    let mut req_count = 0usize;
    for r in 0..num_pkgs {
        if is_requested[r] {
            req_count += 1;
        }
        if !is_root[r] {
            continue;
        }
        root_count += 1;
        let row = &closure_bits[r * words..(r + 1) * words];
        for dep in iter_bits(row) {
            if dep >= num_pkgs || !is_root[dep] {
                sharer_count[dep] += 1;
            }
        }
    }

    log::debug!("Roots: {root_count} ({req_count} requested)");
    (is_root, is_requested, sharer_count)
}

impl<'a> IndexedTopology<'a> {
    fn build(metadata: &'a BrewMetadata, catalog: bool) -> Self {
        // Casks ship no bottles, so the catalog has nothing to size them by;
        // leaving them in would let unsized casks take shares of formulae.
        let casks: &[Cask] = if catalog { &[] } else { &metadata.casks };
        let est_pkgs = metadata.formulae.len() + casks.len();

        let short_names: AHashMap<&str, &str> = metadata
            .formulae
            .iter()
            .map(|f| {
                let full = f.full_name.as_deref().unwrap_or(f.name.as_str());
                (full, f.name.as_str())
            })
            .collect();

        let mut names: Vec<String> = Vec::with_capacity(est_pkgs);
        let mut name_to_id: AHashMap<String, usize> = AHashMap::with_capacity(est_pkgs);
        let mut pkg_data: Vec<PkgRef<'a>> = Vec::with_capacity(est_pkgs);

        for formula in &metadata.formulae {
            if let Some(&id) = name_to_id.get(&formula.name) {
                pkg_data[id] = PkgRef::Formula(formula);
            } else {
                let id = names.len();
                names.push(formula.name.clone());
                name_to_id.insert(formula.name.clone(), id);
                pkg_data.push(PkgRef::Formula(formula));
            }
        }

        for cask in casks {
            // Formulae and casks share a namespace in the graph; disambiguate
            // collisions (e.g. the `docker` formula vs. the `docker` cask).
            let key = if name_to_id.contains_key(&cask.token) {
                format!("{} (cask)", cask.token)
            } else {
                cask.token.clone()
            };
            if let Some(&id) = name_to_id.get(&key) {
                pkg_data[id] = PkgRef::Cask(cask);
            } else {
                let id = names.len();
                names.push(key.clone());
                name_to_id.insert(key, id);
                pkg_data.push(PkgRef::Cask(cask));
            }
        }

        let num_pkgs = names.len();
        let (graph, closure_adj) = Self::build_adjacency(
            num_pkgs,
            &pkg_data,
            &short_names,
            &mut names,
            &mut name_to_id,
        );

        log::debug!(
            "Dependency graph: {num_pkgs} packages ({} casks)",
            casks.len()
        );

        let num_all = names.len();
        let words = num_all.div_ceil(64);
        let closure_bits = compute_closure_bits(num_pkgs, words, &closure_adj);

        let (is_root, is_requested, sharer_count) = compute_root_stats(
            num_pkgs,
            num_all,
            words,
            &pkg_data,
            &closure_adj,
            &closure_bits,
            catalog,
        );

        Self {
            num_pkgs,
            num_all,
            words,
            names,
            name_to_id,
            pkg_data,
            graph,
            closure_bits,
            is_root,
            is_requested,
            sharer_count,
        }
    }

    fn build_adjacency(
        num_pkgs: usize,
        pkg_data: &[PkgRef<'a>],
        short_names: &AHashMap<&str, &str>,
        names: &mut Vec<String>,
        name_to_id: &mut AHashMap<String, usize>,
    ) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
        let mut graph: Vec<Vec<usize>> = Vec::with_capacity(num_pkgs);
        let mut closure_adj: Vec<Vec<usize>> = Vec::with_capacity(num_pkgs);
        let mut seen: AHashSet<usize> = AHashSet::new();

        for &pkg in &pkg_data[..num_pkgs] {
            seen.clear();
            match pkg {
                PkgRef::Formula(formula) => {
                    // Dependencies on other tap formulae may use the full `user/tap/name`.
                    let mut deps = Vec::with_capacity(formula.dependencies.len());
                    let mut closure = Vec::with_capacity(formula.dependencies.len());
                    for d in &formula.dependencies {
                        let resolved = short_names.get(d.as_str()).copied().unwrap_or(d.as_str());
                        let dep_id = intern_name(resolved, names, name_to_id);
                        deps.push(dep_id);
                        if seen.insert(dep_id) {
                            closure.push(dep_id);
                        }
                    }
                    for keg in &formula.installed {
                        for dep in &keg.runtime_dependencies {
                            if let Some(full_name) = dep.full_name.as_deref() {
                                let resolved =
                                    short_names.get(full_name).copied().unwrap_or(full_name);
                                let dep_id = intern_name(resolved, names, name_to_id);
                                if seen.insert(dep_id) {
                                    closure.push(dep_id);
                                }
                            }
                        }
                    }
                    graph.push(deps);
                    closure_adj.push(closure);
                }
                PkgRef::Cask(cask) => {
                    let cap = cask.depends_on.formula.len() + cask.depends_on.cask.len();
                    let mut deps = Vec::with_capacity(cap);
                    let mut closure = Vec::with_capacity(cap);
                    for d in &cask.depends_on.formula {
                        let resolved = short_names.get(d.as_str()).copied().unwrap_or(d.as_str());
                        let dep_id = intern_name(resolved, names, name_to_id);
                        deps.push(dep_id);
                        if seen.insert(dep_id) {
                            closure.push(dep_id);
                        }
                    }
                    for d in &cask.depends_on.cask {
                        let dep_id = intern_name(d.as_str(), names, name_to_id);
                        deps.push(dep_id);
                        if seen.insert(dep_id) {
                            closure.push(dep_id);
                        }
                    }
                    graph.push(deps);
                    closure_adj.push(closure);
                }
            }
        }

        (graph, closure_adj)
    }

    #[inline]
    fn pkg_id(&self, name: &str) -> Option<usize> {
        self.name_to_id
            .get(name)
            .copied()
            .filter(|&id| id < self.num_pkgs)
    }

    #[inline]
    fn closure_slice(&self, id: usize) -> &[u64] {
        &self.closure_bits[id * self.words..(id + 1) * self.words]
    }

    #[inline]
    fn has_trans_dep(&self, pkg: usize, dep: usize) -> bool {
        test_bit(self.closure_slice(pkg), dep)
    }

    #[inline]
    fn is_root_id(&self, id: usize) -> bool {
        id < self.num_pkgs && self.is_root[id]
    }

    fn into_topology(self) -> Topology {
        let mut graph = IndexMap::with_capacity(self.num_pkgs);
        let mut pkg_data = IndexMap::with_capacity(self.num_pkgs);
        let mut trans_deps = HashMap::with_capacity(self.num_pkgs);
        let mut roots = HashSet::new();
        let mut requested = HashSet::new();

        for i in 0..self.num_pkgs {
            let name = self.names[i].clone();
            let direct: Vec<String> = self.graph[i]
                .iter()
                .map(|&d| self.names[d].clone())
                .collect();
            let trans: HashSet<String> = iter_bits(self.closure_slice(i))
                .map(|d| self.names[d].clone())
                .collect();

            if self.is_root[i] {
                roots.insert(name.clone());
            }
            if self.is_requested[i] {
                requested.insert(name.clone());
            }

            graph.insert(name.clone(), direct);
            pkg_data.insert(name.clone(), self.pkg_data[i].to_owned_data());
            trans_deps.insert(name, trans);
        }

        let mut sharer_count = HashMap::new();
        for (id, &count) in self.sharer_count.iter().enumerate() {
            if count > 0 {
                sharer_count.insert(self.names[id].clone(), count);
            }
        }

        Topology {
            graph,
            trans_deps,
            roots,
            requested,
            sharer_count,
            pkg_data,
        }
    }
}

fn require_targets_indexed<S: AsRef<str>>(
    targets: &[S],
    topo: &IndexedTopology<'_>,
    location: &'static str,
) -> Result<Vec<usize>> {
    let mut ids = Vec::with_capacity(targets.len());
    let mut missing = Vec::new();
    for target in targets {
        let name = target.as_ref();
        if let Some(id) = topo.pkg_id(name) {
            ids.push(id);
        } else {
            missing.push(name.to_string());
        }
    }
    if !missing.is_empty() {
        return Err(DarkMatterError::PackageNotFound {
            packages: missing,
            location,
        });
    }
    Ok(ids)
}

fn require_catalog_targets_indexed<S: AsRef<str>>(
    targets: &[S],
    topo: &IndexedTopology<'_>,
    metadata: &BrewMetadata,
) -> Result<Vec<usize>> {
    let tokens: AHashSet<&str> = metadata.casks.iter().map(|c| c.token.as_str()).collect();
    let casks: Vec<String> = targets
        .iter()
        .filter_map(|t| {
            let name = t.as_ref();
            (topo.pkg_id(name).is_none() && tokens.contains(name)).then(|| name.to_string())
        })
        .collect();
    if !casks.is_empty() {
        return Err(DarkMatterError::CaskInCatalog { casks });
    }
    require_targets_indexed(targets, topo, CATALOG)
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
    IndexedTopology::build(metadata, catalog).into_topology()
}

fn measure_package(pkg: PkgRef<'_>, prefix: &Path, sizer: &dyn PhysicalSizer) -> u64 {
    match pkg {
        PkgRef::Cask(cask) => sizer.cask_size(cask, prefix),
        // Older kegs are cleanup debris, not part of the package's cost.
        PkgRef::Formula(formula) => sizer.current_keg_size(formula, prefix),
    }
}

fn resolve_physical_sizes_indexed(
    topo: &IndexedTopology<'_>,
    prefix: &Path,
    sizer: &dyn PhysicalSizer,
) -> Vec<u64> {
    let mut byte_sizes = vec![0u64; topo.num_all];
    let measured: Vec<u64> = topo.pkg_data[..topo.num_pkgs]
        .par_iter()
        .map(|&pkg| measure_package(pkg, prefix, sizer))
        .collect();
    byte_sizes[..topo.num_pkgs].copy_from_slice(&measured);
    byte_sizes
}

fn resolve_physical_sizes_subset_indexed(
    topo: &IndexedTopology<'_>,
    ids: &[usize],
    prefix: &Path,
    sizer: &dyn PhysicalSizer,
) -> Vec<u64> {
    let mut byte_sizes = vec![0u64; topo.num_all];
    let measured: Vec<(usize, u64)> = ids
        .par_iter()
        .copied()
        .filter(|&id| id < topo.num_pkgs)
        .map(|id| (id, measure_package(topo.pkg_data[id], prefix, sizer)))
        .collect();
    for (id, bytes) in measured {
        byte_sizes[id] = bytes;
    }
    byte_sizes
}

fn closure_ids(target_ids: &[usize], topo: &IndexedTopology<'_>) -> Vec<usize> {
    let mut bits = vec![0u64; topo.words];
    for &id in target_ids {
        set_bit(&mut bits, id);
        or_bits(&mut bits, topo.closure_slice(id));
    }
    iter_bits(&bits).collect()
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

fn resolve_theoretical_sizes_indexed(
    resolution_ids: &[usize],
    topo: &IndexedTopology<'_>,
    arch: Option<&str>,
    sizer: &dyn TheoreticalSizer,
) -> Vec<u64> {
    let tag = arch
        .filter(|a| !a.is_empty())
        .map_or_else(|| sizer.detect_bottle_tag(), str::to_owned);

    let mut package_digest: Vec<(usize, &str)> = Vec::new();
    let mut bottles: IndexMap<String, String> = IndexMap::new();

    for &id in resolution_ids {
        if id >= topo.num_pkgs {
            continue;
        }
        let Some(formula) = topo.pkg_data[id].as_formula() else {
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
            package_digest.push((id, file_info.sha256.as_str()));
            bottles.insert(file_info.sha256.clone(), file_info.url.clone());
        }
    }

    let unbottled = resolution_ids.len().saturating_sub(package_digest.len());
    if unbottled > 0 {
        log::debug!("{unbottled} package(s) have no {tag} bottle; they count as 0");
    }

    let resolved_sizes = sizer.resolve_bottle_sizes(&bottles);
    let mut byte_sizes = vec![0u64; topo.num_all];
    for (id, digest) in package_digest {
        byte_sizes[id] = resolved_sizes.get(digest).copied().unwrap_or(0);
    }
    byte_sizes
}

#[inline]
fn attributed_size_indexed(dep_id: usize, sizes: &[u64], topo: &IndexedTopology<'_>) -> f64 {
    if topo.is_root_id(dep_id) {
        return 0.0;
    }
    let size = sizes[dep_id];
    let sharers = topo.sharer_count[dep_id].max(1);
    size as f64 / sharers as f64
}

fn compute_bloat_metrics_indexed(
    target_ids: &[usize],
    sizes: &[u64],
    topo: &IndexedTopology<'_>,
) -> Vec<BloatRow> {
    target_ids
        .iter()
        .map(|&target_id| {
            let core_bytes = sizes[target_id];
            let closure = topo.closure_slice(target_id);
            let dep_count = count_bits(closure);

            let mut dep_standard: u64 = 0;
            let mut dep_weighted: f64 = 0.0;
            for dep_id in iter_bits(closure) {
                dep_standard += sizes[dep_id];
                dep_weighted += attributed_size_indexed(dep_id, sizes, topo);
            }

            let standard_bytes = core_bytes + dep_standard;
            let weighted_bytes = core_bytes as f64 + dep_weighted;
            // A package with no measurable core has no meaningful ratio.
            let bloat_ratio = if core_bytes > 0 {
                Some(weighted_bytes / core_bytes as f64)
            } else {
                None
            };

            BloatRow {
                package: topo.names[target_id].clone(),
                core_bytes,
                standard_bytes,
                weighted_bytes,
                bloat_ratio,
                dep_count,
                is_root: topo.is_root[target_id],
                is_requested: topo.is_requested[target_id],
            }
        })
        .collect()
}

fn compute_explain_metrics_indexed(
    target_id: usize,
    sizes: &[u64],
    topo: &IndexedTopology<'_>,
) -> Vec<ExplainRow> {
    let closure = topo.closure_slice(target_id);
    if count_bits(closure) == 0 {
        return Vec::new();
    }

    let mut subgraph_bits = closure.to_vec();
    set_bit(&mut subgraph_bits, target_id);
    let subgraph_pkgs: Vec<usize> = iter_bits(&subgraph_bits)
        .take_while(|&p| p < topo.num_pkgs)
        .collect();

    let mut rows: Vec<ExplainRow> = iter_bits(closure)
        .map(|dep_id| {
            let size_bytes = sizes[dep_id];
            let is_root = topo.is_root_id(dep_id);
            let shared_by = if is_root {
                0
            } else {
                topo.sharer_count[dep_id].max(1)
            };
            let attributed_bytes = attributed_size_indexed(dep_id, sizes, topo);

            // Direct dependents within the target's closure, for tree views. A
            // dependency known only from a keg's install record has no declared
            // parent, so it hangs off the target.
            let mut required_by: Vec<String> = subgraph_pkgs
                .iter()
                .copied()
                .filter(|&p| topo.graph[p].contains(&dep_id))
                .map(|p| topo.names[p].clone())
                .collect();
            required_by.sort();
            if required_by.is_empty() {
                required_by.push(topo.names[target_id].clone());
            }

            ExplainRow {
                dependency: topo.names[dep_id].clone(),
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

fn log_conservation_indexed(rows: &[BloatRow], sizes: &[u64]) {
    let total: u64 = sizes.iter().copied().sum();
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
    let topo = IndexedTopology::build(metadata, false);
    let byte_sizes = resolve_physical_sizes_indexed(&topo, prefix, sizer);
    let target_ids: Vec<usize> = (0..topo.num_pkgs).collect();

    let rows = compute_bloat_metrics_indexed(&target_ids, &byte_sizes, &topo);
    log_conservation_indexed(&rows, &byte_sizes);
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
    let topo = IndexedTopology::build(metadata, false);
    let target_ids = require_targets_indexed(&[target], &topo, INSTALLED)?;

    let scope = closure_ids(&target_ids, &topo);
    let byte_sizes = resolve_physical_sizes_subset_indexed(&topo, &scope, prefix, sizer);
    Ok(compute_bloat_metrics_indexed(
        &target_ids,
        &byte_sizes,
        &topo,
    ))
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
    let topo = IndexedTopology::build(metadata, false);
    let target_ids = require_targets_indexed(targets, &topo, INSTALLED)?;

    let scope = closure_ids(&target_ids, &topo);
    let byte_sizes = resolve_physical_sizes_subset_indexed(&topo, &scope, prefix, sizer);
    Ok(compute_bloat_metrics_indexed(
        &target_ids,
        &byte_sizes,
        &topo,
    ))
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
    let topo = IndexedTopology::build(metadata, false);
    let target_ids = require_targets_indexed(&[target], &topo, INSTALLED)?;

    let scope = closure_ids(&target_ids, &topo);
    let byte_sizes = resolve_physical_sizes_subset_indexed(&topo, &scope, prefix, sizer);
    Ok(compute_explain_metrics_indexed(
        target_ids[0],
        &byte_sizes,
        &topo,
    ))
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
    let topo = IndexedTopology::build(metadata, true);
    let resolution_ids: Vec<usize> = (0..topo.num_pkgs).collect();
    let byte_sizes = resolve_theoretical_sizes_indexed(&resolution_ids, &topo, arch, sizer);

    let rows = compute_bloat_metrics_indexed(&resolution_ids, &byte_sizes, &topo);
    log_conservation_indexed(&rows, &byte_sizes);
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
    let topo = IndexedTopology::build(metadata, true);
    let target_ids = require_catalog_targets_indexed(&[target], &topo, metadata)?;

    let scope = closure_ids(&target_ids, &topo);
    let byte_sizes = resolve_theoretical_sizes_indexed(&scope, &topo, arch, sizer);
    Ok(compute_bloat_metrics_indexed(
        &target_ids,
        &byte_sizes,
        &topo,
    ))
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
    let topo = IndexedTopology::build(metadata, true);
    let target_ids = require_catalog_targets_indexed(targets, &topo, metadata)?;

    let scope = closure_ids(&target_ids, &topo);
    let byte_sizes = resolve_theoretical_sizes_indexed(&scope, &topo, arch, sizer);
    Ok(compute_bloat_metrics_indexed(
        &target_ids,
        &byte_sizes,
        &topo,
    ))
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
    let topo = IndexedTopology::build(metadata, true);
    let target_ids = require_catalog_targets_indexed(&[target], &topo, metadata)?;

    let scope = closure_ids(&target_ids, &topo);
    let byte_sizes = resolve_theoretical_sizes_indexed(&scope, &topo, arch, sizer);
    Ok(compute_explain_metrics_indexed(
        target_ids[0],
        &byte_sizes,
        &topo,
    ))
}

fn dependents_indexed(
    topo: &IndexedTopology<'_>,
    pkg_id: usize,
    among_pkgs: &[usize],
) -> Vec<String> {
    let mut direct: Vec<String> = among_pkgs
        .iter()
        .copied()
        .filter(|&p| topo.graph[p].contains(&pkg_id))
        .map(|p| topo.names[p].clone())
        .collect();
    if !direct.is_empty() {
        direct.sort();
        return direct;
    }

    let mut transitive: Vec<String> = among_pkgs
        .iter()
        .copied()
        .filter(|&p| topo.has_trans_dep(p, pkg_id))
        .map(|p| topo.names[p].clone())
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
    let topo = IndexedTopology::build(metadata, false);
    let target_ids = require_targets_indexed(targets, &topo, INSTALLED)?;

    let mut is_target = vec![false; topo.num_pkgs];
    for &id in &target_ids {
        is_target[id] = true;
    }

    let mut kept_bits = vec![0u64; topo.words];
    for (r, (&root, &targeted)) in topo.is_root.iter().zip(&is_target).enumerate() {
        if root && !targeted {
            set_bit(&mut kept_bits, r);
            or_bits(&mut kept_bits, topo.closure_slice(r));
        }
    }

    let mut freed_ids = Vec::new();
    let mut kept_pkgs = Vec::new();
    for id in 0..topo.num_pkgs {
        if test_bit(&kept_bits, id) {
            kept_pkgs.push(id);
        } else {
            freed_ids.push(id);
        }
    }

    let mut blocked = BTreeMap::new();
    for &tid in &target_ids {
        if test_bit(&kept_bits, tid) {
            blocked.insert(
                topo.names[tid].clone(),
                dependents_indexed(&topo, tid, &kept_pkgs),
            );
        }
    }

    let byte_sizes = resolve_physical_sizes_subset_indexed(&topo, &freed_ids, prefix, sizer);
    let mut freed: Vec<ReclaimRow> = freed_ids
        .into_iter()
        .map(|id| ReclaimRow {
            package: topo.names[id].clone(),
            core_bytes: byte_sizes[id],
            is_target: is_target[id],
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
    let topo = IndexedTopology::build(metadata, false);
    let mut kept_bits = vec![0u64; topo.words];
    for (r, &requested) in topo.is_requested.iter().enumerate() {
        if requested {
            set_bit(&mut kept_bits, r);
            or_bits(&mut kept_bits, topo.closure_slice(r));
        }
    }

    let orphan_ids: Vec<usize> = (0..topo.num_pkgs)
        .filter(|&id| !test_bit(&kept_bits, id))
        .collect();

    let byte_sizes = resolve_physical_sizes_subset_indexed(&topo, &orphan_ids, prefix, sizer);

    let mut rows: Vec<OrphanRow> = orphan_ids
        .iter()
        .copied()
        .map(|id| OrphanRow {
            package: topo.names[id].clone(),
            core_bytes: byte_sizes[id],
            required_by: dependents_indexed(&topo, id, &orphan_ids),
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
            let stale = sizer.stale_keg_sizes(formula, prefix);
            let pkg = formula.name.clone();
            stale.into_iter().map(move |(version, bytes)| CleanupRow {
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
    let old_map: AHashMap<&str, &SnapshotRow> =
        old.iter().map(|row| (row.package.as_str(), row)).collect();
    let new_map: AHashMap<&str, &SnapshotRow> =
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
fn resolve_physical_sizes(
    pkg_data_map: &IndexMap<String, PackageData>,
    prefix: &Path,
    sizer: &dyn PhysicalSizer,
) -> HashMap<String, u64> {
    let entries: Vec<(&String, &PackageData)> = pkg_data_map.iter().collect();
    entries
        .into_par_iter()
        .map(|(name, pkg)| {
            let bytes = match pkg {
                PackageData::Cask(cask) => sizer.cask_size(cask, prefix),
                PackageData::Formula(formula) => sizer.current_keg_size(formula, prefix),
            };
            (name.clone(), bytes)
        })
        .collect()
}

#[cfg(test)]
fn attributed_size(dep: &str, size_map: &HashMap<String, u64>, topo: &Topology) -> f64 {
    if topo.roots.contains(dep) {
        return 0.0;
    }
    let size = size_map.get(dep).copied().unwrap_or(0);
    let sharers = topo.sharer_count.get(dep).copied().unwrap_or(1).max(1);
    size as f64 / sharers as f64
}

#[cfg(test)]
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

#[cfg(test)]
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

    rows.sort_by(|a, b| {
        b.attributed_bytes
            .total_cmp(&a.attributed_bytes)
            .then_with(|| a.dependency.cmp(&b.dependency))
    });
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
