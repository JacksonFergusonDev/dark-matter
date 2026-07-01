import typer
from rich.console import Console

from dark_matter import __version__, core, display, homebrew

app = typer.Typer(
    help="Analyze dependency bloat and storage mass of macOS Homebrew installations.",
    add_completion=False,
)
console = Console()

state = {"verbose": False}


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
    state["verbose"] = verbose


@app.command()
def analyze(
    sort_by: str = typer.Option(
        "ratio", "--sort", "-s", help="Sorting metric: 'ratio', 'core', or 'recursive'."
    ),
    top_n: int = typer.Option(20, "--top", "-n", help="Number of packages to display."),
    fractional: bool = typer.Option(
        True,
        "--fractional/--standard",
        help="Utilize the fractional attribution model for shared dependencies.",
    ),
) -> None:
    """Execute the storage bloat analysis pipeline."""
    console.print("[bold cyan]Initializing Dark Matter pipeline...[/bold cyan]")

    try:
        prefix = homebrew.get_brew_prefix()
        console.print(f"Resolved Homebrew prefix: [green]{prefix}[/green]")

        with console.status("[yellow]Ingesting JSON metadata via brew API...[/yellow]"):
            metadata = homebrew.get_brew_metadata()
            formulae_count = len(metadata.get("formulae", []))
            casks_count = len(metadata.get("casks", []))

        console.print(
            f"Successfully parsed [bold]{formulae_count}[/bold] formulae and "
            f"[bold]{casks_count}[/bold] casks."
        )

        # 2. Replaced the pending message with the actual execution calls
        with console.status("[yellow]Computing fractional attribution DAG...[/yellow]"):
            df = core.build_analysis_dataframe(metadata, prefix)

        display.render_bloat_table(
            df, sort_by=sort_by, top_n=top_n, fractional=fractional
        )

    except RuntimeError as e:
        console.print(f"[bold red]Pipeline Error:[/bold red] {e}")
        raise typer.Exit(code=1) from e


@app.command()
def leaderboard(
    sort_by: str = typer.Option(
        "ratio", "--sort", "-s", help="Sorting metric: 'ratio', 'core', or 'recursive'."
    ),
    top_n: int = typer.Option(20, "--top", "-n", help="Number of packages to display."),
    arch: str = typer.Option(
        "arm64_tahoe", "--arch", "-a", help="Target architecture profile."
    ),
) -> None:
    """Evaluate the complete theoretical ecosystem leaderboard from local API caches."""
    console.print("[bold cyan]Accessing local ecosystem cache...[/bold cyan]")

    try:
        prefix = homebrew.get_brew_prefix()
        metadata = homebrew.get_theoretical_catalog(prefix)

        with console.status("[yellow]Processing massive theoretical DAG...[/yellow]"):
            df = core.build_theoretical_dataframe(metadata, arch=arch)

        display.render_bloat_table(
            df, sort_by=sort_by, top_n=top_n, fractional=True, is_theoretical=True
        )
    except RuntimeError as e:
        console.print(f"[bold red]Pipeline Error:[/bold red] {e}")
        raise typer.Exit(code=1) from e


@app.command()
def inspect(
    package: str = typer.Argument(..., help="The specific package to analyze."),
    arch: str = typer.Option(
        "arm64_tahoe", "--arch", "-a", help="Target architecture profile."
    ),
) -> None:
    """Evaluate the theoretical bloat of a single target package."""
    console.print(
        f"[bold cyan]Inspecting theoretical bloat for '{package}'...[/bold cyan]"
    )

    try:
        prefix = homebrew.get_brew_prefix()
        metadata = homebrew.get_theoretical_catalog(prefix)

        with console.status(
            f"[yellow]Computing fractional DAG for {package}...[/yellow]"
        ):
            try:
                df = core.build_targeted_theoretical_dataframe(
                    metadata, target=package, arch=arch
                )
            except ValueError as e:
                console.print(f"[bold red]Error:[/bold red] {e}")
                raise typer.Exit(code=1) from e

        display.render_bloat_table(
            df, sort_by="ratio", top_n=1, fractional=True, is_theoretical=True
        )

    except RuntimeError as e:
        console.print(f"[bold red]Pipeline Error:[/bold red] {e}")
        raise typer.Exit(code=1) from e


@app.command()
def compare(
    packages: list[str] = typer.Argument(..., help="The specific packages to compare."),
    sort_by: str = typer.Option(
        "ratio", "--sort", "-s", help="Sorting metric: 'ratio', 'core', or 'recursive'."
    ),
    arch: str = typer.Option(
        "arm64_tahoe", "--arch", "-a", help="Target architecture profile."
    ),
) -> None:
    """Evaluate and compare the theoretical bloat of multiple packages."""
    pkg_list_str = ", ".join(packages)
    console.print(
        f"[bold cyan]Comparing theoretical bloat for: {pkg_list_str}[/bold cyan]"
    )

    try:
        prefix = homebrew.get_brew_prefix()
        metadata = homebrew.get_theoretical_catalog(prefix)

        with console.status("[yellow]Computing fractional DAG union...[/yellow]"):
            try:
                df = core.build_compare_theoretical_dataframe(
                    metadata, targets=packages, arch=arch
                )
            except ValueError as e:
                console.print(f"[bold red]Error:[/bold red] {e}")
                raise typer.Exit(code=1) from e

        display.render_bloat_table(
            df,
            sort_by=sort_by,
            top_n=len(packages),
            fractional=True,
            is_theoretical=True,
        )

    except RuntimeError as e:
        console.print(f"[bold red]Pipeline Error:[/bold red] {e}")
        raise typer.Exit(code=1) from e


if __name__ == "__main__":
    app()
