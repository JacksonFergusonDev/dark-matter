use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use owo_colors::OwoColorize;

use crate::VERSION;
use crate::core;
use crate::display;
use crate::error::{self, DarkMatterError, Result};
use crate::homebrew::{
    self, BrewRunner, DefaultTheoreticalSizer, FsSizer, PhysicalSizer, SystemBrewRunner,
    TheoreticalSizer,
};
use crate::model::{BloatRow, BrewMetadata, ExportFormat, ExportSource, SnapshotRow, SortKey};

const ALL_HELP: &str = "Also list dependencies, not only the packages their sizes are shared by.";
const ARCH_HELP: &str =
    "Target bottle tag (e.g. arm64_tahoe, x86_64_linux). Defaults to this machine's.";

const EXPORT_CSV_HEADERS: [&str; 8] = [
    "Package",
    "Core_Bytes",
    "Standard_Bytes",
    "Weighted_Bytes",
    "Bloat_Ratio",
    "Dep_Count",
    "Is_Root",
    "Is_Requested",
];

/// Analyze dependency bloat and storage mass of Homebrew installations.
#[derive(Debug, Parser)]
#[command(
    name = "dark-matter",
    about = "Analyze dependency bloat and storage mass of Homebrew installations.",
    disable_version_flag = true
)]
pub struct Cli {
    /// Enable verbose logging.
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,

    /// Show the application version and exit.
    #[arg(long)]
    pub version: bool,

    /// Install completion for the current shell.
    #[arg(long, value_name = "SHELL")]
    pub install_completion: Option<Option<Shell>>,

    /// Show completion for the current shell, to copy it or customize the installation.
    #[arg(long, value_name = "SHELL")]
    pub show_completion: Option<Option<Shell>>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Execute the storage bloat analysis pipeline.
    Analyze {
        /// Sorting metric.
        #[arg(short = 's', long = "sort", value_enum, default_value_t = SortKey::Ratio)]
        sort_by: SortKey,

        /// Number of packages to display.
        #[arg(short = 'n', long = "top", default_value_t = 20, value_parser = parse_positive_usize)]
        top_n: usize,

        /// Attribute shared dependencies fractionally (default).
        #[arg(long, conflicts_with = "standard")]
        fractional: bool,

        /// Count full dependency sizes without fractional splitting.
        #[arg(long, conflicts_with = "fractional")]
        standard: bool,

        #[arg(short = 'A', long = "all", help = ALL_HELP)]
        show_all: bool,
    },

    /// Evaluate the complete theoretical ecosystem leaderboard.
    Leaderboard {
        /// Sorting metric.
        #[arg(short = 's', long = "sort", value_enum, default_value_t = SortKey::Ratio)]
        sort_by: SortKey,

        /// Number of packages to display.
        #[arg(short = 'n', long = "top", default_value_t = 20, value_parser = parse_positive_usize)]
        top_n: usize,

        #[arg(short = 'a', long, help = ARCH_HELP)]
        arch: Option<String>,

        #[arg(short = 'A', long = "all", help = ALL_HELP)]
        show_all: bool,
    },

    /// Evaluate the bloat of a single target package.
    Inspect {
        /// The specific package to analyze.
        #[arg(add = ArgValueCompleter::new(installed_value_completer))]
        package: String,

        #[arg(short = 's', long, value_enum, default_value_t = ExportSource::Installed)]
        source: ExportSource,

        #[arg(short = 'a', long, help = ARCH_HELP)]
        arch: Option<String>,
    },

    /// Evaluate and compare the bloat of multiple packages.
    Compare {
        /// The specific packages to compare.
        #[arg(required = true, num_args = 1.., add = ArgValueCompleter::new(installed_value_completer))]
        packages: Vec<String>,

        /// Sorting metric.
        #[arg(short = 's', long = "sort", value_enum, default_value_t = SortKey::Ratio)]
        sort_by: SortKey,

        #[arg(long, value_enum, default_value_t = ExportSource::Installed)]
        source: ExportSource,

        #[arg(short = 'a', long, help = ARCH_HELP)]
        arch: Option<String>,
    },

