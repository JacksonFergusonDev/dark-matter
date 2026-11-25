use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use indexmap::{IndexMap, IndexSet};
use rayon::prelude::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{DarkMatterError, Result};
use crate::model::{
    Bottle, BottleFile, BottleStable, BrewMetadata, Cask, CaskDependsOn, Formula, KegSizes,
};

pub const DEFAULT_MAX_WORKERS: usize = 16;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
pub const TOKEN_SCOPE_BATCH_SIZE: usize = 50;
pub const BOTTLE_SIZE_CACHE_MAX_ENTRIES: usize = 50_000;

const BREW_NOT_FOUND: &str = "Homebrew (`brew`) was not found on PATH. Is Homebrew installed?";
const GHCR_TOKEN_URL: &str = "https://ghcr.io/token";
const DEFAULT_APPDIR: &str = "/Applications";
const APP_ARTIFACT_KINDS: [&str; 2] = ["app", "suite"];
const NON_RUNTIME_DEPENDENCY_TAGS: [&str; 3] = [":build", ":test", ":optional"];

/// Homebrew bottle tags by macOS major version.
const MACOS_CODENAMES: &[(u32, &str)] = &[
    (11, "big_sur"),
    (12, "monterey"),
    (13, "ventura"),
    (14, "sonoma"),
    (15, "sequoia"),
    (26, "tahoe"),
    (27, "golden_gate"),
];

pub trait BrewRunner: Send + Sync {
    fn run_brew(&self, args: &[&str], failure: &str) -> Result<String>;
}

pub struct SystemBrewRunner;

impl BrewRunner for SystemBrewRunner {
    fn run_brew(&self, args: &[&str], failure: &str) -> Result<String> {
        log::debug!("Running: brew {}", args.join(" "));
        let output = Command::new("brew").args(args).output().map_err(|err| {
            if err.kind() == io::ErrorKind::NotFound {
                DarkMatterError::Pipeline(BREW_NOT_FOUND.to_string())
            } else {
                DarkMatterError::Pipeline(format!("{failure}: {err}"))
            }
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DarkMatterError::Pipeline(format!("{failure}: {stderr}")));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

pub trait BottleFetcher: Send + Sync {
    fn fetch_token(&self, repositories: &[String], timeout: Duration) -> Option<String>;
    fn fetch_blob_size(&self, url: &str, token: &str, timeout: Duration) -> Option<u64>;
}

pub trait PhysicalSizer: Send + Sync {
    fn keg_sizes(&self, formula: &Formula, prefix: &Path) -> KegSizes;
    fn cask_size(&self, cask: &Cask, prefix: &Path) -> u64;
}

pub struct FsSizer;

impl PhysicalSizer for FsSizer {
    fn keg_sizes(&self, formula: &Formula, prefix: &Path) -> KegSizes {
        get_keg_sizes(formula, prefix)
    }

    fn cask_size(&self, cask: &Cask, prefix: &Path) -> u64 {
        get_cask_size(cask, prefix)
    }
}

pub trait TheoreticalSizer: Send + Sync {
    fn detect_bottle_tag(&self) -> String;
    fn resolve_bottle_sizes(&self, bottles: &IndexMap<String, String>) -> HashMap<String, u64>;
}

pub struct DefaultTheoreticalSizer;

impl TheoreticalSizer for DefaultTheoreticalSizer {
    fn detect_bottle_tag(&self) -> String {
        detect_bottle_tag()
    }

    fn resolve_bottle_sizes(&self, bottles: &IndexMap<String, String>) -> HashMap<String, u64> {
        let cache_path = default_bottle_size_cache_path();
        let mut cache = load_bottle_size_cache(&cache_path);
        let resolved =
            resolve_bottle_sizes(bottles, &mut cache, DEFAULT_MAX_WORKERS, DEFAULT_TIMEOUT);
        save_bottle_size_cache(&cache_path, &cache, BOTTLE_SIZE_CACHE_MAX_ENTRIES);
        resolved
    }
}

/// Retrieve the local Homebrew installation prefix.
pub fn get_brew_prefix() -> Result<PathBuf> {
    get_brew_prefix_with(&SystemBrewRunner)
}

/// Retrieve the local Homebrew installation prefix using the provided runner.
pub fn get_brew_prefix_with(runner: &dyn BrewRunner) -> Result<PathBuf> {
    let stdout = runner.run_brew(&["--prefix"], "Failed to resolve Homebrew prefix")?;
    Ok(PathBuf::from(stdout.trim()))
}

/// Retrieve Homebrew's cache directory, honoring `HOMEBREW_CACHE`.
pub fn get_brew_cache() -> Result<PathBuf> {
    get_brew_cache_with(&SystemBrewRunner)
}

/// Retrieve Homebrew's cache directory using the provided runner.
pub fn get_brew_cache_with(runner: &dyn BrewRunner) -> Result<PathBuf> {
    let stdout = runner.run_brew(&["--cache"], "Failed to resolve Homebrew cache")?;
    Ok(PathBuf::from(stdout.trim()))
}

/// Execute `brew info --json=v2 --installed` to extract installed package metadata.
pub fn get_brew_metadata() -> Result<BrewMetadata> {
    get_brew_metadata_with(&SystemBrewRunner)
}

/// Extract installed package metadata using the provided runner.
pub fn get_brew_metadata_with(runner: &dyn BrewRunner) -> Result<BrewMetadata> {
    let stdout = runner.run_brew(
        &["info", "--json=v2", "--installed"],
        "Failed to execute brew API",
    )?;
    serde_json::from_str(&stdout)
        .map_err(|_| DarkMatterError::Pipeline("Failed to parse Homebrew JSON output.".into()))
}

/// Split a macOS bottle tag into its CPU architecture and macOS major version.
pub fn macos_bottle_version(tag: &str) -> Option<(&'static str, u32)> {
    let (arch, codename) = match tag.strip_prefix("arm64_") {
        Some(rest) => ("arm64", rest),
        None => ("x86_64", tag),
    };
    MACOS_CODENAMES
        .iter()
        .find(|&&(_, name)| name == codename)
        .map(|&(major, _)| (arch, major))
}

fn extract_plist_product_version(plist: &str) -> Option<String> {
    let after_key = plist.split("<key>ProductVersion</key>").nth(1)?;
    let after_open = after_key.split("<string>").nth(1)?;
    let value = after_open.split("</string>").next()?.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn current_macos_release() -> Option<String> {
    if let Ok(output) = Command::new("sw_vers").arg("-productVersion").output()
        && output.status.success()
    {
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !version.is_empty() {
            return Some(version);
        }
    }

    let plist = fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    extract_plist_product_version(&plist)
}

/// Infer the Homebrew bottle tag for the current machine.
pub fn detect_bottle_tag() -> String {
    let mac_release = if std::env::consts::OS == "macos" {
        current_macos_release()
    } else {
        None
    };
    detect_bottle_tag_for(
        std::env::consts::OS,
        std::env::consts::ARCH,
        mac_release.as_deref(),
    )
}

/// Infer the Homebrew bottle tag for an explicit OS, CPU architecture, and optional macOS release.
pub fn detect_bottle_tag_for(os: &str, arch: &str, mac_release: Option<&str>) -> String {
    let is_arm = matches!(arch.to_ascii_lowercase().as_str(), "arm64" | "aarch64");

    if os != "macos" && os != "darwin" {
        return if is_arm {
            "arm64_linux".to_string()
        } else {
            "x86_64_linux".to_string()
        };
    }

    let max_known = MACOS_CODENAMES
        .iter()
        .map(|&(version, _)| version)
        .max()
        .unwrap_or(26);
    let major = mac_release
        .filter(|release| !release.is_empty())
        .and_then(|release| release.split('.').next()?.parse::<u32>().ok())
        .unwrap_or(max_known);

    let codename = MACOS_CODENAMES
        .iter()
        .filter(|&&(version, _)| version <= major)
        .max_by_key(|&&(version, _)| version)
        .or_else(|| MACOS_CODENAMES.iter().min_by_key(|&&(version, _)| version))
        .map_or("tahoe", |&(_, name)| name);

    if is_arm {
        format!("arm64_{codename}")
    } else {
        codename.to_string()
    }
}

/// Recursively calculate the aggregate physical byte size of a directory without following symlinks.
pub fn get_directory_size(path: &Path) -> u64 {
    let Ok(meta) = fs::metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return 0;
    }
    dir_size_recursive(path)
}

fn dir_size_recursive(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total: u64 = 0;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_file() {
            if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        } else if ft.is_dir() {
            subdirs.push(entry.path());
        }
    }
    // `entries` was consumed and closed by the loop, so each thread holds at most one FD.
    for subdir in subdirs {
        total += dir_size_recursive(&subdir);
    }
    total
}

/// Calculate the physical byte size of a file or directory. Symlinks and missing paths count as 0.
pub fn get_path_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if meta.is_dir() {
        return get_directory_size(path);
    }
    if meta.is_file() {
        return meta.len();
    }
    0
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn resolve_symlink(link: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = fs::canonicalize(link) {
        return Some(canonical);
    }
    let raw_target = fs::read_link(link).ok()?;
    let joined = if raw_target.is_absolute() {
        raw_target
    } else {
        link.parent()?.join(raw_target)
    };
    Some(normalize_path(&joined))
}

fn current_keg(formula: &Formula, prefix: &Path) -> Option<String> {
    let opt = prefix.join("opt").join(&formula.name);
    if fs::symlink_metadata(&opt).is_ok_and(|meta| meta.file_type().is_symlink())
        && let Some(target) = resolve_symlink(&opt)
        && target.parent().and_then(Path::file_name) == Some(OsStr::new(&formula.name))
        && let Some(name) = target.file_name().and_then(OsStr::to_str)
    {
        return Some(name.to_string());
    }

    if let Some(linked) = formula.linked_keg.as_deref().filter(|s| !s.is_empty()) {
        return Some(linked.to_string());
    }

    formula
        .installed
        .last()
        .and_then(|keg| keg.version.as_deref())
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn latest_keg(formula: &Formula) -> Option<String> {
    let stable = formula
        .versions
        .as_ref()
        .and_then(|versions| versions.stable.as_deref())
        .filter(|s| !s.is_empty())?;
    if formula.revision > 0 {
        Some(format!("{stable}_{}", formula.revision))
    } else {
        Some(stable.to_string())
    }
}

fn keg_link_target(link: &Path) -> Option<String> {
    if !fs::symlink_metadata(link).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return None;
    }
    let target = resolve_symlink(link)?;
    target
        .file_name()
        .and_then(OsStr::to_str)
        .map(ToOwned::to_owned)
}

fn has_live_keepme_refs(keg: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(keg.join(".keepme")) else {
        return false;
    };
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .any(|line| Path::new(line).exists())
}

fn cleanup_candidates<'a>(
    formula: &Formula,
    prefix: &Path,
    kegs: &'a [(String, PathBuf)],
    current: &str,
) -> Vec<&'a (String, PathBuf)> {
    let names: HashSet<&str> = kegs.iter().map(|(name, _)| name.as_str()).collect();
    let Some(latest) = latest_keg(formula) else {
        return Vec::new();
    };
    if !names.contains(latest.as_str()) || names.iter().any(|name| name.starts_with("HEAD")) {
        return Vec::new();
    }

