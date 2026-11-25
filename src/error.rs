use std::fmt;

use thiserror::Error;

pub const INSTALLED: &str = "local installation";
pub const CATALOG: &str = "catalog";

/// All errors surfaced by the Dark Matter analysis pipeline.
#[derive(Debug, Error)]
pub enum DarkMatterError {
    /// One or more requested packages are absent from the analyzed graph.
    #[error("{}", format_missing_packages(.packages, .location))]
    PackageNotFound {
        packages: Vec<String>,
        location: &'static str,
    },

    /// One or more requested catalog targets are casks, which have no bottles.
    #[error("{}", format_cask_in_catalog(.casks))]
    CaskInCatalog { casks: Vec<String> },

    /// Homebrew execution, catalog loading, or general pipeline failure.
    #[error("{0}")]
    Pipeline(String),

    /// Snapshot reading or schema validation failure.
    #[error("{0}")]
    Snapshot(String),
}

pub type Result<T> = std::result::Result<T, DarkMatterError>;

fn format_quoted_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("'{item}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_missing_packages(packages: &[String], location: &str) -> String {
    let noun = if packages.len() == 1 {
        "Package"
    } else {
        "Packages"
    };
    let names = format_quoted_list(packages);
    format!("{noun} {names} not found in the {location}.")
}

fn format_cask_in_catalog(casks: &[String]) -> String {
    let names = format_quoted_list(casks);
    let verb = if casks.len() == 1 {
        "is a cask"
    } else {
        "are casks"
    };
    format!(
        "{names} {verb}. Casks have no bottles, so the catalog can't size them; \
         install and analyze them with --source installed."
    )
}

/// Format a list of quoted package names for prompts (e.g. `'a', 'b'`).
pub fn format_package_names(packages: &[String]) -> String {
    format_quoted_list(packages)
}

/// Helper for converting any displayable error into a `Pipeline` error.
pub fn pipeline_err(msg: impl fmt::Display) -> DarkMatterError {
    DarkMatterError::Pipeline(msg.to_string())
}