    /// Break down the bloat of a package by its dependencies.
    Explain {
        /// The specific package to analyze.
        #[arg(add = ArgValueCompleter::new(installed_value_completer))]
        package: String,

        #[arg(short = 's', long, value_enum, default_value_t = ExportSource::Installed)]
        source: ExportSource,

        #[arg(short = 'a', long, help = ARCH_HELP)]
        arch: Option<String>,

        /// Show the dependency tree instead of a table.
        #[arg(short = 't', long)]
        tree: bool,
    },

    /// Export the computed bloat analysis data for external pipelines.
    Export {
        #[arg(short = 's', long, value_enum, default_value_t = ExportSource::Installed)]
        source: ExportSource,

        #[arg(short = 'f', long = "format", value_enum, default_value_t = ExportFormat::Csv)]
        export_format: ExportFormat,

        #[arg(short = 'a', long, help = ARCH_HELP)]
        arch: Option<String>,
    },

    /// Measure the space freed by uninstalling packages and their unused deps.
    Reclaim {
        /// The packages to uninstall.
        #[arg(required = true, num_args = 1.., add = ArgValueCompleter::new(installed_value_completer))]
        packages: Vec<String>,
    },

    /// List installed dependencies that nothing you requested still needs.
    Orphans,

    /// List old package versions still taking up space in the Cellar.
    Cleanup,

    /// Compare two snapshots made with `export` to see what grew or shrank.
    Diff {
        /// The earlier `export` snapshot.
        old: PathBuf,

        /// The later `export` snapshot.
        new: PathBuf,

        /// Number of packages to display.
        #[arg(short = 'n', long = "top", default_value_t = 20, value_parser = parse_positive_usize)]
        top_n: usize,
    },
}

/// Runtime dependencies and I/O streams for executing the CLI.
pub struct CliContext<'a> {
    pub runner: &'a dyn BrewRunner,
    pub physical_sizer: &'a dyn PhysicalSizer,
    pub theoretical_sizer: &'a dyn TheoreticalSizer,
    pub api_dir: Option<&'a Path>,
    pub stdin: &'a mut dyn BufRead,
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
    pub stdout_color: bool,
    pub stderr_color: bool,
    pub interactive_spinners: bool,
}

fn parse_positive_usize(s: &str) -> std::result::Result<usize, String> {
    let val: usize = s
        .parse()
        .map_err(|_| format!("invalid integer value '{s}'"))?;
    if val == 0 {
        return Err("value must be at least 1".to_string());
    }
    Ok(val)
}

fn installed_value_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let incomplete = current.to_string_lossy();
    complete_installed(&incomplete)
        .into_iter()
        .map(CompletionCandidate::new)
        .collect()
}

/// Shell-complete the names of installed formulae and casks.
///
/// Reads the `Cellar` and `Caskroom` directly so completion stays fast, and
/// offers nothing rather than an error when Homebrew cannot be found.
pub fn complete_installed(incomplete: &str) -> Vec<String> {
    let prefix_override = std::env::var_os("HOMEBREW_PREFIX")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    complete_installed_with(incomplete, prefix_override, &SystemBrewRunner)
}

/// Shell-complete installed package names using an optional prefix override and custom runner.
pub fn complete_installed_with(
    incomplete: &str,
    prefix_override: Option<PathBuf>,
    runner: &dyn BrewRunner,
) -> Vec<String> {
    let prefix = match prefix_override {
        Some(p) => p,
        None => match homebrew::get_brew_prefix_with(runner) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        },
    };

    let mut names = BTreeSet::new();
    for rack in ["Cellar", "Caskroom"] {
        let dir = prefix.join(rack);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(name) = entry.file_name().into_string()
                && name.starts_with(incomplete)
            {
                names.insert(name);
            }
        }
    }
    names.into_iter().collect()
}

fn configure_logging(verbose: bool) {
    let _ = env_logger::Builder::new()
        .target(env_logger::Target::Stderr)
        .filter_level(log::LevelFilter::Trace)
        .format_timestamp(None)
        .try_init();
    let level = if verbose {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Warn
    };
    log::set_max_level(level);
}