    let pinned = keg_link_target(
        &prefix
            .join("var")
            .join("homebrew")
            .join("pinned")
            .join(&formula.name),
    );

    let mut keep: HashSet<&str> = HashSet::from([current, latest.as_str()]);
    if let Some(linked) = formula.linked_keg.as_deref() {
        keep.insert(linked);
    }
    if let Some(pinned_name) = pinned.as_deref() {
        keep.insert(pinned_name);
    }

    kegs.iter()
        .filter(|(name, path)| !keep.contains(name.as_str()) && !has_live_keepme_refs(path))
        .collect()
}

/// Measure a formula's current keg separately from any older kegs `brew cleanup` would remove.
pub fn get_keg_sizes(formula: &Formula, prefix: &Path) -> KegSizes {
    let rack = prefix.join("Cellar").join(&formula.name);
    let current = current_keg(formula, prefix);

    let mut kegs: Vec<(String, PathBuf)> = match fs::read_dir(&rack) {
        Ok(entries) => entries
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| {
                let ft = entry.file_type().ok()?;
                if ft.is_dir() && !ft.is_symlink() {
                    let name = entry.file_name().into_string().ok()?;
                    Some((name, entry.path()))
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    kegs.sort_by(|(a, _), (b, _)| a.cmp(b));

    let Some(current_name) = current.filter(|name| kegs.iter().any(|(k, _)| k == name)) else {
        return KegSizes {
            current: get_directory_size(&rack),
            stale: BTreeMap::new(),
        };
    };

    let current_size = get_directory_size(&rack.join(&current_name));
    let stale = cleanup_candidates(formula, prefix, &kegs, &current_name)
        .into_iter()
        .map(|(name, path)| (name.clone(), get_directory_size(path)))
        .collect();

    KegSizes {
        current: current_size,
        stale,
    }
}

fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

fn cask_artifact_paths(cask: &Cask) -> Vec<PathBuf> {
    let mut paths = IndexSet::new();
    let default_appdir = Path::new(DEFAULT_APPDIR);

    for artifact in &cask.artifacts {
        let Some(obj) = artifact.as_object() else {
            continue;
        };

        if let Some(target) = obj.get("target").and_then(Value::as_str) {
            paths.insert(expand_tilde(target));
            continue;
        }

        for kind in APP_ARTIFACT_KINDS {
            let Some(spec) = obj.get(kind).and_then(Value::as_array) else {
                continue;
            };
            let Some(first) = spec.first().and_then(Value::as_str) else {
                continue;
            };
            let override_target = spec
                .get(1)
                .and_then(Value::as_object)
                .and_then(|opts| opts.get("target"))
                .and_then(Value::as_str);

            let name = override_target.unwrap_or_else(|| {
                Path::new(first)
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or(first)
            });
            paths.insert(default_appdir.join(expand_tilde(name)));
        }
    }

    paths.into_iter().collect()
}

/// Calculate the installed footprint of a cask across its Caskroom and installed artifacts.
pub fn get_cask_size(cask: &Cask, prefix: &Path) -> u64 {
    let caskroom_size = get_directory_size(&prefix.join("Caskroom").join(&cask.token));
    let artifacts_size: u64 = cask_artifact_paths(cask)
        .iter()
        .map(|path| get_path_size(path))
        .sum();
    caskroom_size + artifacts_size
}

fn read_jws_payload_str(jws_path: &Path) -> Option<String> {
    let raw = match fs::read_to_string(jws_path) {
        Ok(content) => content,
        Err(err) => {
            log::error!("Failed to read {}: {err}", jws_path.display());
            return None;
        }
    };

    let jws_data: Value = match serde_json::from_str(&raw) {
        Ok(data) => data,
        Err(err) => {
            log::error!("Failed to read {}: {err}", jws_path.display());
            return None;
        }
    };

    let Some(payload) = jws_data.get("payload").and_then(Value::as_str) else {
        log::error!("JWS payload in {} is not a string.", jws_path.display());
        return None;
    };

    Some(payload.to_string())
}

fn read_jws_payload(jws_path: &Path) -> Option<Value> {
    let payload = read_jws_payload_str(jws_path)?;
    match serde_json::from_str(&payload) {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            log::error!("Failed to parse JWS payload: {err}");
            None
        }
    }
}

fn load_api_cache<T: DeserializeOwned>(json_path: &Path, jws_path: &Path) -> Vec<T> {
    let data = if jws_path.exists() {
        let Some(parsed) = read_jws_payload(jws_path) else {
            return Vec::new();
        };
        parsed
    } else if json_path.exists() {
        match fs::read_to_string(json_path)
            .map_err(|err| err.to_string())
            .and_then(|raw| serde_json::from_str::<Value>(&raw).map_err(|err| err.to_string()))
        {
            Ok(parsed) => parsed,
            Err(err) => {
                log::error!("Failed to read {}: {err}", json_path.display());
                return Vec::new();
            }
        }
    } else {
        return Vec::new();
    };

    if !data.is_array() {
        log::error!("API payload is not a list.");
        return Vec::new();
    }

    match serde_json::from_value(data) {
        Ok(items) => items,
        Err(err) => {
            log::error!("Failed to deserialize API payload: {err}");
            Vec::new()
        }
    }
}

fn is_non_runtime_tag(tag: &str) -> bool {
    NON_RUNTIME_DEPENDENCY_TAGS.contains(&tag)
}

fn runtime_dependencies(entries: &[Value]) -> Vec<String> {
    let mut deps = Vec::new();
    for entry in entries {
        match entry {
            Value::String(name) => deps.push(name.clone()),
            Value::Object(map) => {
                for (name, tags) in map {
                    let excluded = match tags {
                        Value::String(tag) => is_non_runtime_tag(tag),
                        Value::Array(list) => list
                            .iter()
                            .filter_map(Value::as_str)
                            .any(is_non_runtime_tag),
                        _ => false,
                    };
                    if !excluded {
                        deps.push(name.clone());
                    }
                }
            }
            _ => {}
        }
    }
    deps
}

fn ghcr_image_name(formula: &str) -> String {
    formula.replace('@', "/").replace('+', "x")
}

fn normalize_internal_formula(name: &str, formula: &Value, tag: &str) -> Formula {
    let obj = formula.as_object();
    let mut deps = obj
        .and_then(|m| m.get("stable_dependencies"))
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |entries| runtime_dependencies(entries));

    if tag.ends_with("_linux")
        && let Some(uses) = obj
            .and_then(|m| m.get("stable_uses_from_macos"))
            .and_then(Value::as_array)
    {
        let first_elements: Vec<Value> = uses
            .iter()
            .filter_map(|spec| spec.as_array().and_then(|arr| arr.first()).cloned())
            .collect();
        deps.extend(runtime_dependencies(&first_elements));
    }

    let bottle = obj
        .and_then(|m| m.get("bottle_checksum"))
        .and_then(Value::as_str)
        .filter(|checksum| !checksum.is_empty())
        .map(|checksum| {
            let raw_tag = obj
                .and_then(|m| m.get("bottle_tag"))
                .and_then(Value::as_str)
                .unwrap_or(tag);
            let bottle_tag = raw_tag.strip_prefix(':').unwrap_or(raw_tag).to_string();
            let image = ghcr_image_name(name);
            let url = format!("https://ghcr.io/v2/homebrew/core/{image}/blobs/sha256:{checksum}");
            let mut files = IndexMap::new();
            files.insert(
                bottle_tag,
                BottleFile {
                    sha256: checksum.to_string(),
                    url,
                },
            );
            Bottle {
                stable: Some(BottleStable { files }),
            }
        });

    Formula {
        name: name.to_string(),
        dependencies: deps,
        bottle,
        ..Formula::default()
    }
}

fn normalize_internal_cask(token: &str, cask: &Value) -> Cask {
    let obj = cask.as_object();
    let name = obj
        .and_then(|m| m.get("names"))
        .and_then(Value::as_array)
        .map_or_default(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        });

    let formula = match obj
        .and_then(|m| m.get("depends_on_args"))
        .and_then(Value::as_object)
        .and_then(|args| args.get(":formula"))
    {
        Some(Value::String(single)) => vec![single.clone()],
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        _ => Vec::new(),
    };

    Cask {
        token: token.to_string(),
        name,
        depends_on: CaskDependsOn {
            formula,
            cask: Vec::new(),
        },
        artifacts: Vec::new(),
    }
}

