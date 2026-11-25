use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::sync::Mutex;

use assert_cmd::Command;
use clap_complete::Shell;
use indexmap::IndexMap;
use predicates::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;

use dark_matter::VERSION;
use dark_matter::cli::{
    CliContext, complete_installed_with, install_completion_in, read_snapshot, run_args,
};
use dark_matter::error::{DarkMatterError, Result};
use dark_matter::homebrew::{BrewRunner, PhysicalSizer, TheoreticalSizer};
use dark_matter::model::{
    Bottle, BottleFile, BottleStable, BrewMetadata, Cask, Formula, InstalledKeg, KegSizes,
};

struct MockBrewRunner {
    prefix: String,
    cache: String,
    metadata_json: String,
    fail_missing_brew: bool,
}

impl MockBrewRunner {
    fn new(metadata: &BrewMetadata) -> Self {
        Self {
            prefix: "/opt/homebrew".to_string(),
            cache: "/tmp/brew-cache".to_string(),
            metadata_json: serde_json::to_string(metadata).expect("serialize metadata"),
            fail_missing_brew: false,
        }
    }

    fn missing_brew() -> Self {
        Self {
            prefix: String::new(),
            cache: String::new(),
            metadata_json: String::new(),
            fail_missing_brew: true,
        }
    }
}

impl BrewRunner for MockBrewRunner {
    fn run_brew(&self, args: &[&str], _failure: &str) -> Result<String> {
        if self.fail_missing_brew {
            return Err(DarkMatterError::Pipeline(
                "Homebrew (`brew`) was not found on PATH. Is Homebrew installed?".to_string(),
            ));
        }
        match args {
            ["--prefix"] => Ok(format!("{}\n", self.prefix)),
            ["--cache"] => Ok(format!("{}\n", self.cache)),
            ["info", "--json=v2", "--installed"] => Ok(self.metadata_json.clone()),
            other => Err(DarkMatterError::Pipeline(format!(
                "Unexpected brew args: {other:?}"
            ))),
        }
    }
}

struct MockPhysicalSizer {
    formula_sizes: HashMap<String, KegSizes>,
    cask_sizes: HashMap<String, u64>,
}

impl MockPhysicalSizer {
    fn from_pairs(pairs: &[(&str, u64)]) -> Self {
        let formula_sizes = pairs
            .iter()
            .map(|&(name, bytes)| {
                (
                    name.to_string(),
                    KegSizes {
                        current: bytes,
                        stale: BTreeMap::new(),
                    },
                )
            })
            .collect();
        Self {
            formula_sizes,
            cask_sizes: HashMap::new(),
        }
    }

    fn with_stale(mut self, name: &str, version: &str, bytes: u64) -> Self {
        self.formula_sizes
            .entry(name.to_string())
            .or_default()
            .stale
            .insert(version.to_string(), bytes);
        self
    }
}

impl PhysicalSizer for MockPhysicalSizer {
    fn keg_sizes(&self, formula: &Formula, _prefix: &Path) -> KegSizes {
        self.formula_sizes
            .get(&formula.name)
            .cloned()
            .unwrap_or_default()
    }

    fn cask_size(&self, cask: &Cask, _prefix: &Path) -> u64 {
        self.cask_sizes.get(&cask.token).copied().unwrap_or(0)
    }
}

struct MockTheoreticalSizer {
    tag: String,
    sizes: HashMap<String, u64>,
    seen_bottles: Mutex<Vec<String>>,
}

impl MockTheoreticalSizer {
    fn new(tag: &str, sizes: &[(&str, u64)]) -> Self {
        Self {
            tag: tag.to_string(),
            sizes: sizes
                .iter()
                .map(|&(sha, sz)| (sha.to_string(), sz))
                .collect(),
            seen_bottles: Mutex::new(Vec::new()),
        }
    }
}

impl TheoreticalSizer for MockTheoreticalSizer {
    fn detect_bottle_tag(&self) -> String {
        self.tag.clone()
    }