fn with_spinner<T>(interactive: bool, to_stderr: bool, msg: &str, f: impl FnOnce() -> T) -> T {
    let target_is_tty = if to_stderr {
        io::stderr().is_terminal()
    } else {
        io::stdout().is_terminal()
    };
    if !interactive || !target_is_tty {
        return f();
    }
    let target = if to_stderr {
        ProgressDrawTarget::stderr()
    } else {
        ProgressDrawTarget::stdout()
    };
    let pb = ProgressBar::with_draw_target(None, target);
    let style = ProgressStyle::with_template("{spinner:.yellow} {msg:.yellow}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner());
    pb.set_style(style);
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    let result = f();
    pb.finish_and_clear();
    result
}

fn print_pipeline_error(w: &mut dyn Write, err: &DarkMatterError, color: bool) {
    if color {
        let _ = writeln!(w, "{} {err}", "Pipeline Error:".red().bold());
    } else {
        let _ = writeln!(w, "Pipeline Error: {err}");
    }
}

fn print_cyan_bold(w: &mut dyn Write, msg: &str, color: bool) {
    if color {
        let _ = writeln!(w, "{}", msg.cyan().bold());
    } else {
        let _ = writeln!(w, "{msg}");
    }
}

fn dedup_preserve_order(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}

fn detect_shell(shell_env: Option<&str>) -> Result<Shell> {
    let name = shell_env
        .and_then(|s| Path::new(s).file_name())
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase);
    match name.as_deref() {
        Some("zsh") => Ok(Shell::Zsh),
        Some("bash") => Ok(Shell::Bash),
        Some("fish") => Ok(Shell::Fish),
        Some("elvish") => Ok(Shell::Elvish),
        Some("pwsh" | "powershell") => Ok(Shell::PowerShell),
        _ => Err(DarkMatterError::Pipeline(
            "Could not detect shell from $SHELL; pass one explicitly (e.g. --install-completion zsh)."
                .to_string(),
        )),
    }
}

fn completion_hook(shell: Shell) -> Result<&'static str> {
    match shell {
        Shell::Zsh => Ok("source <(COMPLETE=zsh dark-matter)"),
        Shell::Bash => Ok("source <(COMPLETE=bash dark-matter)"),
        Shell::Fish => Ok("COMPLETE=fish dark-matter | source"),
        Shell::Elvish => Ok("eval (E:COMPLETE=elvish dark-matter | slurp)"),
        Shell::PowerShell => Ok(
            "$env:COMPLETE = \"powershell\"; dark-matter | Out-String | Invoke-Expression; Remove-Item Env:\\COMPLETE",
        ),
        _ => Err(DarkMatterError::Pipeline(format!(
            "Unsupported shell for completion: {shell}"
        ))),
    }
}

fn completion_config_path(shell: Shell, home: &Path) -> Result<PathBuf> {
    match shell {
        Shell::Zsh => Ok(home.join(".zshrc")),
        Shell::Bash => Ok(home.join(".bashrc")),
        Shell::Fish => Ok(home.join(".config/fish/completions/dark-matter.fish")),
        Shell::Elvish => Ok(home.join(".config/elvish/rc.elv")),
        Shell::PowerShell => Ok(home.join(".config/powershell/Microsoft.PowerShell_profile.ps1")),
        _ => Err(DarkMatterError::Pipeline(format!(
            "Unsupported shell for completion: {shell}"
        ))),
    }
}

/// Install the dynamic completion hook for `shell` under `home`, idempotently.
pub fn install_completion_in(shell: Shell, home: &Path, out: &mut dyn Write) -> Result<PathBuf> {
    let hook = completion_hook(shell)?;
    let path = completion_config_path(shell, home)?;

    if path.exists() {
        let existing = fs::read_to_string(&path).map_err(|err| {
            DarkMatterError::Pipeline(format!("Could not read '{}': {err}", path.display()))
        })?;
        if existing.contains(hook) {
            let _ = writeln!(
                out,
                "{shell} completion is already installed in {}.",
                path.display()
            );
            return Ok(path);
        }
        let mut file = OpenOptions::new().append(true).open(&path).map_err(|err| {
            DarkMatterError::Pipeline(format!("Could not open '{}': {err}", path.display()))
        })?;
        let prefix = if existing.is_empty() || existing.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        write!(file, "{prefix}# dark-matter shell completion\n{hook}\n").map_err(|err| {
            DarkMatterError::Pipeline(format!("Could not write '{}': {err}", path.display()))
        })?;
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                DarkMatterError::Pipeline(format!("Could not create '{}': {err}", parent.display()))
            })?;
        }
        fs::write(&path, format!("# dark-matter shell completion\n{hook}\n")).map_err(|err| {
            DarkMatterError::Pipeline(format!("Could not write '{}': {err}", path.display()))
        })?;
    }

    let _ = writeln!(
        out,
        "Installed {shell} completion in {}. Restart your shell to activate it.",
        path.display()
    );
    Ok(path)
}