#[derive(Deserialize)]
struct InternalCatalogPayload {
    #[serde(default)]
    formulae: IndexMap<String, Value>,
    #[serde(default)]
    casks: IndexMap<String, Value>,
}

fn load_internal_catalog(jws_path: &Path, tag: &str) -> Result<BrewMetadata> {
    let parse_err = || {
        DarkMatterError::Pipeline(format!(
            "Failed to parse Homebrew API cache at {}.",
            jws_path.display()
        ))
    };

    let payload = read_jws_payload_str(jws_path).ok_or_else(parse_err)?;
    let parsed: InternalCatalogPayload = serde_json::from_str(&payload).map_err(|err| {
        log::error!("Failed to parse JWS payload: {err}");
        parse_err()
    })?;

    let formulae = parsed
        .formulae
        .iter()
        .map(|(name, entry)| normalize_internal_formula(name, entry, tag))
        .collect();
    let casks = parsed
        .casks
        .iter()
        .map(|(token, entry)| normalize_internal_cask(token, entry))
        .collect();

    Ok(BrewMetadata { formulae, casks })
}

fn list_cached_internal_tags(internal_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(internal_dir) else {
        return Vec::new();
    };
    let mut tags: Vec<String> = entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let file_name = entry.file_name().into_string().ok()?;
            let rest = file_name.strip_prefix("packages.")?;
            let tag = rest.strip_suffix(".jws.json")?;
            Some(tag.to_string())
        })
        .collect();
    tags.sort();
    tags
}

/// Load Homebrew's local API JSON catalog, preferring Homebrew 7's internal format over legacy files.
pub fn get_theoretical_catalog(api_dir: Option<&Path>, tag: Option<&str>) -> Result<BrewMetadata> {
    get_theoretical_catalog_with(&SystemBrewRunner, api_dir, tag, None)
}

/// Load Homebrew's local API JSON catalog using the provided runner and optional detected tag.
pub fn get_theoretical_catalog_with(
    runner: &dyn BrewRunner,
    api_dir: Option<&Path>,
    tag: Option<&str>,
    detected_tag: Option<&str>,
) -> Result<BrewMetadata> {
    let owned_api_dir;
    let api_dir = if let Some(dir) = api_dir {
        dir
    } else {
        owned_api_dir = get_brew_cache_with(runner)?.join("api");
        &owned_api_dir
    };

    let tag = tag
        .or(detected_tag)
        .map_or_else(detect_bottle_tag, ToOwned::to_owned);

    let internal_dir = api_dir.join("internal");
    let internal_path = internal_dir.join(format!("packages.{tag}.jws.json"));
    if internal_path.exists() {
        log::debug!(
            "Loading Homebrew 7 catalog from {}",
            internal_path.display()
        );
        return load_internal_catalog(&internal_path, &tag);
    }

    let formula_api_path = api_dir.join("formula.json");
    let formula_jws_path = api_dir.join("formula.jws.json");
    if formula_jws_path.exists() || formula_api_path.exists() {
        log::debug!("Loading legacy catalog from {}", api_dir.display());
        return Ok(BrewMetadata {
            formulae: load_api_cache(&formula_api_path, &formula_jws_path),
            casks: load_api_cache(&api_dir.join("cask.json"), &api_dir.join("cask.jws.json")),
        });
    }

    let cached_tags = list_cached_internal_tags(&internal_dir);
    if !cached_tags.is_empty() {
        return Err(DarkMatterError::Pipeline(format!(
            "No Homebrew catalog cached for '{tag}' (available: {}). \
             Homebrew only caches the current platform's catalog; omit --arch or pass a cached tag.",
            cached_tags.join(", ")
        )));
    }

    Err(DarkMatterError::Pipeline(format!(
        "Homebrew API cache not found in {}. Please run `brew update` first.",
        api_dir.display()
    )))
}

fn parse_repository(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://ghcr.io/v2/")?;
    let (repo, _) = rest.rsplit_once("/blobs/sha256:")?;
    if repo.is_empty() { None } else { Some(repo) }
}

fn build_token_query(repositories: &[String]) -> Vec<(&'static str, String)> {
    let mut params = Vec::with_capacity(repositories.len() + 1);
    params.push(("service", "ghcr.io".to_string()));
    for repo in repositories {
        params.push(("scope", format!("repository:{repo}:pull")));
    }
    params
}

fn parse_content_length(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|&size| size > 0)
}

struct GhcrFetcher {
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
    token_url: String,
}

impl GhcrFetcher {
    fn new() -> Option<Self> {
        Self::with_token_url(GHCR_TOKEN_URL.to_string())
    }

    fn with_token_url(token_url: String) -> Option<Self> {
        let client = reqwest::Client::builder().build().ok()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .ok()?;
        Some(Self {
            client,
            runtime,
            token_url,
        })
    }
}

impl BottleFetcher for GhcrFetcher {
    fn fetch_token(&self, repositories: &[String], timeout: Duration) -> Option<String> {
        let params = build_token_query(repositories);
        self.runtime.block_on(async {
            let resp = self
                .client
                .get(&self.token_url)
                .query(&params)
                .timeout(timeout)
                .send()
                .await
                .ok()?;
            let resp = resp.error_for_status().ok()?;
            let body: Value = resp.json().await.ok()?;
            body.get("token")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
    }

    fn fetch_blob_size(&self, url: &str, token: &str, timeout: Duration) -> Option<u64> {
        self.runtime.block_on(async {
            let resp = self
                .client
                .head(url)
                .bearer_auth(token)
                .timeout(timeout)
                .send()
                .await
                .ok()?;
            let resp = resp.error_for_status().ok()?;
            parse_content_length(resp.headers())
        })
    }
}

/// Default on-disk location for the persisted bottle size cache.
pub fn default_bottle_size_cache_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".cache")
        .join("dark-matter")
        .join("bottle_sizes.json")
}

/// Load the persisted sha256-to-size cache from disk, dropping non-positive or malformed entries.
pub fn load_bottle_size_cache(path: &Path) -> IndexMap<String, u64> {
    let Ok(raw) = fs::read_to_string(path) else {
        return IndexMap::new();
    };
    let Ok(data) = serde_json::from_str::<IndexMap<String, Value>>(&raw) else {
        return IndexMap::new();
    };

    data.into_iter()
        .filter_map(|(digest, value)| {
            let size = value.as_u64().filter(|&n| n > 0)?;
            Some((digest, size))
        })
        .collect()
}

fn write_bottle_size_cache(path: &Path, entries: &IndexMap<&str, u64>) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let prefix = format!(
        ".{}.",
        path.file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("bottle_sizes.json")
    );
    let mut tmp = tempfile::Builder::new()
        .prefix(&prefix)
        .tempfile_in(parent)?;
    serde_json::to_writer(&mut tmp, entries).map_err(io::Error::other)?;
    tmp.persist(path).map_err(|err| err.error)?;
    Ok(())
}

/// Persist the sha256-to-size cache atomically, keeping at most `max_entries` newest entries.
pub fn save_bottle_size_cache(path: &Path, cache: &IndexMap<String, u64>, max_entries: usize) {
    let skip = cache.len().saturating_sub(max_entries);
    let trimmed: IndexMap<&str, u64> = cache
        .iter()
        .skip(skip)
        .map(|(k, &v)| (k.as_str(), v))
        .collect();

    if let Err(err) = write_bottle_size_cache(path, &trimmed) {
        log::warn!("Could not write bottle size cache: {err}");
    }
}

