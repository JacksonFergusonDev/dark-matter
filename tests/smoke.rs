use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use assert_cmd::cargo::cargo_bin_cmd;
use dark_matter::model::BloatRow;

/// Live Homebrew smoke test (replaces `scripts/smoke_check.py`).
///
/// Run with:
///   `cargo test --test smoke -- --ignored`
///
/// Optional environment variables:
/// - `SMOKE_EXPORT_JSON`: validate an existing `export --format json` file instead of invoking `dark-matter`.
/// - `SMOKE_PACKAGE`: verify this package is reported as requested (`Is_Requested == true`),
///   is a root (`Is_Root == true`), has positive core size, and has at least one dependency.
/// - `SMOKE_DEPENDENCY`: paired with `SMOKE_PACKAGE`, verify `Standard_Bytes >= pkg.Core_Bytes + dep.Core_Bytes`.
#[test]
#[ignore = "requires a real Homebrew installation"]
fn smoke_check_homebrew_export() {
    let raw_json = if let Ok(path) = env::var("SMOKE_EXPORT_JSON") {
        let path = PathBuf::from(path);
        fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
    } else {
        let output = cargo_bin_cmd!("dark-matter")
            .args(["export", "--format", "json"])
            .output()
            .expect("failed to execute dark-matter binary");
        assert!(
            output.status.success(),
            "dark-matter export failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("export output is not valid UTF-8")
    };

    let rows: Vec<BloatRow> =
        serde_json::from_str(&raw_json).expect("export JSON is malformed or missing columns");
    assert!(!rows.is_empty(), "smoke check failed: the export is empty");

    let by_name: HashMap<&str, &BloatRow> =
        rows.iter().map(|row| (row.package.as_str(), row)).collect();

    if let Ok(package) = env::var("SMOKE_PACKAGE") {
        let target = by_name
            .get(package.as_str())
            .unwrap_or_else(|| panic!("smoke check failed: {package} is not in the export"));
        assert!(
            target.is_requested,
            "smoke check failed: {package} is not marked as requested"
        );
        assert!(
            target.is_root,
            "smoke check failed: {package} is not a root"
        );
        assert!(
            target.core_bytes > 0,
            "smoke check failed: {package} measured 0 bytes"
        );
        assert!(
            target.dep_count >= 1,
            "smoke check failed: {package} has no dependencies"
        );

        if let Ok(dependency) = env::var("SMOKE_DEPENDENCY") {
            let dep = by_name
                .get(dependency.as_str())
                .unwrap_or_else(|| panic!("smoke check failed: {dependency} is not in the export"));
            assert!(
                target.standard_bytes >= target.core_bytes + dep.core_bytes,
                "smoke check failed: {package}'s recursive size doesn't include {dependency}"
            );
        }
    }

    let measured: u64 = rows.iter().map(|r| r.core_bytes).sum();
    let attributed: f64 = rows
        .iter()
        .filter(|r| r.is_root)
        .map(|r| r.weighted_bytes)
        .sum();
    let measured_f64 = measured as f64;
    let diff = (measured_f64 - attributed).abs();
    let tol = 1e-9 * measured_f64.max(attributed.abs()).max(1.0);
    assert!(
        diff <= tol,
        "smoke check failed: roots account for {attributed:.0} of {measured} bytes (diff {diff})"
    );
}