fn resolve_shell(shell_opt: Option<Shell>) -> Result<Shell> {
    if let Some(shell) = shell_opt {
        Ok(shell)
    } else {
        let env_shell = std::env::var("SHELL").ok();
        detect_shell(env_shell.as_deref())
    }
}

fn load_installed_context(
    ctx: &CliContext<'_>,
    spinner_to_stderr: bool,
    target_msg: Option<&str>,
) -> Result<(PathBuf, BrewMetadata)> {
    let msg = target_msg.unwrap_or("Scanning physical disk footprint...");
    with_spinner(ctx.interactive_spinners, spinner_to_stderr, msg, || {
        std::thread::scope(|s| {
            let prefix_handle = s.spawn(|| homebrew::get_brew_prefix_with(ctx.runner));
            let metadata_res = homebrew::get_brew_metadata_with(ctx.runner);
            let prefix_res = prefix_handle
                .join()
                .unwrap_or_else(|_| homebrew::get_brew_prefix_with(ctx.runner));
            Ok((prefix_res?, metadata_res?))
        })
    })
}

fn load_catalog_context(
    ctx: &CliContext<'_>,
    spinner_to_stderr: bool,
    target_msg: Option<&str>,
    arch: Option<&str>,
) -> Result<BrewMetadata> {
    let msg = target_msg.unwrap_or("Computing theoretical ecosystem DAG...");
    with_spinner(ctx.interactive_spinners, spinner_to_stderr, msg, || {
        homebrew::get_theoretical_catalog_with(ctx.runner, ctx.api_dir, arch, None)
    })
}

fn read_confirm(reader: &mut dyn BufRead, default: bool) -> bool {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => {
            let trimmed = line.trim().to_ascii_lowercase();
            if trimmed.is_empty() {
                default
            } else {
                matches!(trimmed.as_str(), "y" | "yes")
            }
        }
    }
}

/// Build a targeted result, offering the theoretical catalog when packages are not installed.
///
/// Returns `Ok(None)` when the user declines the catalog fallback and no targets are installed.
fn build_with_catalog_fallback<T, FI, FC>(
    ctx: &mut CliContext<'_>,
    packages: &[String],
    source: ExportSource,
    arch: Option<&str>,
    build_installed: FI,
    build_catalog: FC,
) -> Result<Option<(T, bool)>>
where
    FI: Fn(&BrewMetadata, &Path, &[String], &dyn PhysicalSizer) -> Result<T>,
    FC: Fn(&BrewMetadata, &[String], Option<&str>, &dyn TheoreticalSizer) -> Result<T>,
{
    let status = Some("Resolving data for targets...");
    if source == ExportSource::Catalog {
        let metadata = load_catalog_context(ctx, false, status, arch)?;
        let result = build_catalog(&metadata, packages, arch, ctx.theoretical_sizer)?;
        return Ok(Some((result, true)));
    }

    let (prefix, metadata) = load_installed_context(ctx, false, status)?;
    match build_installed(&metadata, &prefix, packages, ctx.physical_sizer) {
        Ok(result) => Ok(Some((result, false))),
        Err(DarkMatterError::PackageNotFound {
            packages: missing, ..
        }) => {
            let names = error::format_package_names(&missing);
            let verb = if missing.len() == 1 { "is" } else { "are" };
            let _ = write!(
                ctx.stdout,
                "\n{names} {verb} not installed. Fall back to the theoretical catalog? [Y/n]: "
            );
            let _ = ctx.stdout.flush();

            let use_catalog = read_confirm(ctx.stdin, true);
            if !use_catalog {
                let missing_set: HashSet<&str> = missing.iter().map(String::as_str).collect();
                let installed: Vec<String> = packages
                    .iter()
                    .filter(|p| !missing_set.contains(p.as_str()))
                    .cloned()
                    .collect();
                if installed.is_empty() {
                    return Ok(None);
                }
                let result = build_installed(&metadata, &prefix, &installed, ctx.physical_sizer)?;
                return Ok(Some((result, false)));
            }

            let catalog_metadata =
                load_catalog_context(ctx, false, Some("Loading theoretical catalog..."), arch)?;
            let result = build_catalog(&catalog_metadata, packages, arch, ctx.theoretical_sizer)?;
            Ok(Some((result, true)))
        }
        Err(err) => Err(err),
    }
}