/// Resolve compressed bottle sizes via `ghcr.io`, updating `cache` in place for successful lookups.
pub fn resolve_bottle_sizes(
    bottles: &IndexMap<String, String>,
    cache: &mut IndexMap<String, u64>,
    max_workers: usize,
    timeout: Duration,
) -> HashMap<String, u64> {
    struct NoopFetcher;
    impl BottleFetcher for NoopFetcher {
        fn fetch_token(&self, _repositories: &[String], _timeout: Duration) -> Option<String> {
            None
        }
        fn fetch_blob_size(&self, _url: &str, _token: &str, _timeout: Duration) -> Option<u64> {
            None
        }
    }

    let needs_network = bottles
        .iter()
        .any(|(digest, url)| !cache.contains_key(digest) && parse_repository(url).is_some());
    if !needs_network {
        return resolve_bottle_sizes_with(bottles, cache, &NoopFetcher, max_workers, timeout);
    }

    if let Some(fetcher) = GhcrFetcher::new() {
        resolve_bottle_sizes_with(bottles, cache, &fetcher, max_workers, timeout)
    } else {
        resolve_bottle_sizes_with(bottles, cache, &NoopFetcher, max_workers, timeout)
    }
}

/// Resolve compressed bottle sizes using a caller-provided `BottleFetcher`.
pub fn resolve_bottle_sizes_with(
    bottles: &IndexMap<String, String>,
    cache: &mut IndexMap<String, u64>,
    fetcher: &dyn BottleFetcher,
    max_workers: usize,
    timeout: Duration,
) -> HashMap<String, u64> {
    let mut resolved = HashMap::with_capacity(bottles.len());
    let mut pending: Vec<(&str, &str, &str)> = Vec::new();

    for (digest, url) in bottles {
        if let Some(&size) = cache.get(digest) {
            resolved.insert(digest.clone(), size);
            continue;
        }

        if let Some(repository) = parse_repository(url) {
            pending.push((digest.as_str(), url.as_str(), repository));
        } else {
            resolved.insert(digest.clone(), 0);
        }
    }

    log::debug!(
        "Bottle sizes: {} cached or unresolvable, {} to fetch from ghcr.io",
        bottles.len() - pending.len(),
        pending.len()
    );
    if pending.is_empty() {
        return resolved;
    }

    let batches: Vec<&[(&str, &str, &str)]> = pending.chunks(TOKEN_SCOPE_BATCH_SIZE).collect();
    let num_threads = max_workers.max(1).min(pending.len().max(batches.len()));

    let fetch_all = || {
        let tokens: Vec<Option<String>> = batches
            .par_iter()
            .map(|batch| {
                let mut repos: Vec<String> =
                    batch.iter().map(|&(_, _, repo)| repo.to_string()).collect();
                repos.sort();
                repos.dedup();
                fetcher.fetch_token(&repos, timeout)
            })
            .collect();

        let mut blob_tasks: Vec<(&str, &str, &str)> = Vec::new();
        let mut zeroed: Vec<&str> = Vec::new();
        for (batch, token) in batches.iter().zip(tokens.iter()) {
            if let Some(tok) = token.as_deref() {
                for &(digest, url, _) in *batch {
                    blob_tasks.push((digest, url, tok));
                }
            } else {
                for &(digest, _, _) in *batch {
                    zeroed.push(digest);
                }
            }
        }

        let blob_sizes: Vec<(&str, Option<u64>)> = blob_tasks
            .par_iter()
            .map(|&(digest, url, token)| (digest, fetcher.fetch_blob_size(url, token, timeout)))
            .collect();

        (zeroed, blob_sizes)
    };

    let (zeroed, blob_sizes) = match rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build()
    {
        Ok(pool) => pool.install(fetch_all),
        Err(_) => fetch_all(),
    };

    for digest in zeroed {
        resolved.insert(digest.to_string(), 0);
    }

    for (digest, size) in blob_sizes {
        if let Some(valid_size) = size.filter(|&s| s > 0) {
            resolved.insert(digest.to_string(), valid_size);
            cache.insert(digest.to_string(), valid_size);
        } else {
            resolved.insert(digest.to_string(), 0);
        }
    }

    let failed = pending
        .iter()
        .filter(|&&(digest, _, _)| !cache.contains_key(digest))
        .count();
    if failed > 0 {
        log::warn!(
            "Couldn't fetch {failed} of {} bottle sizes from ghcr.io. \
             They count as 0, so sizes and ratios that depend on them are \
             understated; run again to retry them.",
            pending.len()
        );
    }

    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::thread;

    use crate::model::{InstalledKeg, Versions};
    use serde_json::json;
    use tempfile::TempDir;

    struct TestLogger {
        records: Mutex<Vec<String>>,
    }

    impl log::Log for TestLogger {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            if let Ok(mut guard) = self.records.lock() {
                guard.push(format!("{}", record.args()));
            }
        }

        fn flush(&self) {}
    }

    fn test_logger() -> &'static TestLogger {
        static LOGGER: OnceLock<TestLogger> = OnceLock::new();
        let logger = LOGGER.get_or_init(|| TestLogger {
            records: Mutex::new(Vec::new()),
        });
        let _ = log::set_logger(logger);
        log::set_max_level(log::LevelFilter::Trace);
        logger
    }

    fn logs_contain(needle: &str) -> bool {
        test_logger()
            .records
            .lock()
            .expect("lock test logs")
            .iter()
            .any(|msg| msg.contains(needle))
    }

    struct MockBrewRunner {
        response: std::result::Result<String, String>,
        recorded_args: Mutex<Vec<Vec<String>>>,
    }

    impl MockBrewRunner {
        fn ok(stdout: impl Into<String>) -> Self {
            Self {
                response: Ok(stdout.into()),
                recorded_args: Mutex::new(Vec::new()),
            }
        }

        fn err(msg: impl Into<String>) -> Self {
            Self {
                response: Err(msg.into()),
                recorded_args: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.recorded_args.lock().expect("lock args").clone()
        }
    }

    impl BrewRunner for MockBrewRunner {
        fn run_brew(&self, args: &[&str], failure: &str) -> Result<String> {
            self.recorded_args
                .lock()
                .expect("lock args")
                .push(args.iter().map(|s| (*s).to_string()).collect());
            match &self.response {
                Ok(stdout) => Ok(stdout.clone()),
                Err(msg) if msg == BREW_NOT_FOUND => Err(DarkMatterError::Pipeline(msg.clone())),
                Err(stderr) => Err(DarkMatterError::Pipeline(format!("{failure}: {stderr}"))),
            }
        }
    }

    struct MockBottleFetcher<FT, FB>
    where
        FT: Fn(&[String], Duration) -> Option<String> + Send + Sync,
        FB: Fn(&str, &str, Duration) -> Option<u64> + Send + Sync,
    {
        token_fn: FT,
        blob_fn: FB,
        token_calls: AtomicUsize,
        blob_calls: AtomicUsize,
        token_batches: Mutex<Vec<Vec<String>>>,
    }

    impl<FT, FB> MockBottleFetcher<FT, FB>
    where
        FT: Fn(&[String], Duration) -> Option<String> + Send + Sync,
        FB: Fn(&str, &str, Duration) -> Option<u64> + Send + Sync,
    {
        fn new(token_fn: FT, blob_fn: FB) -> Self {
            Self {
                token_fn,
                blob_fn,
                token_calls: AtomicUsize::new(0),
                blob_calls: AtomicUsize::new(0),
                token_batches: Mutex::new(Vec::new()),
            }
        }
    }

    impl<FT, FB> BottleFetcher for MockBottleFetcher<FT, FB>
    where
        FT: Fn(&[String], Duration) -> Option<String> + Send + Sync,
        FB: Fn(&str, &str, Duration) -> Option<u64> + Send + Sync,
    {
        fn fetch_token(&self, repositories: &[String], timeout: Duration) -> Option<String> {
            self.token_calls.fetch_add(1, Ordering::SeqCst);
            self.token_batches
                .lock()
                .expect("lock batches")
                .push(repositories.to_vec());
            (self.token_fn)(repositories, timeout)
        }

        fn fetch_blob_size(&self, url: &str, token: &str, timeout: Duration) -> Option<u64> {
            self.blob_calls.fetch_add(1, Ordering::SeqCst);
            (self.blob_fn)(url, token, timeout)
        }
    }

    fn blob_url(repo: &str, digest: &str) -> String {
        format!("https://ghcr.io/v2/homebrew/core/{repo}/blobs/sha256:{digest}")
    }

    fn write_jws(path: &Path, payload: &Value) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        let wrapper = json!({
            "payload": serde_json::to_string(payload).expect("serialize payload"),
            "signatures": []
        });
        fs::write(
            path,
            serde_json::to_string(&wrapper).expect("serialize jws"),
        )
        .expect("write jws file");
    }

    fn write_jws_raw(path: &Path, payload_json: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        let wrapper = json!({
            "payload": payload_json,
            "signatures": []
        });
        fs::write(
            path,
            serde_json::to_string(&wrapper).expect("serialize jws"),
        )
        .expect("write jws file");
    }

    const INTERNAL_PAYLOAD_JSON: &str = r#"{
        "formulae": {
            "wget": {
                "stable_dependencies": ["openssl@4", {"pkgconf": ":build"}],
                "stable_uses_from_macos": [["zlib"], [{"bison": ":build"}]],
                "bottle_checksum": "abc"
            },
            "python@3.14": {"bottle_checksum": "def", "bottle_tag": ":all"},
            "afl++": {"stable_dependencies": [{"llvm": [":build", ":test"]}]}
        },
        "casks": {
            "brave-browser": {"names": ["Brave"], "depends_on_args": {":macos": ":any"}},
            "azure-cli": {
                "names": ["Azure CLI"],
                "depends_on_args": {":formula": "python@3.14"}
            }
        }
    }"#;

    #[cfg(unix)]
    fn symlink_path(original: &Path, link: &Path) {
        std::os::unix::fs::symlink(original, link).expect("create symlink");
    }

    fn make_rack(prefix: &Path, name: &str, kegs: &[(&str, usize)]) {
        for &(version, size) in kegs {
            let keg = prefix.join("Cellar").join(name).join(version);
            fs::create_dir_all(&keg).expect("create keg dir");
            fs::write(keg.join("payload"), vec![b'x'; size]).expect("write payload");
        }
    }

    #[cfg(unix)]
    fn old_and_new_kegs(prefix: &Path, name: &str) {
        make_rack(prefix, name, &[("1.0", 10), ("2.0", 20)]);
        let opt_dir = prefix.join("opt");
        fs::create_dir_all(&opt_dir).expect("create opt dir");
        symlink_path(
            Path::new(&format!("../Cellar/{name}/2.0")),
            &opt_dir.join(name),
        );
    }

    #[test]
    fn test_get_brew_prefix() {
        let runner = MockBrewRunner::ok("/opt/homebrew\n");
        let prefix = get_brew_prefix_with(&runner).expect("prefix");
        assert_eq!(prefix, PathBuf::from("/opt/homebrew"));
        assert_eq!(runner.calls(), vec![vec!["--prefix".to_string()]]);
    }

    #[test]
    fn test_get_brew_prefix_failure() {
        let runner = MockBrewRunner::err("error");
        let err = get_brew_prefix_with(&runner).expect_err("should fail");
        assert!(
            err.to_string()
                .contains("Failed to resolve Homebrew prefix")
        );
    }

    #[test]
    fn test_get_brew_cache() {
        let runner = MockBrewRunner::ok("/home/user/.cache/Homebrew\n");
        let cache = get_brew_cache_with(&runner).expect("cache");
        assert_eq!(cache, PathBuf::from("/home/user/.cache/Homebrew"));
        assert_eq!(runner.calls(), vec![vec!["--cache".to_string()]]);
    }

    #[test]
    fn test_get_brew_metadata_missing_executable() {
        let runner = MockBrewRunner::err(BREW_NOT_FOUND);
        let err = get_brew_metadata_with(&runner).expect_err("should fail");
        assert!(err.to_string().contains("not found on PATH"));
    }

    #[test]
    fn test_get_brew_metadata_invalid_json() {
        let runner = MockBrewRunner::ok("not json");
        let err = get_brew_metadata_with(&runner).expect_err("should fail");
        assert!(err.to_string().contains("Failed to parse Homebrew JSON"));
    }

    #[test]
    fn test_get_directory_size() {
        let tmp = TempDir::new().expect("tempdir");
        let test_dir = tmp.path().join("target");
        fs::create_dir(&test_dir).expect("mkdir");
        fs::write(test_dir.join("file1.txt"), b"1234567890").expect("write file1");
        let sub_dir = test_dir.join("lib");
        fs::create_dir(&sub_dir).expect("mkdir sub");
        fs::write(sub_dir.join("file2.bin"), vec![b'A'; 15]).expect("write file2");

        assert_eq!(get_directory_size(&test_dir), 25);
    }

    #[cfg(unix)]
    #[test]
    fn test_get_directory_size_permission_error() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().expect("tempdir");
        let protected = tmp.path().join("protected");
        fs::create_dir(&protected).expect("mkdir");
        fs::write(protected.join("secret.bin"), vec![b'x'; 12]).expect("write secret");
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o000)).expect("chmod 000");

        let size = get_directory_size(&protected);
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o755)).expect("chmod 755");
        assert_eq!(size, 0);
    }

    #[cfg(unix)]
    #[test]
    fn test_get_directory_size_skips_unreadable_entries() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().expect("tempdir");
        fs::write(tmp.path().join("ok.bin"), vec![b'x'; 5]).expect("write ok");
        let bad_dir = tmp.path().join("unreadable");
        fs::create_dir(&bad_dir).expect("mkdir bad");
        fs::write(bad_dir.join("bad.bin"), vec![b'x'; 9]).expect("write bad");
        fs::set_permissions(&bad_dir, fs::Permissions::from_mode(0o000)).expect("chmod 000");

        let size = get_directory_size(tmp.path());
        fs::set_permissions(&bad_dir, fs::Permissions::from_mode(0o755)).expect("chmod 755");
        assert_eq!(size, 5);
    }

    #[cfg(unix)]
    #[test]
    fn test_get_path_size() {
        let tmp = TempDir::new().expect("tempdir");
        let app = tmp.path().join("Foo.app");
        fs::create_dir(&app).expect("mkdir app");
        fs::write(app.join("bin"), vec![b'x'; 30]).expect("write bin");
        let font = tmp.path().join("Font.otf");
        fs::write(&font, vec![b'x'; 7]).expect("write font");
        let link = tmp.path().join("foo");
        symlink_path(&app.join("bin"), &link);

        assert_eq!(get_path_size(&app), 30);
        assert_eq!(get_path_size(&font), 7);
        assert_eq!(get_path_size(&link), 0);
        assert_eq!(get_path_size(&tmp.path().join("missing")), 0);
    }

    #[test]
    fn test_fetch_blob_size_header_parsing() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_LENGTH,
            reqwest::header::HeaderValue::from_static("1048576"),
        );
        assert_eq!(parse_content_length(&headers), Some(1_048_576));
    }

    #[test]
    fn test_fetch_blob_size_unusable_headers() {
        let empty = reqwest::header::HeaderMap::new();
        assert_eq!(parse_content_length(&empty), None);

        for invalid in ["0", "x"] {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::CONTENT_LENGTH,
                reqwest::header::HeaderValue::from_static(invalid),
            );
            assert_eq!(parse_content_length(&headers), None);
        }
    }

    #[test]
    fn test_fetch_ghcr_token_query_and_http_roundtrip() {
        let repos = vec!["homebrew/core/a".to_string(), "homebrew/core/b".to_string()];
        assert_eq!(
            build_token_query(&repos),
            vec![
                ("service", "ghcr.io".to_string()),
                ("scope", "repository:homebrew/core/a:pull".to_string()),
                ("scope", "repository:homebrew/core/b:pull".to_string()),
            ]
        );

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            for idx in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf);
                if idx == 0 {
                    let body = r#"{"token":"abc"}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).expect("write token");
                } else {
                    let response =
                        "HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n";
                    stream.write_all(response.as_bytes()).expect("write head");
                }
            }
        });

        let fetcher =
            GhcrFetcher::with_token_url(format!("http://{addr}/token")).expect("create fetcher");
        let token = fetcher.fetch_token(&repos, Duration::from_secs(2));
        assert_eq!(token.as_deref(), Some("abc"));

        let size = fetcher.fetch_blob_size(
            &format!("http://{addr}/v2/homebrew/core/wget/blobs/sha256:123"),
            "abc",
            Duration::from_secs(2),
        );
        assert_eq!(size, Some(1_048_576));

        handle.join().expect("server thread");
    }

    #[test]
    fn test_fetch_ghcr_token_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf);
            let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).expect("write 500");
        });

        let fetcher =
            GhcrFetcher::with_token_url(format!("http://{addr}/token")).expect("create fetcher");
        let token =
            fetcher.fetch_token(&["homebrew/core/wget".to_string()], Duration::from_secs(2));
        assert_eq!(token, None);

        handle.join().expect("server thread");
    }

    #[test]
    fn test_parse_repository_edge_cases() {
        assert_eq!(parse_repository("https://invalid.url/blob"), None);
        assert_eq!(
            parse_repository("https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:abc"),
            Some("homebrew/core/wget")
        );
        assert_eq!(
            parse_repository("https://ghcr.io/v2//blobs/sha256:abc"),
            None
        );
    }

    #[test]
    fn test_load_api_cache_jws_handling() {
        let tmp = TempDir::new().expect("tempdir");
        let jws_file = tmp.path().join("formula.jws.json");
        let json_file = tmp.path().join("formula.json");

        write_jws(
            &jws_file,
            &json!([{"name": "test-pkg", "dependencies": []}]),
        );
        let data: Vec<Formula> = load_api_cache(&json_file, &jws_file);
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].name, "test-pkg");
    }

    #[test]
    fn test_load_api_cache_corrupt_payload() {
        let tmp = TempDir::new().expect("tempdir");
        let jws_file = tmp.path().join("formula.jws.json");
        let json_file = tmp.path().join("formula.json");

        fs::write(
            &jws_file,
            serde_json::to_string(&json!({"payload": "{corrupt-json"})).expect("json"),
        )
        .expect("write");
        let data: Vec<Formula> = load_api_cache(&json_file, &jws_file);
        assert_eq!(data, Vec::new());
    }

    #[test]
    fn test_resolve_bottle_sizes_skips_network_for_cached() {
        let fetcher = MockBottleFetcher::new(|_, _| Some("t".to_string()), |_, _, _| Some(999));
        let mut bottles = IndexMap::new();
        bottles.insert("d1".to_string(), blob_url("wget", "d1"));
        let mut cache = IndexMap::new();
        cache.insert("d1".to_string(), 42);

        let result = resolve_bottle_sizes_with(&bottles, &mut cache, &fetcher, 4, DEFAULT_TIMEOUT);
        assert_eq!(result, HashMap::from([("d1".to_string(), 42)]));
        assert_eq!(fetcher.token_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fetcher.blob_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_resolve_bottle_sizes_does_not_cache_failures() {
        let fetcher = MockBottleFetcher::new(
            |_, _| Some("t".to_string()),
            |url, _, _| if url.contains("bad") { None } else { Some(100) },
        );
        let mut bottles = IndexMap::new();
        bottles.insert("ok".to_string(), blob_url("wget", "ok"));
        bottles.insert("bad".to_string(), blob_url("jq", "bad"));
        let mut cache = IndexMap::new();

        let result = resolve_bottle_sizes_with(&bottles, &mut cache, &fetcher, 4, DEFAULT_TIMEOUT);
        assert_eq!(
            result,
            HashMap::from([("ok".to_string(), 100), ("bad".to_string(), 0)])
        );
        assert_eq!(cache, IndexMap::from([("ok".to_string(), 100)]));
    }

    #[test]
    fn test_resolve_bottle_sizes_warns_about_failures() {
        let _ = test_logger();
        let fetcher = MockBottleFetcher::new(|_, _| None, |_, _, _| Some(100));
        let mut bottles = IndexMap::new();
        bottles.insert("d1".to_string(), blob_url("wget", "d1"));
        let mut cache = IndexMap::new();

        resolve_bottle_sizes_with(&bottles, &mut cache, &fetcher, 4, DEFAULT_TIMEOUT);
        assert!(logs_contain("Couldn't fetch 1 of 1 bottle sizes"));
    }

    #[test]
    fn test_resolve_bottle_sizes_token_failure_not_cached() {
        let fetcher = MockBottleFetcher::new(|_, _| None, |_, _, _| Some(100));
        let mut bottles = IndexMap::new();
        bottles.insert("d1".to_string(), blob_url("wget", "d1"));
        let mut cache = IndexMap::new();

        let result = resolve_bottle_sizes_with(&bottles, &mut cache, &fetcher, 4, DEFAULT_TIMEOUT);
        assert_eq!(result, HashMap::from([("d1".to_string(), 0)]));
        assert!(cache.is_empty());
        assert_eq!(fetcher.blob_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_resolve_bottle_sizes_batches_token_scopes() {
        let fetcher = MockBottleFetcher::new(|_, _| Some("t".to_string()), |_, _, _| Some(1));
        let count = TOKEN_SCOPE_BATCH_SIZE + 1;
        let mut bottles = IndexMap::new();
        for i in 0..count {
            bottles.insert(
                format!("d{i}"),
                blob_url(&format!("pkg{i}"), &format!("d{i}")),
            );
        }
        let mut cache = IndexMap::new();

        resolve_bottle_sizes_with(&bottles, &mut cache, &fetcher, 4, DEFAULT_TIMEOUT);
        assert_eq!(fetcher.token_calls.load(Ordering::SeqCst), 2);
        let mut scoped: Vec<usize> = fetcher
            .token_batches
            .lock()
            .expect("lock batches")
            .iter()
            .map(Vec::len)
            .collect();
        scoped.sort_unstable();
        assert_eq!(scoped, vec![1, TOKEN_SCOPE_BATCH_SIZE]);
    }

    #[test]
    fn test_resolve_bottle_sizes_unparseable_url() {
        let fetcher = MockBottleFetcher::new(|_, _| Some("t".to_string()), |_, _, _| Some(1));
        let mut bottles = IndexMap::new();
        bottles.insert("d1".to_string(), "https://example.com/x".to_string());
        let mut cache = IndexMap::new();

        let result = resolve_bottle_sizes_with(&bottles, &mut cache, &fetcher, 4, DEFAULT_TIMEOUT);
        assert_eq!(result, HashMap::from([("d1".to_string(), 0)]));
        assert_eq!(fetcher.token_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_bottle_size_cache_roundtrip() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("cache").join("bottle_sizes.json");
        let cache = IndexMap::from([("a".to_string(), 1), ("b".to_string(), 2)]);

        save_bottle_size_cache(&path, &cache, BOTTLE_SIZE_CACHE_MAX_ENTRIES);
        assert_eq!(load_bottle_size_cache(&path), cache);

        let entries: Vec<String> = fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().into_string().expect("utf8"))
            .collect();
        assert_eq!(entries, vec!["bottle_sizes.json"]);
    }

    #[test]
    fn test_load_bottle_size_cache_drops_poisoned_entries() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("cache").join("bottle_sizes.json");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &path,
            serde_json::to_string(&json!({"good": 10, "zero": 0, "junk": "x", "flag": true}))
                .expect("json"),
        )
        .expect("write");

        assert_eq!(
            load_bottle_size_cache(&path),
            IndexMap::from([("good".to_string(), 10)])
        );
    }

    #[test]
    fn test_load_bottle_size_cache_invalid() {
        for content in ["{not json", "[1, 2]"] {
            let tmp = TempDir::new().expect("tempdir");
            let path = tmp.path().join("bottle_sizes.json");
            fs::write(&path, content).expect("write");
            assert!(load_bottle_size_cache(&path).is_empty());
        }
    }

    #[test]
    fn test_load_bottle_size_cache_missing() {
        let tmp = TempDir::new().expect("tempdir");
        assert!(load_bottle_size_cache(&tmp.path().join("missing.json")).is_empty());
    }

    #[test]
    fn test_save_bottle_size_cache_evicts_oldest() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("bottle_sizes.json");
        let cache = IndexMap::from([
            ("old".to_string(), 1),
            ("mid".to_string(), 2),
            ("new".to_string(), 3),
        ]);

        save_bottle_size_cache(&path, &cache, 2);
        assert_eq!(
            load_bottle_size_cache(&path),
            IndexMap::from([("mid".to_string(), 2), ("new".to_string(), 3)])
        );
    }

    #[test]
    fn test_save_bottle_size_cache_unwritable() {
        let _ = test_logger();
        let tmp = TempDir::new().expect("tempdir");
        let blocker = tmp.path().join("not_a_dir");
        fs::write(&blocker, b"file").expect("write blocker");
        let bad_path = blocker.join("bottle_sizes.json");
        let cache = IndexMap::from([("a".to_string(), 1)]);

        save_bottle_size_cache(&bad_path, &cache, BOTTLE_SIZE_CACHE_MAX_ENTRIES);
        assert!(logs_contain("Could not write bottle size cache"));
    }

    #[test]
    fn test_macos_bottle_version() {
        let cases = [
            ("arm64_sonoma", Some(("arm64", 14))),
            ("arm64_big_sur", Some(("arm64", 11))),
            ("tahoe", Some(("x86_64", 26))),
            ("x86_64_linux", None),
            ("all", None),
            ("catalina", None),
        ];
        for (tag, expected) in cases {
            assert_eq!(macos_bottle_version(tag), expected, "tag={tag}");
        }
    }

    #[test]
    fn test_detect_bottle_tag_for() {
        let cases = [
            ("darwin", "arm64", Some("26.6.2"), "arm64_tahoe"),
            ("macos", "x86_64", Some("15.1"), "sequoia"),
            ("darwin", "arm64", Some("27.0"), "arm64_golden_gate"),
            ("darwin", "arm64", Some("28.0"), "arm64_golden_gate"),
            ("darwin", "arm64", Some("10.15"), "arm64_big_sur"),
            ("darwin", "arm64", None, "arm64_golden_gate"),
            ("darwin", "arm64", Some(""), "arm64_golden_gate"),
            ("linux", "x86_64", Some(""), "x86_64_linux"),
            ("linux", "aarch64", Some(""), "arm64_linux"),
        ];
        for (os, arch, release, expected) in cases {
            assert_eq!(
                detect_bottle_tag_for(os, arch, release),
                expected,
                "os={os} arch={arch} release={release:?}"
            );
        }
    }

    #[test]
    fn test_extract_plist_product_version() {
        let plist = r"
        <dict>
            <key>ProductName</key>
            <string>macOS</string>
            <key>ProductVersion</key>
            <string>15.3.1</string>
        </dict>
        ";
        assert_eq!(
            extract_plist_product_version(plist).as_deref(),
            Some("15.3.1")
        );
    }

    #[test]
    fn test_get_theoretical_catalog_uses_brew_cache() {
        let tmp = TempDir::new().expect("tempdir");
        let api_dir = tmp.path().join("api");
        fs::create_dir(&api_dir).expect("mkdir");
        fs::write(api_dir.join("formula.json"), r#"[{"name": "wget"}]"#).expect("write");

        let runner = MockBrewRunner::ok(format!("{}\n", tmp.path().display()));
        let catalog = get_theoretical_catalog_with(&runner, None, None, Some("arm64_tahoe"))
            .expect("catalog");
        assert_eq!(catalog.formulae.len(), 1);
        assert_eq!(catalog.formulae[0].name, "wget");
        assert_eq!(catalog.casks, Vec::new());
    }

    #[test]
    fn test_get_theoretical_catalog_missing() {
        let tmp = TempDir::new().expect("tempdir");
        let runner = MockBrewRunner::ok("");
        let err =
            get_theoretical_catalog_with(&runner, Some(tmp.path()), Some("arm64_tahoe"), None)
                .expect_err("should fail");
        assert!(err.to_string().contains(&tmp.path().display().to_string()));
    }

    #[test]
    fn test_runtime_dependencies_excludes_build_and_test() {
        let entries = vec![
            json!("a"),
            json!({"b": ":build"}),
            json!({"c": [":build", ":test"]}),
            json!({"d": ":recommended"}),
        ];
        assert_eq!(runtime_dependencies(&entries), vec!["a", "d"]);
    }

    #[test]
    fn test_load_internal_catalog_formulae() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp
            .path()
            .join("internal")
            .join("packages.arm64_tahoe.jws.json");
        write_jws_raw(&path, INTERNAL_PAYLOAD_JSON);

        let catalog = load_internal_catalog(&path, "arm64_tahoe").expect("catalog");
        let formulae: HashMap<&str, &Formula> = catalog
            .formulae
            .iter()
            .map(|f| (f.name.as_str(), f))
            .collect();

        assert_eq!(formulae["wget"].dependencies, vec!["openssl@4"]);
        let wget_files = &formulae["wget"]
            .bottle
            .as_ref()
            .expect("bottle")
            .stable
            .as_ref()
            .expect("stable")
            .files;
        assert_eq!(
            wget_files.get("arm64_tahoe"),
            Some(&BottleFile {
                sha256: "abc".to_string(),
                url: "https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:abc".to_string(),
            })
        );

        let python_files = &formulae["python@3.14"]
            .bottle
            .as_ref()
            .expect("bottle")
            .stable
            .as_ref()
            .expect("stable")
            .files;
        assert!(
            python_files["all"]
                .url
                .starts_with("https://ghcr.io/v2/homebrew/core/python/3.14/blobs/")
        );
        assert_eq!(formulae["afl++"].dependencies, Vec::<String>::new());
        assert!(formulae["afl++"].bottle.is_none());
    }

    #[test]
    fn test_load_internal_catalog_linux_includes_uses_from_macos() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("packages.x86_64_linux.jws.json");
        write_jws_raw(&path, INTERNAL_PAYLOAD_JSON);

        let catalog = load_internal_catalog(&path, "x86_64_linux").expect("catalog");
        let wget = catalog
            .formulae
            .iter()
            .find(|f| f.name == "wget")
            .expect("wget");
        assert_eq!(wget.dependencies, vec!["openssl@4", "zlib"]);
    }

    #[test]
    fn test_load_internal_catalog_casks() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("packages.arm64_tahoe.jws.json");
        write_jws_raw(&path, INTERNAL_PAYLOAD_JSON);

        let catalog = load_internal_catalog(&path, "arm64_tahoe").expect("catalog");
        assert_eq!(
            catalog.casks,
            vec![
                Cask {
                    token: "brave-browser".to_string(),
                    name: vec!["Brave".to_string()],
                    depends_on: CaskDependsOn::default(),
                    artifacts: Vec::new(),
                },
                Cask {
                    token: "azure-cli".to_string(),
                    name: vec!["Azure CLI".to_string()],
                    depends_on: CaskDependsOn {
                        formula: vec!["python@3.14".to_string()],
                        cask: Vec::new(),
                    },
                    artifacts: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn test_load_internal_catalog_malformed() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("packages.arm64_tahoe.jws.json");
        write_jws(&path, &json!(["not", "a", "dict"]));

        let err = load_internal_catalog(&path, "arm64_tahoe").expect_err("should fail");
        assert!(err.to_string().contains("Failed to parse"));
    }

    #[test]
    fn test_get_theoretical_catalog_prefers_internal() {
        let tmp = TempDir::new().expect("tempdir");
        write_jws_raw(
            &tmp.path()
                .join("internal")
                .join("packages.arm64_tahoe.jws.json"),
            INTERNAL_PAYLOAD_JSON,
        );
        write_jws(
            &tmp.path().join("formula.jws.json"),
            &json!([{"name": "legacy"}]),
        );

        let runner = MockBrewRunner::ok("");
        let catalog =
            get_theoretical_catalog_with(&runner, Some(tmp.path()), Some("arm64_tahoe"), None)
                .expect("catalog");
        let names: HashSet<&str> = catalog.formulae.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, HashSet::from(["wget", "python@3.14", "afl++"]));
    }

    #[test]
    fn test_get_theoretical_catalog_falls_back_to_legacy() {
        let tmp = TempDir::new().expect("tempdir");
        write_jws_raw(
            &tmp.path()
                .join("internal")
                .join("packages.sequoia.jws.json"),
            INTERNAL_PAYLOAD_JSON,
        );
        write_jws(
            &tmp.path().join("formula.jws.json"),
            &json!([{"name": "legacy"}]),
        );

        let runner = MockBrewRunner::ok("");
        let catalog =
            get_theoretical_catalog_with(&runner, Some(tmp.path()), Some("arm64_tahoe"), None)
                .expect("catalog");
        assert_eq!(catalog.formulae.len(), 1);
        assert_eq!(catalog.formulae[0].name, "legacy");
        assert_eq!(catalog.casks, Vec::new());
    }

    #[test]
    fn test_get_theoretical_catalog_wrong_tag() {
        let tmp = TempDir::new().expect("tempdir");
        write_jws_raw(
            &tmp.path()
                .join("internal")
                .join("packages.arm64_tahoe.jws.json"),
            INTERNAL_PAYLOAD_JSON,
        );

        let runner = MockBrewRunner::ok("");
        let err =
            get_theoretical_catalog_with(&runner, Some(tmp.path()), Some("x86_64_linux"), None)
                .expect_err("should fail");
        let msg = err.to_string();
        assert!(msg.contains("x86_64_linux"));
        assert!(msg.contains("available: arm64_tahoe"));
    }

    #[test]
    fn test_get_theoretical_catalog_detects_tag() {
        let tmp = TempDir::new().expect("tempdir");
        write_jws_raw(
            &tmp.path()
                .join("internal")
                .join("packages.arm64_tahoe.jws.json"),
            INTERNAL_PAYLOAD_JSON,
        );

        let runner = MockBrewRunner::ok("");
        let catalog =
            get_theoretical_catalog_with(&runner, Some(tmp.path()), None, Some("arm64_tahoe"))
                .expect("catalog");
        assert_eq!(catalog.casks.len(), 2);
    }

    #[test]
    fn test_read_jws_payload_malformed() {
        for content in ["{not json", r#"{"payload": 5}"#] {
            let tmp = TempDir::new().expect("tempdir");
            let path = tmp.path().join("x.jws.json");
            fs::write(&path, content).expect("write");
            assert!(read_jws_payload(&path).is_none());
        }
    }

    #[test]
    fn test_load_api_cache_plain_json() {
        let tmp = TempDir::new().expect("tempdir");
        let json_file = tmp.path().join("formula.json");
        let missing = tmp.path().join("missing");

        fs::write(&json_file, r#"[{"name": "wget"}]"#).expect("write");
        let data: Vec<Formula> = load_api_cache(&json_file, &missing);
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].name, "wget");

        fs::write(&json_file, r#"{"name": "wget"}"#).expect("write");
        let invalid: Vec<Formula> = load_api_cache(&json_file, &missing);
        assert_eq!(invalid, Vec::new());
    }

    #[cfg(unix)]
    #[test]
    fn test_get_cask_size_uses_resolved_targets() {
        let tmp = TempDir::new().expect("tempdir");
        let prefix = tmp.path().join("brew");
        let caskroom = prefix.join("Caskroom").join("brave-browser").join("1.0");
        fs::create_dir_all(&caskroom).expect("mkdir caskroom");
        fs::write(caskroom.join("meta.json"), vec![b'x'; 3]).expect("write meta");

        let app = tmp.path().join("Applications").join("Brave Browser.app");
        fs::create_dir_all(&app).expect("mkdir app");
        fs::write(app.join("binary"), vec![b'x'; 100]).expect("write binary");

        let link = prefix.join("bin").join("brave");
        fs::create_dir_all(link.parent().expect("bin dir")).expect("mkdir bin");
        symlink_path(&app.join("binary"), &link);

        let cask = Cask {
            token: "brave-browser".to_string(),
            artifacts: vec![
                json!({"app": ["Brave Browser.app"], "target": app.to_string_lossy()}),
                json!({"binary": [app.join("binary").to_string_lossy()], "target": link.to_string_lossy()}),
                json!({"uninstall": [{"quit": "com.brave.Browser"}]}),
                json!("unexpected"),
            ],
            ..Cask::default()
        };
        assert_eq!(get_cask_size(&cask, &prefix), 103);
    }

    #[test]
    fn test_cask_artifact_paths_infers_legacy_app_locations() {
        let cask = Cask {
            token: "demo".to_string(),
            artifacts: vec![
                json!({"app": ["qbittorrent.app", {"target": "qBittorrent.app"}]}),
                json!({"suite": ["Adobe Suite"]}),
                json!({"app": ["Absolute.app", {"target": "/Opt/Absolute.app"}]}),
                json!({"pkg": ["Installer.pkg"]}),
            ],
            ..Cask::default()
        };
        assert_eq!(
            cask_artifact_paths(&cask),
            vec![
                PathBuf::from("/Applications/qBittorrent.app"),
                PathBuf::from("/Applications/Adobe Suite"),
                PathBuf::from("/Opt/Absolute.app"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_get_keg_sizes_follows_opt_link() {
        let tmp = TempDir::new().expect("tempdir");
        make_rack(
            tmp.path(),
            "foo",
            &[("1.0", 10), ("2.0", 20), ("2.0_1", 30)],
        );
        fs::create_dir(tmp.path().join("opt")).expect("mkdir opt");
        symlink_path(
            Path::new("../Cellar/foo/2.0_1"),
            &tmp.path().join("opt").join("foo"),
        );

        let formula = Formula {
            name: "foo".to_string(),
            versions: Some(Versions {
                stable: Some("2.0".to_string()),
            }),
            revision: 1,
            ..Formula::default()
        };
        assert_eq!(
            get_keg_sizes(&formula, tmp.path()),
            KegSizes {
                current: 30,
                stale: BTreeMap::from([("1.0".to_string(), 10), ("2.0".to_string(), 20)]),
            }
        );
    }

    #[test]
    fn test_get_keg_sizes_without_opt_link() {
        let cases = [
            Formula {
                name: "foo".to_string(),
                linked_keg: Some("2.0".to_string()),
                versions: Some(Versions {
                    stable: Some("2.0".to_string()),
                }),
                ..Formula::default()
            },
            Formula {
                name: "foo".to_string(),
                installed: vec![
                    InstalledKeg {
                        version: Some("1.0".to_string()),
                        ..InstalledKeg::default()
                    },
                    InstalledKeg {
                        version: Some("2.0".to_string()),
                        ..InstalledKeg::default()
                    },
                ],
                versions: Some(Versions {
                    stable: Some("2.0".to_string()),
                }),
                ..Formula::default()
            },
        ];

        for formula in cases {
            let tmp = TempDir::new().expect("tempdir");
            make_rack(tmp.path(), "foo", &[("1.0", 10), ("2.0", 20)]);
            assert_eq!(
                get_keg_sizes(&formula, tmp.path()),
                KegSizes {
                    current: 20,
                    stale: BTreeMap::from([("1.0".to_string(), 10)]),
                }
            );
        }
    }

    #[test]
    fn test_get_keg_sizes_unknown_current_counts_everything() {
        let cases = [
            Formula {
                name: "foo".to_string(),
                ..Formula::default()
            },
            Formula {
                name: "foo".to_string(),
                linked_keg: Some("9.9".to_string()),
                ..Formula::default()
            },
        ];

        for formula in cases {
            let tmp = TempDir::new().expect("tempdir");
            make_rack(tmp.path(), "foo", &[("1.0", 10), ("2.0", 20)]);
            assert_eq!(
                get_keg_sizes(&formula, tmp.path()),
                KegSizes {
                    current: 30,
                    stale: BTreeMap::new(),
                }
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_get_keg_sizes_ignores_foreign_opt_link() {
        let tmp = TempDir::new().expect("tempdir");
        make_rack(tmp.path(), "foo", &[("1.0", 10)]);
        fs::create_dir(tmp.path().join("opt")).expect("mkdir opt");
        symlink_path(tmp.path(), &tmp.path().join("opt").join("foo"));

        let formula = Formula {
            name: "foo".to_string(),
            linked_keg: Some("1.0".to_string()),
            ..Formula::default()
        };
        assert_eq!(
            get_keg_sizes(&formula, tmp.path()),
            KegSizes {
                current: 10,
                stale: BTreeMap::new(),
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_get_keg_sizes_keeps_what_cleanup_keeps() {
        let cases = [
            Formula {
                name: "foo".to_string(),
                versions: Some(Versions {
                    stable: Some("3.0".to_string()),
                }),
                ..Formula::default()
            },
            Formula {
                name: "foo".to_string(),
                ..Formula::default()
            },
            Formula {
                name: "foo".to_string(),
                versions: Some(Versions {
                    stable: Some("2.0".to_string()),
                }),
                linked_keg: Some("1.0".to_string()),
                ..Formula::default()
            },
        ];

        for formula in cases {
            let tmp = TempDir::new().expect("tempdir");
            old_and_new_kegs(tmp.path(), "foo");
            assert_eq!(
                get_keg_sizes(&formula, tmp.path()),
                KegSizes {
                    current: 20,
                    stale: BTreeMap::new(),
                }
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_get_keg_sizes_keeps_pinned_keg() {
        let tmp = TempDir::new().expect("tempdir");
        old_and_new_kegs(tmp.path(), "foo");
        let pins = tmp.path().join("var").join("homebrew").join("pinned");
        fs::create_dir_all(&pins).expect("mkdir pins");
        symlink_path(
            &tmp.path().join("Cellar").join("foo").join("1.0"),
            &pins.join("foo"),
        );

        let formula = Formula {
            name: "foo".to_string(),
            versions: Some(Versions {
                stable: Some("2.0".to_string()),
            }),
            pinned: true,
            ..Formula::default()
        };
        assert_eq!(
            get_keg_sizes(&formula, tmp.path()),
            KegSizes {
                current: 20,
                stale: BTreeMap::new(),
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_get_keg_sizes_honors_keepme() {
        for (ref_exists, expected_base) in [(true, None), (false, Some(10_u64))] {
            let tmp = TempDir::new().expect("tempdir");
            old_and_new_kegs(tmp.path(), "foo");
            let reference = tmp.path().join("user-of-foo");
            if ref_exists {
                fs::write(&reference, b"").expect("touch ref");
            }
            let keepme_content = format!("{}\n", reference.display());
            fs::write(
                tmp.path()
                    .join("Cellar")
                    .join("foo")
                    .join("1.0")
                    .join(".keepme"),
                &keepme_content,
            )
            .expect("write keepme");

            let formula = Formula {
                name: "foo".to_string(),
                versions: Some(Versions {
                    stable: Some("2.0".to_string()),
                }),
                ..Formula::default()
            };
            let expected_stale = match expected_base {
                Some(base) => {
                    BTreeMap::from([("1.0".to_string(), base + keepme_content.len() as u64)])
                }
                None => BTreeMap::new(),
            };
            assert_eq!(
                get_keg_sizes(&formula, tmp.path()),
                KegSizes {
                    current: 20,
                    stale: expected_stale,
                }
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_get_keg_sizes_skips_head_installs() {
        let tmp = TempDir::new().expect("tempdir");
        make_rack(tmp.path(), "foo", &[("2.0", 20), ("HEAD-abc1234", 5)]);
        fs::create_dir(tmp.path().join("opt")).expect("mkdir opt");
        symlink_path(
            Path::new("../Cellar/foo/HEAD-abc1234"),
            &tmp.path().join("opt").join("foo"),
        );

        let formula = Formula {
            name: "foo".to_string(),
            versions: Some(Versions {
                stable: Some("2.0".to_string()),
            }),
            ..Formula::default()
        };
        assert_eq!(
            get_keg_sizes(&formula, tmp.path()),
            KegSizes {
                current: 5,
                stale: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn test_get_keg_sizes_missing_rack() {
        let tmp = TempDir::new().expect("tempdir");
        let formula = Formula {
            name: "gone".to_string(),
            linked_keg: Some("1.0".to_string()),
            ..Formula::default()
        };
        assert_eq!(
            get_keg_sizes(&formula, tmp.path()),
            KegSizes {
                current: 0,
                stale: BTreeMap::new(),
            }
        );
    }
}