    fn resolve_bottle_sizes(&self, bottles: &IndexMap<String, String>) -> HashMap<String, u64> {
        let mut seen = self.seen_bottles.lock().expect("lock");
        for digest in bottles.keys() {
            seen.push(digest.clone());
        }
        bottles
            .keys()
            .map(|sha| (sha.clone(), self.sizes.get(sha).copied().unwrap_or(0)))
            .collect()
    }
}

fn make_formula(name: &str, deps: &[&str], requested: bool, bottles: &[(&str, &str)]) -> Formula {
    let bottle = if bottles.is_empty() {
        None
    } else {
        let mut files = IndexMap::new();
        for &(tag, sha) in bottles {
            files.insert(
                tag.to_string(),
                BottleFile {
                    sha256: sha.to_string(),
                    url: format!("https://ghcr.io/v2/homebrew/core/{name}/blobs/sha256:{sha}"),
                },
            );
        }
        Some(Bottle {
            stable: Some(BottleStable { files }),
        })
    };

    Formula {
        name: name.to_string(),
        full_name: Some(name.to_string()),
        dependencies: deps.iter().map(|s| (*s).to_string()).collect(),
        installed: vec![InstalledKeg {
            version: Some("1.0".to_string()),
            installed_on_request: requested,
            runtime_dependencies: Vec::new(),
        }],
        bottle,
        ..Default::default()
    }
}

/// Fixture matching Python's `mock_brew_metadata`:
/// - `pkg-a` (requested) -> `pkg-c`, `pkg-d`
/// - `pkg-b` (requested) -> `pkg-d`
/// - `pkg-c` (dep)       -> `pkg-e`
/// - `pkg-d` (dep)       -> []
/// - `pkg-e` (dep)       -> []
/// - `orphan-x` (not requested, no dependents) -> []
fn sample_installed_metadata() -> BrewMetadata {
    BrewMetadata {
        formulae: vec![
            make_formula(
                "pkg-a",
                &["pkg-c", "pkg-d"],
                true,
                &[("arm64_tahoe", "sha-a")],
            ),
            make_formula("pkg-b", &["pkg-d"], true, &[("arm64_tahoe", "sha-b")]),
            make_formula("pkg-c", &["pkg-e"], false, &[("arm64_tahoe", "sha-c")]),
            make_formula("pkg-d", &[], false, &[("arm64_tahoe", "sha-d")]),
            make_formula("pkg-e", &[], false, &[("arm64_tahoe", "sha-e")]),
        ],
        casks: vec![],
    }
}

fn sample_physical_sizer() -> MockPhysicalSizer {
    MockPhysicalSizer::from_pairs(&[
        ("pkg-a", 1_000),
        ("pkg-b", 5_000),
        ("pkg-c", 4_000),
        ("pkg-d", 10_000),
        ("pkg-e", 2_000),
    ])
}

fn write_legacy_catalog(dir: &Path, metadata: &BrewMetadata) {
    fs::write(
        dir.join("formula.json"),
        serde_json::to_string(&metadata.formulae).expect("serialize formulae"),
    )
    .expect("write formula.json");
    fs::write(
        dir.join("cask.json"),
        serde_json::to_string(&metadata.casks).expect("serialize casks"),
    )
    .expect("write cask.json");
}

struct CliRunResult {
    code: i32,
    stdout: String,
    stderr: String,
}

static RUN_LOCK: Mutex<()> = Mutex::new(());

fn invoke_unlocked(
    args: &[&str],
    input: &str,
    runner: &dyn BrewRunner,
    physical: &dyn PhysicalSizer,
    theoretical: &dyn TheoreticalSizer,
    api_dir: Option<&Path>,
) -> CliRunResult {
    let mut full_args = vec!["dark-matter"];
    full_args.extend_from_slice(args);

    let mut stdin = Cursor::new(input.as_bytes().to_vec());
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let mut ctx = CliContext {
        runner,
        physical_sizer: physical,
        theoretical_sizer: theoretical,
        api_dir,
        stdin: &mut stdin,
        stdout: &mut stdout,
        stderr: &mut stderr,
        stdout_color: false,
        stderr_color: false,
        interactive_spinners: false,
    };

    let code = run_args(full_args, &mut ctx);
    CliRunResult {
        code,
        stdout: String::from_utf8(stdout).expect("utf8 stdout"),
        stderr: String::from_utf8(stderr).expect("utf8 stderr"),
    }
}