fn snapshot_read_err(path: &Path, err: &dyn std::fmt::Display) -> DarkMatterError {
    DarkMatterError::Snapshot(format!(
        "Could not read snapshot '{}': {err}",
        path.display()
    ))
}

fn validate_snapshot_columns(present: &HashSet<&str>, label: &str) -> Result<()> {
    let missing: Vec<&str> = core::SNAPSHOT_COLUMNS
        .iter()
        .copied()
        .filter(|col| !present.contains(col))
        .collect();
    if !missing.is_empty() {
        return Err(DarkMatterError::Snapshot(format!(
            "The {label} snapshot is missing {}; create snapshots with `dark-matter export`.",
            missing.join(", ")
        )));
    }
    Ok(())
}

/// Load an `export` snapshot from CSV or JSON and validate required columns.
pub fn read_snapshot(path: &Path, label: &str) -> Result<Vec<SnapshotRow>> {
    let is_csv = path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("csv"));

    if is_csv {
        let mut rdr = csv::Reader::from_path(path).map_err(|err| snapshot_read_err(path, &err))?;
        let headers = rdr.headers().map_err(|err| snapshot_read_err(path, &err))?;
        let header_set: HashSet<&str> = headers.iter().collect();
        validate_snapshot_columns(&header_set, label)?;

        let mut rows = Vec::new();
        for record in rdr.deserialize::<SnapshotRow>() {
            let row = record.map_err(|err| snapshot_read_err(path, &err))?;
            rows.push(row);
        }
        return Ok(rows);
    }

    let raw = fs::read_to_string(path).map_err(|err| snapshot_read_err(path, &err))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|err| snapshot_read_err(path, &err))?;
    let Some(items) = value.as_array() else {
        return Err(DarkMatterError::Snapshot(format!(
            "Could not read snapshot '{}': expected a JSON array",
            path.display()
        )));
    };
    if items.is_empty() {
        return Ok(Vec::new());
    }
    for item in items {
        let Some(obj) = item.as_object() else {
            return Err(DarkMatterError::Snapshot(format!(
                "Could not read snapshot '{}': expected JSON objects",
                path.display()
            )));
        };
        let keys: HashSet<&str> = obj.keys().map(String::as_str).collect();
        validate_snapshot_columns(&keys, label)?;
    }
    serde_json::from_value(value).map_err(|err| snapshot_read_err(path, &err))
}

fn write_export_csv(w: &mut dyn Write, rows: &[BloatRow]) -> Result<()> {
    let mut wtr = csv::WriterBuilder::new().has_headers(false).from_writer(w);
    wtr.write_record(EXPORT_CSV_HEADERS)
        .map_err(error::pipeline_err)?;
    for row in rows {
        wtr.serialize(row).map_err(error::pipeline_err)?;
    }
    wtr.flush().map_err(error::pipeline_err)
}

fn write_export_json(w: &mut dyn Write, rows: &[BloatRow]) -> Result<()> {
    let mut buf = io::BufWriter::new(w);
    serde_json::to_writer_pretty(&mut buf, rows).map_err(error::pipeline_err)?;
    writeln!(buf).map_err(error::pipeline_err)?;
    buf.flush().map_err(error::pipeline_err)
}

fn cmd_analyze(
    ctx: &mut CliContext<'_>,
    sort_by: SortKey,
    top_n: usize,
    use_fractional: bool,
    show_all: bool,
) -> Result<i32> {
    print_cyan_bold(
        ctx.stdout,
        "Initializing Dark Matter pipeline...",
        ctx.stdout_color,
    );
    let (prefix, metadata) = load_installed_context(ctx, false, None)?;
    let formulae_count = metadata.formulae.len();
    let casks_count = metadata.casks.len();
    if ctx.stdout_color {
        let _ = writeln!(
            ctx.stdout,
            "Successfully parsed {} formulae and {} casks.",
            formulae_count.bold(),
            casks_count.bold()
        );
    } else {
        let _ = writeln!(
            ctx.stdout,
            "Successfully parsed {formulae_count} formulae and {casks_count} casks."
        );
    }
    let rows = with_spinner(
        ctx.interactive_spinners,
        false,
        "Computing fractional attribution DAG...",
        || core::build_analysis_with(&metadata, &prefix, ctx.physical_sizer),
    );
    let _ = display::render_bloat_table_to(
        &mut ctx.stdout,
        &rows,
        sort_by,
        top_n,
        use_fractional,
        false,
        !show_all,
        ctx.stdout_color,
    );
    Ok(0)
}

