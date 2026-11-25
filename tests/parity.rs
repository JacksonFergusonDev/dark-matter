use std::collections::HashMap;

use assert_cmd::cargo::cargo_bin_cmd;
use dark_matter::model::BloatRow;

fn run_export(use_brew_cli: bool) -> Vec<BloatRow> {
    let mut cmd = cargo_bin_cmd!("dark-matter");
    cmd.args(["export", "--format", "json"]);
    if use_brew_cli {
        cmd.env("DARK_MATTER_USE_BREW_CLI", "1");
    } else {
        cmd.env_remove("DARK_MATTER_USE_BREW_CLI");
    }
    let output = cmd.output().expect("failed to execute dark-matter");
    assert!(
        output.status.success(),
        "dark-matter export (use_brew_cli={use_brew_cli}) failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("export output is not valid BloatRow JSON")
}

fn run_stdout(args: &[&str], use_brew_cli: bool) -> String {
    let mut cmd = cargo_bin_cmd!("dark-matter");
    cmd.args(args);
    if use_brew_cli {
        cmd.env("DARK_MATTER_USE_BREW_CLI", "1");
    } else {
        cmd.env_remove("DARK_MATTER_USE_BREW_CLI");
    }
    let output = cmd.output().expect("failed to execute dark-matter");
    assert!(
        output.status.success(),
        "dark-matter {args:?} (use_brew_cli={use_brew_cli}) failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout is not valid UTF-8")
}

/// Differential oracle test comparing the pure-Rust receipt reader against
/// `brew info --json=v2 --installed` (`DARK_MATTER_USE_BREW_CLI=1`).
///
/// Run with:
///   `cargo test --test parity -- --ignored --nocapture`
#[test]
#[ignore = "requires a real Homebrew installation"]
fn parity_direct_receipts_match_brew_cli() {
    let cli_rows = run_export(true);
    let native_rows = run_export(false);

    assert!(
        !cli_rows.is_empty(),
        "expected non-empty Homebrew installation"
    );

    let cli_map: HashMap<&str, &BloatRow> = cli_rows
        .iter()
        .map(|row| (row.package.as_str(), row))
        .collect();
    let native_map: HashMap<&str, &BloatRow> = native_rows
        .iter()
        .map(|row| (row.package.as_str(), row))
        .collect();

    let mut mismatches = Vec::new();

    for (&pkg, &cli_row) in &cli_map {
        match native_map.get(pkg) {
            None => mismatches.push(format!("missing in native: {pkg}")),
            Some(&native_row) => {
                let weighted_diff = (cli_row.weighted_bytes - native_row.weighted_bytes).abs();
                if cli_row.core_bytes != native_row.core_bytes
                    || cli_row.standard_bytes != native_row.standard_bytes
                    || cli_row.dep_count != native_row.dep_count
                    || cli_row.is_root != native_row.is_root
                    || cli_row.is_requested != native_row.is_requested
                    || weighted_diff > 1e-3
                {
                    mismatches.push(format!(
                        "{pkg}:\n  brew-cli: {cli_row:?}\n  native:   {native_row:?}"
                    ));
                }
            }
        }
    }

    for &pkg in native_map.keys() {
        if !cli_map.contains_key(pkg) {
            mismatches.push(format!("extra in native: {pkg}"));
        }
    }

    assert!(
        mismatches.is_empty(),
        "Found {} parity mismatch(es) between native receipt reader and `brew info --json=v2`:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );

    // Also verify `orphans` and `cleanup` reports match verbatim.
    assert_eq!(
        run_stdout(&["orphans"], false),
        run_stdout(&["orphans"], true),
        "`orphans` output differed between native and brew-cli modes"
    );
    assert_eq!(
        run_stdout(&["cleanup"], false),
        run_stdout(&["cleanup"], true),
        "`cleanup` output differed between native and brew-cli modes"
    );

    println!(
        "Parity OK: {} packages matched `brew info --json=v2 --installed` exactly.",
        native_rows.len()
    );
}
