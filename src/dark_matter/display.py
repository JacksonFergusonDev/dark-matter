import pandas as pd
from rich.console import Console
from rich.table import Table

console = Console()


def format_bytes(size: float) -> str:
    """Dynamically format byte sizes into human-readable strings."""
    if size < 1024:
        return f"{size:.0f} B"
    if size < 1024**2:
        return f"{size / 1024:.1f} KB"
    if size < 999 * (1024**2):
        return f"{size / (1024**2):.1f} MB"
    return f"{size / (1024**3):.2f} GB"


def render_bloat_table(
    df: pd.DataFrame,
    sort_by: str = "ratio",
    top_n: int = 20,
    fractional: bool = True,
    is_theoretical: bool = False,
) -> None:
    """Render the mathematical analysis as a formatted terminal table.

    Args:
        df: The fully processed pandas DataFrame containing bloat metrics.
        sort_by: The column criteria utilized for descending sort operations.
        top_n: The maximum number of rows to print to standard output.
        fractional: Boolean flag to display the Fractional Attribution Model data.
        is_theoretical: Boolean flag to indicate if the data is from theoretical analysis.
    """
    if df.empty or "Is_Leaf" not in df.columns:
        console.print("[bold red]No package data available to render.[/bold red]")
        return

    # Filter explicitly for root invocations (leaves)
    df = df[df["Is_Leaf"]]

    # Map the CLI flag to the corresponding DataFrame column
    sort_map = {
        "ratio": "Bloat_Ratio",
        "core": "Core_Bytes",
        "recursive": "Weighted_Bytes" if fractional else "Standard_Bytes",
    }
    sort_col = sort_map.get(sort_by, "Bloat_Ratio")

    df = df.sort_values(by=sort_col, ascending=False).head(top_n)

    table = Table(
        title="[bold]Dark Matter: Homebrew Bloat Analysis[/bold]",
        show_header=True,
        header_style="bold cyan",
    )

    table.add_column("Package", style="white", no_wrap=True)

    # Adjust headers based on the measurement type
    core_label = "Archive Size" if is_theoretical else "Core Size"
    table.add_column(core_label, justify="right", style="dim")

    if fractional:
        rec_label = "Theoretical Rec. Size" if is_theoretical else "Weighted Rec. Size"
        table.add_column(rec_label, justify="right", style="magenta")
    else:
        rec_label = "Theoretical Std. Size" if is_theoretical else "Standard Rec. Size"
        table.add_column(rec_label, justify="right", style="magenta")

    table.add_column("Bloat Ratio", justify="right")
    table.add_column("Deps", justify="right", style="dim")

    for _, row in df.iterrows():
        # Pass raw byte values to the dynamic formatter
        core_bytes = row["Core_Bytes"]
        rec_bytes = row["Weighted_Bytes"] if fractional else row["Standard_Bytes"]

        ratio = row["Bloat_Ratio"]
        deps = int(row["Dep_Count"])

        # Color code the bloat severity
        if ratio >= 10.0:
            ratio_str = f"[bold red]{ratio:.1f}x[/bold red]"
        elif ratio >= 3.0:
            ratio_str = f"[bold yellow]{ratio:.1f}x[/bold yellow]"
        else:
            ratio_str = f"[green]{ratio:.1f}x[/green]"

        table.add_row(
            str(row["Package"]),
            format_bytes(core_bytes),
            format_bytes(rec_bytes),
            ratio_str,
            str(deps),
        )

    console.print(table)