fn cmd_leaderboard(
    ctx: &mut CliContext<'_>,
    sort_by: SortKey,
    top_n: usize,
    arch: Option<&str>,
    show_all: bool,
) -> Result<i32> {
    print_cyan_bold(
        ctx.stdout,
        "Accessing local ecosystem cache...",
        ctx.stdout_color,
    );
    let metadata = load_catalog_context(ctx, false, None, arch)?;
    let rows = with_spinner(
        ctx.interactive_spinners,
        false,
        "Processing massive theoretical DAG...",
        || core::build_theoretical_with(&metadata, arch, ctx.theoretical_sizer),
    );
    let _ = display::render_bloat_table_to(
        &mut ctx.stdout,
        &rows,
        sort_by,
        top_n,
        true,
        true,
        !show_all,
        ctx.stdout_color,
    );
    Ok(0)
}

fn cmd_inspect(
    ctx: &mut CliContext<'_>,
    package: String,
    source: ExportSource,
    arch: Option<&str>,
) -> Result<i32> {
    print_cyan_bold(
        ctx.stdout,
        &format!("Inspecting bloat for '{package}'..."),
        ctx.stdout_color,
    );
    let targets = [package];
    let Some((rows, is_theoretical)) = build_with_catalog_fallback(
        ctx,
        &targets,
        source,
        arch,
        |metadata, prefix, pkgs, sizer| {
            core::build_targeted_analysis_with(metadata, prefix, &pkgs[0], sizer)
        },
        |metadata, pkgs, arch_opt, sizer| {
            core::build_targeted_theoretical_with(metadata, &pkgs[0], arch_opt, sizer)
        },
    )?
    else {
        return Ok(1);
    };
    let _ = display::render_bloat_table_to(
        &mut ctx.stdout,
        &rows,
        SortKey::Ratio,
        1,
        true,
        is_theoretical,
        false,
        ctx.stdout_color,
    );
    Ok(0)
}

fn cmd_compare(
    ctx: &mut CliContext<'_>,
    packages: Vec<String>,
    sort_by: SortKey,
    source: ExportSource,
    arch: Option<&str>,
) -> Result<i32> {
    let packages = dedup_preserve_order(packages);
    print_cyan_bold(
        ctx.stdout,
        &format!("Comparing bloat for: {}", packages.join(", ")),
        ctx.stdout_color,
    );
    let Some((rows, is_theoretical)) = build_with_catalog_fallback(
        ctx,
        &packages,
        source,
        arch,
        |metadata, prefix, pkgs, sizer| {
            core::build_compare_analysis_with(metadata, prefix, pkgs, sizer)
        },
        |metadata, pkgs, arch_opt, sizer| {
            core::build_compare_theoretical_with(metadata, pkgs, arch_opt, sizer)
        },
    )?
    else {
        return Ok(1);
    };
    let _ = display::render_bloat_table_to(
        &mut ctx.stdout,
        &rows,
        sort_by,
        packages.len(),
        true,
        is_theoretical,
        false,
        ctx.stdout_color,
    );
    Ok(0)
}

fn cmd_explain(
    ctx: &mut CliContext<'_>,
    package: &str,
    source: ExportSource,
    arch: Option<&str>,
    tree: bool,
) -> Result<i32> {
    print_cyan_bold(
        ctx.stdout,
        &format!("Explaining fractional dependencies for '{package}'..."),
        ctx.stdout_color,
    );
    let targets = [package.to_string()];
    let Some((rows, is_theoretical)) = build_with_catalog_fallback(
        ctx,
        &targets,
        source,
        arch,
        |metadata, prefix, pkgs, sizer| {
            core::build_explain_analysis_with(metadata, prefix, &pkgs[0], sizer)
        },
        |metadata, pkgs, arch_opt, sizer| {
            core::build_explain_theoretical_with(metadata, &pkgs[0], arch_opt, sizer)
        },
    )?
    else {
        return Ok(1);
    };
    if tree {
        let _ = display::render_explain_tree_to(
            &mut ctx.stdout,
            &rows,
            package,
            is_theoretical,
            ctx.stdout_color,
        );
    } else {
        let _ = display::render_explain_table_to(
            &mut ctx.stdout,
            &rows,
            package,
            is_theoretical,
            ctx.stdout_color,
        );
    }
    Ok(0)
}

