import contextlib
import sys
from collections.abc import Callable, Iterator
from enum import StrEnum
from pathlib import Path
from typing import Any

import pandas as pd
import typer
from rich.console import Console

from dark_matter_cli import (
    PackageNotFoundError,
    __version__,
    build_analysis_dataframe,
    build_compare_analysis_dataframe,
    build_compare_theoretical_dataframe,
    build_explain_analysis_dataframe,
    build_explain_theoretical_dataframe,
    build_targeted_analysis_dataframe,
    build_targeted_theoretical_dataframe,
    build_theoretical_dataframe,
    display,
    get_brew_metadata,
    get_brew_prefix,
    get_theoretical_catalog,
)
from dark_matter_cli.logger import get_logger

app = typer.Typer(
    help="Analyze dependency bloat and storage mass of macOS Homebrew installations.",
    add_completion=False,
)
console = Console()

ARCH_HELP = (
    "Target bottle tag (e.g. arm64_tahoe, x86_64_linux). Defaults to this machine's."
)


class ExportFormat(StrEnum):
    """Supported serialized output formats for the export command."""

    csv = "csv"
    json = "json"


class SortKey(StrEnum):
    """Supported sorting metrics for bloat tables."""

    ratio = "ratio"
    core = "core"
    recursive = "recursive"


class ExportSource(StrEnum):
    """Supported theoretical or physical data sources for the export command."""

    installed = "installed"
    catalog = "catalog"


@contextlib.contextmanager
def handle_pipeline_errors(active_console: Console = console) -> Iterator[None]:
    """Centralize exception catching and CLI exit codes."""
    try:
        yield
    except (RuntimeError, ValueError) as e:
        active_console.print(f"[bold red]Pipeline Error:[/bold red] {e}")
        raise typer.Exit(code=1) from e


def load_ecosystem_context(
    source: ExportSource,
    active_console: Console,
    target_msg: str | None = None,
    arch: str | None = None,
) -> tuple[Path, dict[str, Any], bool]:
    """Resolve the prefix and ingest the metadata payload while managing UI state."""
    prefix = get_brew_prefix()
    is_theoretical = source == ExportSource.catalog

    if not is_theoretical:
        msg = target_msg or "[yellow]Scanning physical disk footprint...[/yellow]"
        with active_console.status(msg):
            metadata = get_brew_metadata()
    else:
        msg = target_msg or "[yellow]Computing theoretical ecosystem DAG...[/yellow]"
        with active_console.status(msg):
            metadata = get_theoretical_catalog(tag=arch)

    return prefix, metadata, is_theoretical


InstalledBuilder = Callable[[dict[str, Any], Path, list[str]], pd.DataFrame]
CatalogBuilder = Callable[[dict[str, Any], list[str]], pd.DataFrame]


def build_with_catalog_fallback(
    packages: list[str],
    source: ExportSource,
    arch: str | None,
    build_installed: InstalledBuilder,
    build_catalog: CatalogBuilder,
) -> tuple[pd.DataFrame, bool]:
    """Build a targeted DataFrame, offering the catalog for uninstalled packages.

    When analyzing the local installation and some packages are missing, the
    user may switch the whole run to the theoretical catalog. Declining keeps
    the installed subset, or exits if none of the packages are installed.

    Returns:
        tuple[pd.DataFrame, bool]: The DataFrame and whether it is theoretical.
    """
    status = "[yellow]Resolving data for targets...[/yellow]"
    prefix, metadata, is_theoretical = load_ecosystem_context(
        source, console, target_msg=status, arch=arch
    )
    if is_theoretical:
        return build_catalog(metadata, packages), True

    try:
        return build_installed(metadata, prefix, packages), False
    except PackageNotFoundError as e:
        names = ", ".join(f"'{p}'" for p in e.packages)
        verb = "is" if len(e.packages) == 1 else "are"
        use_catalog = typer.confirm(
            f"\n{names} {verb} not installed. Fall back to the theoretical catalog?",
            default=True,
        )
        if not use_catalog:
            installed = [p for p in packages if p not in e.packages]
            if not installed:
                raise typer.Exit(code=1) from None
            return build_installed(metadata, prefix, installed), False

    _, metadata, _ = load_ecosystem_context(
        ExportSource.catalog,
        console,
        target_msg="[yellow]Loading theoretical catalog...[/yellow]",
        arch=arch,
    )
    return build_catalog(metadata, packages), True


def version_callback(value: bool) -> None:
    """Print the version and exit eagerly."""
    if value:
        console.print(f"dark-matter version [bold cyan]{__version__}[/bold cyan]")
        raise typer.Exit()


@app.callback()
def main(
    verbose: bool = typer.Option(
        False, "--verbose", "-v", help="Enable verbose logging."
    ),
    version: bool = typer.Option(
        None,
        "--version",
        help="Show the application version and exit.",
        callback=version_callback,
        is_eager=True,
    ),
) -> None:
    """Configure global CLI state."""
    get_logger("dark_matter_cli", verbose=verbose)


@app.command()
def analyze(
    sort_by: SortKey = typer.Option(
        SortKey.ratio, "--sort", "-s", help="Sorting metric."
    ),
    top_n: int = typer.Option(
        20, "--top", "-n", min=1, help="Number of packages to display."
    ),
    fractional: bool = typer.Option(True, "--fractional/--standard"),
) -> None:
    """Execute the storage bloat analysis pipeline."""
    console.print("[bold cyan]Initializing Dark Matter pipeline...[/bold cyan]")

    with handle_pipeline_errors():
        prefix, metadata, _ = load_ecosystem_context(ExportSource.installed, console)

        formulae_count = len(metadata.get("formulae", []))
        casks_count = len(metadata.get("casks", []))
        console.print(
            f"Successfully parsed [bold]{formulae_count}[/bold] formulae and [bold]{casks_count}[/bold] casks."
        )

        with console.status("[yellow]Computing fractional attribution DAG...[/yellow]"):
            df = build_analysis_dataframe(metadata, prefix)

        display.render_bloat_table(
            df, sort_by=sort_by, top_n=top_n, fractional=fractional
        )