fn invoke(
    args: &[&str],
    input: &str,
    runner: &dyn BrewRunner,
    physical: &dyn PhysicalSizer,
    theoretical: &dyn TheoreticalSizer,
    api_dir: Option<&Path>,
) -> CliRunResult {
    let _guard = RUN_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    invoke_unlocked(args, input, runner, physical, theoretical, api_dir)
}

#[test]
fn test_version_flag() {
    let runner = MockBrewRunner::missing_brew();
    let physical = MockPhysicalSizer::from_pairs(&[]);
    let theoretical = MockTheoreticalSizer::new("arm64_tahoe", &[]);

    let res = invoke(&["--version"], "", &runner, &physical, &theoretical, None);
    assert_eq!(res.code, 0);
    assert!(
        res.stdout
            .contains(&format!("dark-matter version {VERSION}"))
    );
}

#[test]
fn test_help_offers_completion_install() {
    let runner = MockBrewRunner::missing_brew();
    let physical = MockPhysicalSizer::from_pairs(&[]);
    let theoretical = MockTheoreticalSizer::new("arm64_tahoe", &[]);

    let res = invoke(&["--help"], "", &runner, &physical, &theoretical, None);
    assert_eq!(res.code, 0);
    assert!(res.stdout.contains("--install-completion"));
    assert!(res.stdout.contains("--show-completion"));
}

#[test]
fn test_no_subcommand_prints_help_and_exits_2() {
    let runner = MockBrewRunner::missing_brew();
    let physical = MockPhysicalSizer::from_pairs(&[]);
    let theoretical = MockTheoreticalSizer::new("arm64_tahoe", &[]);

    let res = invoke(&[], "", &runner, &physical, &theoretical, None);
    assert_eq!(res.code, 2);
    assert!(res.stderr.contains("Usage:"));
}

#[test]
fn test_show_and_install_completion() {
    let runner = MockBrewRunner::missing_brew();
    let physical = MockPhysicalSizer::from_pairs(&[]);
    let theoretical = MockTheoreticalSizer::new("arm64_tahoe", &[]);

    let res = invoke(
        &["--show-completion", "zsh"],
        "",
        &runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res.code, 0);
    assert!(res.stdout.contains("COMPLETE=zsh dark-matter"));

    let tmp = TempDir::new().expect("tempdir");
    let mut out = Vec::new();
    let installed_path = install_completion_in(Shell::Zsh, tmp.path(), &mut out).expect("install");
    assert_eq!(installed_path, tmp.path().join(".zshrc"));
    let first_msg = String::from_utf8(out).expect("utf8");
    assert!(first_msg.contains("Installed zsh completion"));

    let contents = fs::read_to_string(&installed_path).expect("read .zshrc");
    assert!(contents.contains("source <(COMPLETE=zsh dark-matter)"));

    // Idempotency check
    let mut out2 = Vec::new();
    install_completion_in(Shell::Zsh, tmp.path(), &mut out2).expect("install again");
    let second_msg = String::from_utf8(out2).expect("utf8");
    assert!(second_msg.contains("already installed"));
    assert_eq!(
        fs::read_to_string(&installed_path).expect("read again"),
        contents
    );
}