fn cmd_export(
    ctx: &mut CliContext<'_>,
    source: ExportSource,
    export_format: ExportFormat,
    arch: Option<&str>,
) -> Result<i32> {
    let rows = if source == ExportSource::Catalog {
        let metadata = load_catalog_context(ctx, true, None, arch)?;
        with_spinner(
            ctx.interactive_spinners,
            true,
            "Computing fractional attribution DAG...",
            || core::build_theoretical_with(&metadata, arch, ctx.theoretical_sizer),
        )
    } else {
        let (prefix, metadata) = load_installed_context(ctx, true, None)?;
        with_spinner(
            ctx.interactive_spinners,
            true,
            "Computing fractional attribution DAG...",
            || core::build_analysis_with(&metadata, &prefix, ctx.physical_sizer),
        )
    };
    match export_format {
        ExportFormat::Csv => write_export_csv(ctx.stdout, &rows)?,
        ExportFormat::Json => write_export_json(ctx.stdout, &rows)?,
    }
    Ok(0)
}

fn cmd_reclaim(ctx: &mut CliContext<'_>, packages: Vec<String>) -> Result<i32> {
    let packages = dedup_preserve_order(packages);
    let (prefix, metadata) = load_installed_context(ctx, false, None)?;
    let result = with_spinner(
        ctx.interactive_spinners,
        false,
        "Measuring reclaimable packages...",
        || core::build_reclaim_with(&metadata, &prefix, &packages, ctx.physical_sizer),
    )?;
    let _ = display::render_reclaim_to(&mut ctx.stdout, &result, &packages, ctx.stdout_color);
    Ok(0)
}

fn cmd_orphans(ctx: &mut CliContext<'_>) -> Result<i32> {
    let (prefix, metadata) = load_installed_context(ctx, false, None)?;
    let rows = with_spinner(
        ctx.interactive_spinners,
        false,
        "Measuring orphaned dependencies...",
        || core::build_orphans_with(&metadata, &prefix, ctx.physical_sizer),
    );
    let _ = display::render_orphans_to(&mut ctx.stdout, &rows, ctx.stdout_color);
    Ok(0)
}

fn cmd_cleanup(ctx: &mut CliContext<'_>) -> Result<i32> {
    let (prefix, metadata) = load_installed_context(ctx, false, None)?;
    let rows = with_spinner(
        ctx.interactive_spinners,
        false,
        "Measuring old versions...",
        || core::build_cleanup_with(&metadata, &prefix, ctx.physical_sizer),
    );
    let _ = display::render_cleanup_to(&mut ctx.stdout, &rows, ctx.stdout_color);
    Ok(0)
}

fn cmd_diff(ctx: &mut CliContext<'_>, old: &Path, new: &Path, top_n: usize) -> Result<i32> {
    let before = read_snapshot(old, "old")?;
    let after = read_snapshot(new, "new")?;
    let rows = core::diff_snapshots(&before, &after);
    let old_total: f64 = before.iter().map(|r| r.core_bytes).sum();
    let new_total: f64 = after.iter().map(|r| r.core_bytes).sum();
    let _ = display::render_diff_to(
        &mut ctx.stdout,
        &rows,
        top_n,
        old_total,
        new_total,
        ctx.stdout_color,
    );
    Ok(0)
}

