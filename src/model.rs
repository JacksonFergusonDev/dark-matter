use std::collections::BTreeMap;
use std::fmt;

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    let opt = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

/// Supported sorting metrics for bloat tables.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum SortKey {
    #[default]
    Ratio,
    Core,
    Recursive,
}

/// Supported serialized output formats for the export command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ExportFormat {
    #[default]
    Csv,
    Json,
}

/// Supported physical or theoretical data sources.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ExportSource {
    #[default]
    Installed,
    Catalog,
}

/// Parsed Homebrew metadata payload (`brew info --json=v2` or local API cache).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BrewMetadata {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub formulae: Vec<Formula>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub casks: Vec<Cask>,
}

/// A single Homebrew formula entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Formula {
    pub name: String,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub dependencies: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub installed: Vec<InstalledKeg>,
    #[serde(default)]
    pub linked_keg: Option<String>,
    #[serde(default)]
    pub versions: Option<Versions>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub revision: u64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub pinned: bool,
    #[serde(default)]
    pub bottle: Option<Bottle>,
}

/// An installed keg record inside a formula's `installed` array.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InstalledKeg {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub installed_on_request: bool,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub runtime_dependencies: Vec<RuntimeDep>,
}

/// A runtime dependency recorded on an installed keg.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RuntimeDep {
    #[serde(default)]
    pub full_name: Option<String>,
}

/// Version metadata for a formula.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Versions {
    #[serde(default)]
    pub stable: Option<String>,
}

/// Bottle container inside a formula entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Bottle {
    #[serde(default)]
    pub stable: Option<BottleStable>,
}

/// Stable bottle file map, preserving Homebrew's newest-first ordering.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BottleStable {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub files: IndexMap<String, BottleFile>,
}

/// A single bottle archive entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BottleFile {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub sha256: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub url: String,
}

/// A single Homebrew cask entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Cask {
    pub token: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub name: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub depends_on: CaskDependsOn,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub artifacts: Vec<serde_json::Value>,
}

/// Formula and cask dependencies declared by a cask.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CaskDependsOn {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub formula: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub cask: Vec<String>,
}

/// Unified package metadata stored inside a `Topology`.
#[derive(Debug, Clone, PartialEq)]
pub enum PackageData {
    Formula(Formula),
    Cask(Cask),
}

impl PackageData {
    /// Whether this package was explicitly requested by the user.
    pub fn is_requested(&self) -> bool {
        match self {
            Self::Cask(_) => true,
            Self::Formula(formula) => formula.installed.iter().any(|k| k.installed_on_request),
        }
    }

    pub fn is_cask(&self) -> bool {
        matches!(self, Self::Cask(_))
    }

    pub fn as_formula(&self) -> Option<&Formula> {
        match self {
            Self::Formula(f) => Some(f),
            Self::Cask(_) => None,
        }
    }
}

/// A formula's Cellar footprint, split into its active keg and removable older kegs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KegSizes {
    pub current: u64,
    pub stale: BTreeMap<String, u64>,
}

/// Primary bloat analysis row (also the CSV/JSON `export` schema).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BloatRow {
    #[serde(rename = "Package")]
    pub package: String,
    #[serde(rename = "Core_Bytes")]
    pub core_bytes: u64,
    #[serde(rename = "Standard_Bytes")]
    pub standard_bytes: u64,
    #[serde(rename = "Weighted_Bytes")]
    pub weighted_bytes: f64,
    #[serde(rename = "Bloat_Ratio")]
    pub bloat_ratio: Option<f64>,
    #[serde(rename = "Dep_Count")]
    pub dep_count: usize,
    #[serde(rename = "Is_Root")]
    pub is_root: bool,
    #[serde(rename = "Is_Requested")]
    pub is_requested: bool,
}

/// One transitive dependency's attributed cost breakdown for `explain`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExplainRow {
    pub dependency: String,
    pub size_bytes: u64,
    pub shared_by: usize,
    pub attributed_bytes: f64,
    pub is_root: bool,
    pub required_by: Vec<String>,
}

/// A package that would be removed by `reclaim`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimRow {
    pub package: String,
    pub core_bytes: u64,
    pub is_target: bool,
}

/// Outcome of simulating `brew uninstall <targets> && brew autoremove`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReclaimResult {
    pub freed: Vec<ReclaimRow>,
    pub blocked: BTreeMap<String, Vec<String>>,
}

/// An orphaned dependency removable by `brew autoremove`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanRow {
    pub package: String,
    pub core_bytes: u64,
    pub required_by: Vec<String>,
}

/// A stale Cellar keg removable by `brew cleanup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupRow {
    pub package: String,
    pub version: String,
    pub bytes: u64,
}

/// Minimal snapshot row required by `diff`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotRow {
    #[serde(rename = "Package")]
    pub package: String,
    #[serde(rename = "Core_Bytes")]
    pub core_bytes: f64,
    #[serde(rename = "Weighted_Bytes")]
    pub weighted_bytes: f64,
}

/// Package status in a snapshot diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStatus {
    Added,
    Removed,
    Changed,
}

impl DiffStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Changed => "changed",
        }
    }
}

impl fmt::Display for DiffStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One changed, added, or removed package between two `export` snapshots.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffRow {
    pub package: String,
    pub status: DiffStatus,
    pub old_core: f64,
    pub new_core: f64,
    pub core_delta: f64,
    pub weighted_delta: f64,
}