@app.command()
def leaderboard(
    sort_by: SortKey = typer.Option(
        SortKey.ratio, "--sort", "-s", help="Sorting metric."
    ),
    top_n: int = typer.Option(
        20, "--top", "-n", min=1, help="Number of packages to display."
    ),
    arch: str | None = typer.Option(None, "--arch", "-a", help=ARCH_HELP),
) -> None:
    """Evaluate the complete theoretical ecosystem leaderboard."""
    console.print("[bold cyan]Accessing local ecosystem cache...[/bold cyan]")

    with handle_pipeline_errors():
        _, metadata, _ = load_ecosystem_context(
            ExportSource.catalog, console, arch=arch
        )

        with console.status("[yellow]Processing massive theoretical DAG...[/yellow]"):
            df = build_theoretical_dataframe(metadata, arch=arch)

        display.render_bloat_table(
            df, sort_by=sort_by, top_n=top_n, fractional=True, is_theoretical=True
        )


@app.command()
def inspect(
    package: str = typer.Argument(..., help="The specific package to analyze."),
    source: ExportSource = typer.Option(ExportSource.installed, "--source", "-s"),
    arch: str | None = typer.Option(None, "--arch", "-a", help=ARCH_HELP),
) -> None:
    """Evaluate the bloat of a single target package."""
    console.print(f"[bold cyan]Inspecting bloat for '{package}'...[/bold cyan]")

    with handle_pipeline_errors():
        df, is_theoretical = build_with_catalog_fallback(
            [package],
            source,
            arch,
            lambda metadata, prefix, targets: build_targeted_analysis_dataframe(
                metadata, prefix, target=targets[0]
            ),
            lambda metadata, targets: build_targeted_theoretical_dataframe(
                metadata, target=targets[0], arch=arch
            ),
        )

        display.render_bloat_table(
            df,
            sort_by=SortKey.ratio,
            top_n=1,
            fractional=True,
            is_theoretical=is_theoretical,
            leaves_only=False,
        )


@app.command()
def compare(
    packages: list[str] = typer.Argument(..., help="The specific packages to compare."),
    sort_by: SortKey = typer.Option(
        SortKey.ratio, "--sort", "-s", help="Sorting metric."
    ),
    source: ExportSource = typer.Option(ExportSource.installed, "--source"),
    arch: str | None = typer.Option(None, "--arch", "-a", help=ARCH_HELP),
) -> None:
    """Evaluate and compare the bloat of multiple packages."""
    packages = list(dict.fromkeys(packages))
    pkg_list_str = ", ".join(packages)
    console.print(f"[bold cyan]Comparing bloat for: {pkg_list_str}[/bold cyan]")

    with handle_pipeline_errors():
        df, is_theoretical = build_with_catalog_fallback(
            packages,
            source,
            arch,
            lambda metadata, prefix, targets: build_compare_analysis_dataframe(
                metadata, prefix, targets=targets
            ),
            lambda metadata, targets: build_compare_theoretical_dataframe(
                metadata, targets=targets, arch=arch
            ),
        )

        display.render_bloat_table(
            df,
            sort_by=sort_by,
            top_n=len(packages),
            fractional=True,
            is_theoretical=is_theoretical,
            leaves_only=False,
        )


@app.command()
def export(
    source: ExportSource = typer.Option(ExportSource.installed, "--source", "-s"),
    export_format: ExportFormat = typer.Option(ExportFormat.csv, "--format", "-f"),
    arch: str | None = typer.Option(None, "--arch", "-a", help=ARCH_HELP),
) -> None:
    """Export the computed bloat analysis dataframe for external pipelines."""
    err_console = Console(stderr=True)

    with handle_pipeline_errors(err_console):
        prefix, metadata, is_theoretical = load_ecosystem_context(
            source, err_console, arch=arch
        )

        with err_console.status(
            "[yellow]Computing fractional attribution DAG...[/yellow]"
        ):
            if is_theoretical:
                df = build_theoretical_dataframe(metadata, arch=arch)
            else:
                df = build_analysis_dataframe(metadata, prefix)

        if export_format == ExportFormat.csv:
            sys.stdout.write(df.to_csv(index=False))
        else:
            sys.stdout.write(df.to_json(orient="records", indent=2))
            sys.stdout.write("\n")


@app.command()
def explain(
    package: str = typer.Argument(..., help="The specific package to analyze."),
    source: ExportSource = typer.Option(ExportSource.installed, "--source", "-s"),
    arch: str | None = typer.Option(None, "--arch", "-a", help=ARCH_HELP),
) -> None:
    """Break down the bloat of a package by its dependencies."""
    console.print(
        f"[bold cyan]Explaining fractional dependencies for '{package}'...[/bold cyan]"
    )

    with handle_pipeline_errors():
        df, is_theoretical = build_with_catalog_fallback(
            [package],
            source,
            arch,
            lambda metadata, prefix, targets: build_explain_analysis_dataframe(
                metadata, prefix, target=targets[0]
            ),
            lambda metadata, targets: build_explain_theoretical_dataframe(
                metadata, target=targets[0], arch=arch
            ),
        )

        display.render_explain_table(df, target=package, is_theoretical=is_theoretical)


if __name__ == "__main__":
    app()