#[test]
fn test_analyze_command_and_flags() {
    let meta = sample_installed_metadata();
    let runner = MockBrewRunner::new(&meta);
    let physical = sample_physical_sizer();
    let theoretical = MockTheoreticalSizer::new("arm64_tahoe", &[]);

    // Default roots-only, fractional
    let res = invoke(
        &["analyze", "--top", "5"],
        "",
        &runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res.code, 0);
    assert!(
        res.stdout
            .contains("Successfully parsed 5 formulae and 0 casks.")
    );
    assert!(res.stdout.contains("Weighted Rec. Size"));
    assert!(res.stdout.contains("pkg-a"));
    assert!(res.stdout.contains("pkg-b"));
    assert!(res.stdout.contains("3 dependencies not shown"));
    assert!(!res.stdout.contains("pkg-c · dependency"));

    // --all shows dependencies too
    let res_all = invoke(
        &["analyze", "--all"],
        "",
        &runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res_all.code, 0);
    assert!(res_all.stdout.contains("pkg-c · dependency"));
    assert!(!res_all.stdout.contains("not shown"));

    // --standard switches column to Standard Rec. Size
    let res_std = invoke(
        &["analyze", "--standard", "--sort", "core"],
        "",
        &runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res_std.code, 0);
    assert!(res_std.stdout.contains("Standard Rec. Size"));
    // Sorted by core: pkg-b (5000) before pkg-a (1000)
    let pos_b = res_std.stdout.find("pkg-b").expect("pkg-b");
    let pos_a = res_std.stdout.find("pkg-a").expect("pkg-a");
    assert!(pos_b < pos_a);

    // --fractional conflicts with --standard
    let conflict = invoke(
        &["analyze", "--fractional", "--standard"],
        "",
        &runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(conflict.code, 2);
}

#[test]
fn test_leaderboard_command_and_all_flag() {
    let meta = sample_installed_metadata();
    let runner = MockBrewRunner::new(&meta);
    let physical = MockPhysicalSizer::from_pairs(&[]);
    let theoretical = MockTheoreticalSizer::new(
        "arm64_tahoe",
        &[
            ("sha-a", 1_000),
            ("sha-b", 2_000),
            ("sha-c", 4_000),
            ("sha-d", 10_000),
            ("sha-e", 2_000),
        ],
    );
    let tmp = TempDir::new().expect("tempdir");
    write_legacy_catalog(tmp.path(), &meta);

    let res = invoke(
        &["leaderboard", "--arch", "arm64_tahoe"],
        "",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res.code, 0);
    assert!(res.stdout.contains("Theoretical Rec. Size"));
    assert!(res.stdout.contains("3 dependencies not shown"));

    let res_all = invoke(
        &["leaderboard", "--arch", "arm64_tahoe", "--all"],
        "",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_all.code, 0);
    assert!(res_all.stdout.contains("pkg-d · dependency"));
    assert!(!res_all.stdout.contains("not shown"));
}

#[test]
fn test_inspect_installed_catalog_and_fallback() {
    let installed_meta = sample_installed_metadata();
    let mut catalog_meta = sample_installed_metadata();
    catalog_meta.formulae.push(make_formula(
        "not-installed",
        &["pkg-d"],
        false,
        &[("arm64_tahoe", "sha-ni"), ("x86_64_linux", "sha-ni-linux")],
    ));
    catalog_meta.casks.push(Cask {
        token: "some-cask".to_string(),
        ..Default::default()
    });

    let runner = MockBrewRunner::new(&installed_meta);
    let physical = sample_physical_sizer();
    let theoretical = MockTheoreticalSizer::new(
        "arm64_tahoe",
        &[
            ("sha-a", 1_000),
            ("sha-d", 10_000),
            ("sha-ni", 2_000),
            ("sha-ni-linux", 3_000),
        ],
    );
    let tmp = TempDir::new().expect("tempdir");
    write_legacy_catalog(tmp.path(), &catalog_meta);

    // Installed target
    let res = invoke(
        &["inspect", "pkg-a", "--source", "installed"],
        "",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res.code, 0);
    assert!(res.stdout.contains("Core Size"));
    assert!(res.stdout.contains("pkg-a"));

    // Catalog target with --arch passes arch tag
    let res_cat = invoke(
        &[
            "inspect",
            "not-installed",
            "--source",
            "catalog",
            "--arch",
            "x86_64_linux",
        ],
        "",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_cat.code, 0);
    assert!(res_cat.stdout.contains("Archive Size"));
    assert!(res_cat.stdout.contains("3.0 KB"));

    // Uninstalled target -> fallback accepted
    let res_fb = invoke(
        &["inspect", "not-installed"],
        "y\n",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_fb.code, 0);
    assert!(
        res_fb
            .stdout
            .contains("'not-installed' is not installed. Fall back to the theoretical catalog?")
    );
    assert!(res_fb.stdout.contains("Theoretical Rec. Size"));

    // Uninstalled target -> fallback declined -> exit 1 without duplicate Pipeline Error
    let res_dec = invoke(
        &["inspect", "not-installed"],
        "n\n",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_dec.code, 1);
    assert!(!res_dec.stdout.contains("Pipeline Error:"));

    // Cask in catalog -> Pipeline Error (NOT caught by fallback)
    let res_cask = invoke(
        &["inspect", "some-cask", "--source", "catalog"],
        "",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_cask.code, 1);
    assert!(res_cask.stdout.contains("Pipeline Error:"));
    assert!(res_cask.stdout.contains("is a cask"));
}

#[test]
fn test_compare_dedup_short_sort_and_fallback() {
    let installed_meta = sample_installed_metadata();
    let mut catalog_meta = sample_installed_metadata();
    catalog_meta.formulae.push(make_formula(
        "fake",
        &[],
        false,
        &[("arm64_tahoe", "sha-fake")],
    ));

    let runner = MockBrewRunner::new(&installed_meta);
    let physical = sample_physical_sizer();
    let theoretical = MockTheoreticalSizer::new(
        "arm64_tahoe",
        &[("sha-a", 1_000), ("sha-b", 5_000), ("sha-fake", 2_000)],
    );
    let tmp = TempDir::new().expect("tempdir");
    write_legacy_catalog(tmp.path(), &catalog_meta);

    // Deduplication preserving order + -s maps to --sort (not --source)
    let res = invoke(
        &["compare", "pkg-a", "pkg-a", "pkg-b", "-s", "core"],
        "",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res.code, 0);
    assert!(res.stdout.contains("Comparing bloat for: pkg-a, pkg-b"));
    let pos_b = res
        .stdout
        .find("pkg-b\n")
        .or_else(|| res.stdout.find("pkg-b "));
    let pos_a = res
        .stdout
        .find("pkg-a\n")
        .or_else(|| res.stdout.find("pkg-a "));
    assert!(pos_b.is_some() && pos_a.is_some());
    // In table body, pkg-b (5 KB) appears before pkg-a (1 KB)
    let table_start = res.stdout.find("Dark Matter:").expect("title");
    let body = &res.stdout[table_start..];
    assert!(body.find("pkg-b").unwrap() < body.find("pkg-a").unwrap());

    // All missing declined -> exit 1, catalog not loaded
    let res_none = invoke(
        &["compare", "fake-1", "fake-2"],
        "n\n",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_none.code, 1);
    assert!(
        res_none
            .stdout
            .contains("'fake-1', 'fake-2' are not installed")
    );
    assert!(!res_none.stdout.contains("Pipeline Error:"));

    // Partial missing declined -> compares installed subset
    let res_sub = invoke(
        &["compare", "pkg-a", "fake"],
        "n\n",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_sub.code, 0);
    assert!(res_sub.stdout.contains("Core Size"));
    assert!(res_sub.stdout.contains("pkg-a"));

    // Partial missing accepted (empty line defaults to yes) -> switches to catalog
    let res_yes = invoke(
        &["compare", "pkg-a", "fake"],
        "\n",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_yes.code, 0);
    assert!(res_yes.stdout.contains("Archive Size"));
    assert!(res_yes.stdout.contains("fake"));
}

#[test]
fn test_explain_table_tree_and_fallback() {
    let installed_meta = sample_installed_metadata();
    let mut catalog_meta = sample_installed_metadata();
    catalog_meta.formulae.push(make_formula(
        "pkg-not-here",
        &["pkg-d"],
        false,
        &[("arm64_tahoe", "sha-nh")],
    ));

    let runner = MockBrewRunner::new(&installed_meta);
    let physical = sample_physical_sizer();
    let theoretical =
        MockTheoreticalSizer::new("arm64_tahoe", &[("sha-nh", 1_000), ("sha-d", 10_000)]);
    let tmp = TempDir::new().expect("tempdir");
    write_legacy_catalog(tmp.path(), &catalog_meta);

    // Table view
    let res_tbl = invoke(
        &["explain", "pkg-a"],
        "",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_tbl.code, 0);
    assert!(res_tbl.stdout.contains("Dependency Breakdown: pkg-a"));
    assert!(res_tbl.stdout.contains("pkg-c"));
    assert!(res_tbl.stdout.contains("pkg-d"));
    assert!(res_tbl.stdout.contains("pkg-e"));

    // Tree view
    let res_tree = invoke(
        &["explain", "pkg-a", "--tree"],
        "",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_tree.code, 0);
    assert!(res_tree.stdout.contains("pkg-a ·"));
    assert!(res_tree.stdout.contains("├── ") || res_tree.stdout.contains("└── "));
    assert!(!res_tree.stdout.contains("Dependency Breakdown:"));

    // Fallback accepted
    let res_fb = invoke(
        &["explain", "pkg-not-here", "--source", "installed"],
        "y\n",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_fb.code, 0);
    assert!(
        res_fb
            .stdout
            .contains("Fall back to the theoretical catalog?")
    );
    assert!(res_fb.stdout.contains("Archive Size"));
    assert!(res_fb.stdout.contains("pkg-d"));

    // Fallback declined
    let res_dec = invoke(
        &["explain", "pkg-not-here"],
        "n\n",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_dec.code, 1);
}

#[test]
fn test_export_installed_json_and_catalog_csv_and_stderr_isolation() {
    let meta = sample_installed_metadata();
    let runner = MockBrewRunner::new(&meta);
    let physical = sample_physical_sizer();
    let theoretical = MockTheoreticalSizer::new(
        "arm64_tahoe",
        &[
            ("sha-a", 1_000),
            ("sha-b", 5_000),
            ("sha-c", 4_000),
            ("sha-d", 10_000),
            ("sha-e", 2_000),
        ],
    );
    let tmp = TempDir::new().expect("tempdir");
    write_legacy_catalog(tmp.path(), &meta);

    // Installed JSON
    let res_json = invoke(
        &["export", "--format", "json"],
        "",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_json.code, 0);
    assert_eq!(res_json.stderr, "");
    let parsed: Vec<Value> = serde_json::from_str(&res_json.stdout).expect("valid JSON");
    assert_eq!(parsed.len(), 5);
    assert_eq!(parsed[0]["Package"], "pkg-a");

    // Catalog CSV
    let res_csv = invoke(
        &["export", "--source", "catalog"],
        "",
        &runner,
        &physical,
        &theoretical,
        Some(tmp.path()),
    );
    assert_eq!(res_csv.code, 0);
    assert_eq!(res_csv.stderr, "");
    let lines: Vec<&str> = res_csv.stdout.lines().collect();
    assert_eq!(
        lines[0],
        "Package,Core_Bytes,Standard_Bytes,Weighted_Bytes,Bloat_Ratio,Dep_Count,Is_Root,Is_Requested"
    );
    assert_eq!(lines.len(), 6);

    // Export error goes to stderr, leaving stdout completely empty
    let missing_runner = MockBrewRunner::missing_brew();
    let res_err = invoke(
        &["export"],
        "",
        &missing_runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res_err.code, 1);
    assert_eq!(res_err.stdout, "");
    assert!(res_err.stderr.contains("Pipeline Error:"));
    assert!(res_err.stderr.contains("not found on PATH"));
}

#[test]
fn test_reclaim_orphans_and_cleanup() {
    let mut meta = sample_installed_metadata();
    meta.formulae
        .push(make_formula("orphan-z", &[], false, &[]));
    let runner = MockBrewRunner::new(&meta);
    let physical = sample_physical_sizer().with_stale("pkg-a", "0.9", 3_000);
    let theoretical = MockTheoreticalSizer::new("arm64_tahoe", &[]);

    // Reclaim deduplicates targets and reports blocked packages
    let res_reclaim = invoke(
        &["reclaim", "pkg-a", "pkg-d", "pkg-a"],
        "",
        &runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res_reclaim.code, 0);
    assert!(
        res_reclaim
            .stdout
            .contains("'pkg-d' can't be uninstalled: required by pkg-b.")
    );
    assert!(
        res_reclaim
            .stdout
            .contains("Reclaimable by removing pkg-a, pkg-d")
    );
    assert!(res_reclaim.stdout.contains("pkg-c"));
    assert!(res_reclaim.stdout.contains("pkg-e"));

    // Reclaim unknown package errors
    let res_unknown = invoke(
        &["reclaim", "nope"],
        "",
        &runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res_unknown.code, 1);
    assert!(res_unknown.stdout.contains("Pipeline Error:"));
    assert!(
        res_unknown
            .stdout
            .contains("not found in the local installation")
    );

    // Orphans
    let res_orphans = invoke(&["orphans"], "", &runner, &physical, &theoretical, None);
    assert_eq!(res_orphans.code, 0);
    assert!(res_orphans.stdout.contains("Orphaned Dependencies"));
    assert!(res_orphans.stdout.contains("orphan-z"));

    // Cleanup
    let res_cleanup = invoke(&["cleanup"], "", &runner, &physical, &theoretical, None);
    assert_eq!(res_cleanup.code, 0);
    assert!(res_cleanup.stdout.contains("Old Versions in the Cellar"));
    assert!(res_cleanup.stdout.contains("pkg-a"));
    assert!(res_cleanup.stdout.contains("0.9"));
}

#[test]
fn test_diff_command_and_snapshot_errors() {
    let runner = MockBrewRunner::missing_brew();
    let physical = MockPhysicalSizer::from_pairs(&[]);
    let theoretical = MockTheoreticalSizer::new("arm64_tahoe", &[]);

    let tmp = TempDir::new().expect("tempdir");
    let old = tmp.path().join("old.csv");
    fs::write(&old, "Package,Core_Bytes,Weighted_Bytes\na,10,20\n").expect("write old");
    let new = tmp.path().join("new.json");
    fs::write(
        &new,
        serde_json::to_string(&json!([{"Package": "a", "Core_Bytes": 30, "Weighted_Bytes": 40}]))
            .expect("json"),
    )
    .expect("write new");

    let res = invoke(
        &[
            "diff",
            old.to_str().unwrap(),
            new.to_str().unwrap(),
            "-n",
            "3",
        ],
        "",
        &runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res.code, 0);
    assert!(res.stdout.contains("10 B → 30 B (+20 B)"));
    assert!(res.stdout.contains("+20 B"));

    // Missing columns error (CSV)
    let missing_cols_csv = tmp.path().join("missing.csv");
    fs::write(&missing_cols_csv, "Package,Core_Bytes\na,10\n").expect("write missing csv");
    let err_csv = read_snapshot(&missing_cols_csv, "old").unwrap_err();
    assert!(err_csv.to_string().contains(
        "The old snapshot is missing Weighted_Bytes; create snapshots with `dark-matter export`."
    ));

    // Missing columns error (JSON)
    let missing_cols_json = tmp.path().join("missing.json");
    fs::write(&missing_cols_json, r#"[{"Package": "a"}]"#).expect("write missing json");
    let res_missing = invoke(
        &[
            "diff",
            old.to_str().unwrap(),
            missing_cols_json.to_str().unwrap(),
        ],
        "",
        &runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res_missing.code, 1);
    assert!(
        res_missing
            .stdout
            .contains("The new snapshot is missing Core_Bytes, Weighted_Bytes")
    );

    // Unreadable / malformed snapshot
    let bad = tmp.path().join("bad.json");
    fs::write(&bad, "not json").expect("write bad");
    let res_bad = invoke(
        &[
            "diff",
            bad.to_str().unwrap(),
            tmp.path().join("missing-file.csv").to_str().unwrap(),
        ],
        "",
        &runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res_bad.code, 1);
    assert!(res_bad.stdout.contains("Could not read snapshot"));
}

#[test]
fn test_validation_and_missing_brew_and_verbose_logging() {
    let meta = sample_installed_metadata();
    let ok_runner = MockBrewRunner::new(&meta);
    let missing_runner = MockBrewRunner::missing_brew();
    let physical = sample_physical_sizer();
    let theoretical = MockTheoreticalSizer::new("arm64_tahoe", &[]);

    // Invalid sort key -> exit 2
    let res_sort = invoke(
        &["analyze", "--sort", "ratoi"],
        "",
        &ok_runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res_sort.code, 2);

    // Non-positive top -> exit 2
    let res_top = invoke(
        &["analyze", "--top", "0"],
        "",
        &ok_runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res_top.code, 2);

    // Missing brew -> clean Pipeline Error, exit 1
    let res_missing = invoke(
        &["analyze"],
        "",
        &missing_runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res_missing.code, 1);
    assert!(res_missing.stdout.contains("Pipeline Error:"));
    assert!(res_missing.stdout.contains("not found on PATH"));

    // --verbose toggles global log max level (serialized against other CLI runs)
    let _guard = RUN_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let res_v = invoke_unlocked(
        &["--verbose", "analyze"],
        "",
        &ok_runner,
        &physical,
        &theoretical,
        None,
    );
    assert_eq!(res_v.code, 0);
    assert_eq!(log::max_level(), log::LevelFilter::Debug);

    let res_quiet = invoke_unlocked(&["analyze"], "", &ok_runner, &physical, &theoretical, None);
    assert_eq!(res_quiet.code, 0);
    assert_eq!(log::max_level(), log::LevelFilter::Warn);
}

#[test]
fn test_complete_installed_lists_formulae_and_casks_and_handles_missing_brew() {
    let tmp = TempDir::new().expect("tempdir");
    for rel in [
        "Cellar/wget",
        "Cellar/wavpack",
        "Cellar/jq",
        "Caskroom/warp",
    ] {
        fs::create_dir_all(tmp.path().join(rel)).expect("mkdir");
    }
    let missing_runner = MockBrewRunner::missing_brew();

    assert_eq!(
        complete_installed_with("wa", Some(tmp.path().to_path_buf()), &missing_runner),
        vec!["warp".to_string(), "wavpack".to_string()]
    );
    assert_eq!(
        complete_installed_with("", Some(tmp.path().to_path_buf()), &missing_runner),
        vec![
            "jq".to_string(),
            "warp".to_string(),
            "wavpack".to_string(),
            "wget".to_string()
        ]
    );
    assert_eq!(
        complete_installed_with("w", None, &missing_runner),
        Vec::<String>::new()
    );
}

#[test]
fn test_binary_integration_with_assert_cmd() {
    // --version
    Command::cargo_bin("dark-matter")
        .expect("binary")
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "dark-matter version {VERSION}"
        )));

    // --help offers completion flags
    Command::cargo_bin("dark-matter")
        .expect("binary")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--install-completion"))
        .stdout(predicate::str::contains("--show-completion"));

    // Invalid sort rejected with exit code 2
    Command::cargo_bin("dark-matter")
        .expect("binary")
        .args(["analyze", "--sort", "ratoi"])
        .assert()
        .code(2);

    // Non-positive --top rejected with exit code 2
    Command::cargo_bin("dark-matter")
        .expect("binary")
        .args(["analyze", "--top", "0"])
        .assert()
        .code(2);

    // Missing brew on empty PATH surfaces clean Pipeline Error
    Command::cargo_bin("dark-matter")
        .expect("binary")
        .env("PATH", "")
        .arg("analyze")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Pipeline Error:"))
        .stdout(predicate::str::contains("not found on PATH"));

    // Binary diff end-to-end
    let tmp = TempDir::new().expect("tempdir");
    let old = tmp.path().join("old.csv");
    let new = tmp.path().join("new.json");
    fs::write(&old, "Package,Core_Bytes,Weighted_Bytes\na,1000,2000\n").expect("write old");
    fs::write(
        &new,
        r#"[{"Package": "a", "Core_Bytes": 3000, "Weighted_Bytes": 4000}]"#,
    )
    .expect("write new");

    Command::cargo_bin("dark-matter")
        .expect("binary")
        .args(["diff", old.to_str().unwrap(), new.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("1.0 KB → 3.0 KB (+2.0 KB)"));
}