fn execute_command(cmd: Commands, ctx: &mut CliContext<'_>) -> Result<i32> {
    match cmd {
        Commands::Analyze {
            sort_by,
            top_n,
            fractional: _,
            standard,
            show_all,
        } => cmd_analyze(ctx, sort_by, top_n, !standard, show_all),
        Commands::Leaderboard {
            sort_by,
            top_n,
            arch,
            show_all,
        } => cmd_leaderboard(ctx, sort_by, top_n, arch.as_deref(), show_all),
        Commands::Inspect {
            package,
            source,
            arch,
        } => cmd_inspect(ctx, package, source, arch.as_deref()),
        Commands::Compare {
            packages,
            sort_by,
            source,
            arch,
        } => cmd_compare(ctx, packages, sort_by, source, arch.as_deref()),
        Commands::Explain {
            package,
            source,
            arch,
            tree,
        } => cmd_explain(ctx, &package, source, arch.as_deref(), tree),
        Commands::Export {
            source,
            export_format,
            arch,
        } => cmd_export(ctx, source, export_format, arch.as_deref()),
        Commands::Reclaim { packages } => cmd_reclaim(ctx, packages),
        Commands::Orphans => cmd_orphans(ctx),
        Commands::Cleanup => cmd_cleanup(ctx),
        Commands::Diff { old, new, top_n } => cmd_diff(ctx, &old, &new, top_n),
    }
}

/// Parse CLI arguments and execute against the provided [`CliContext`], returning the exit code.
pub fn run_args<I, T>(args: I, ctx: &mut CliContext<'_>) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(err) => {
            let code = err.exit_code();
            let rendered = if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                if ctx.stdout_color {
                    err.render().ansi().to_string()
                } else {
                    err.render().to_string()
                }
            } else if ctx.stderr_color {
                err.render().ansi().to_string()
            } else {
                err.render().to_string()
            };
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                let _ = write!(ctx.stdout, "{rendered}");
            } else {
                let _ = write!(ctx.stderr, "{rendered}");
            }
            return code;
        }
    };

    if cli.version {
        if ctx.stdout_color {
            let _ = writeln!(ctx.stdout, "dark-matter version {}", VERSION.cyan().bold());
        } else {
            let _ = writeln!(ctx.stdout, "dark-matter version {VERSION}");
        }
        return 0;
    }

    if let Some(shell_opt) = cli.show_completion {
        return match resolve_shell(shell_opt).and_then(completion_hook) {
            Ok(hook) => {
                let _ = writeln!(ctx.stdout, "{hook}");
                0
            }
            Err(err) => {
                print_pipeline_error(ctx.stdout, &err, ctx.stdout_color);
                1
            }
        };
    }

    if let Some(shell_opt) = cli.install_completion {
        let res = resolve_shell(shell_opt).and_then(|shell| {
            let home = dirs::home_dir().ok_or_else(|| {
                DarkMatterError::Pipeline("Could not determine home directory.".to_string())
            })?;
            install_completion_in(shell, &home, ctx.stdout).map(|_| ())
        });
        return match res {
            Ok(()) => 0,
            Err(err) => {
                print_pipeline_error(ctx.stdout, &err, ctx.stdout_color);
                1
            }
        };
    }

    let Some(command) = cli.command else {
        let mut cmd = Cli::command();
        let help = cmd.render_help();
        if ctx.stderr_color {
            let _ = write!(ctx.stderr, "{}", help.ansi());
        } else {
            let _ = write!(ctx.stderr, "{help}");
        }
        return 2;
    };

    configure_logging(cli.verbose);

    let error_to_stderr = matches!(command, Commands::Export { .. });
    match execute_command(command, ctx) {
        Ok(code) => code,
        Err(err) => {
            if error_to_stderr {
                print_pipeline_error(ctx.stderr, &err, ctx.stderr_color);
            } else {
                print_pipeline_error(ctx.stdout, &err, ctx.stdout_color);
            }
            1
        }
    }
}

/// Entry point for the `dark-matter` binary.
pub fn run() -> i32 {
    clap_complete::env::CompleteEnv::with_factory(Cli::command).complete();

    let stdin = io::stdin();
    let mut stdin_lock = stdin.lock();
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();

    let stdout_color = display::stdout_supports_color();
    let stderr_color = display::stderr_supports_color();
    let interactive_spinners = io::stdout().is_terminal() || io::stderr().is_terminal();

    let mut ctx = CliContext {
        runner: &SystemBrewRunner,
        physical_sizer: &FsSizer,
        theoretical_sizer: &DefaultTheoreticalSizer,
        api_dir: None,
        stdin: &mut stdin_lock,
        stdout: &mut stdout,
        stderr: &mut stderr,
        stdout_color,
        stderr_color,
        interactive_spinners,
    };

    run_args(std::env::args_os(), &mut ctx)
}
