import pandas as pd
from rich.console import Console
from rich.table import Table
from rich.tree import Tree

console = Console()


# Decimal units, as Finder and Disk Utility report sizes on macOS.
_BYTE_UNITS = (("KB", 1e3, 1), ("MB", 1e6, 1), ("GB", 1e9, 2), ("TB", 1e12, 2))


def format_bytes(size: float) -> str:
    """Format a byte count in decimal units (1 MB = 1,000,000 bytes).

    The unit is chosen after rounding, so 999,999 bytes reads "1.0 MB"
    rather than "1000.0 KB".
    """
    if round(size) < 1000:
        return f"{size:.0f} B"
    for unit, scale, digits in _BYTE_UNITS[:-1]:
        value = round(size / scale, digits)
        if value < 1000:
            return f"{value:.{digits}f} {unit}"
    unit, scale, digits = _BYTE_UNITS[-1]
    return f"{size / scale:.{digits}f} {unit}"


def format_delta(size: float) -> str:
    """Format a signed byte change, red for growth and green for shrinkage."""
    if size > 0:
        return f"[red]+{format_bytes(size)}[/red]"
    if size < 0:
        return f"[green]-{format_bytes(-size)}[/green]"
    return "[dim]0 B[/dim]"


def render_bloat_table(
    df: pd.DataFrame,
    sort_by: str = "ratio",
    top_n: int = 20,
    fractional: bool = True,
    is_theoretical: bool = False,
    roots_only: bool = True,
) -> None:
    """Render the mathematical analysis as a formatted terminal table.

    Args:
        df: The fully processed pandas DataFrame containing bloat metrics.
        sort_by: The column criteria utilized for descending sort operations.
        top_n: The maximum number of rows to print to standard output.
        fractional: Boolean flag to display the Fractional Attribution Model data.
        is_theoretical: Boolean flag to indicate if the data is from theoretical analysis.
        roots_only: Restrict rows to root packages (requested, or with nothing
            depending on them). Disable when rendering explicitly requested
            targets, or to list dependencies too.
    """
    if df.empty or "Is_Root" not in df.columns:
        console.print("[bold red]No package data available to render.[/bold red]")
        return

    # Only roots' weighted sizes add up to the total; hide intermediate deps.
    hidden = 0
    if roots_only:
        hidden = int((~df["Is_Root"]).sum())
        df = df[df["Is_Root"]]

    # Map the CLI flag to the corresponding DataFrame column
    sort_map = {
        "ratio": "Bloat_Ratio",
        "core": "Core_Bytes",
        "recursive": "Weighted_Bytes" if fractional else "Standard_Bytes",
    }
    sort_col = sort_map.get(sort_by, "Bloat_Ratio")

    df = df.sort_values(by=sort_col, ascending=False, na_position="last").head(top_n)

    table = Table(
        title="[bold]Dark Matter: Homebrew Bloat Analysis[/bold]",
        show_header=True,
        header_style="bold cyan",
    )
    if hidden:
        noun = "dependency" if hidden == 1 else "dependencies"
        table.caption = (
            f"{hidden} {noun} not shown: each one's size is shared among the "
            "packages that need it. List them with --all."
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

        # Color code the bloat severity; an empty core leaves the ratio undefined
        if pd.isna(ratio):
            ratio_str = "[dim]—[/dim]"
        elif ratio >= 10.0:
            ratio_str = f"[bold red]{ratio:.1f}x[/bold red]"
        elif ratio >= 3.0:
            ratio_str = f"[bold yellow]{ratio:.1f}x[/bold yellow]"
        else:
            ratio_str = f"[green]{ratio:.1f}x[/green]"

        # A dependency's weighted size overlaps the roots that need it.
        name = str(row["Package"])
        if not row["Is_Root"]:
            name += " [dim]· dependency[/dim]"

        table.add_row(
            name,
            format_bytes(core_bytes),
            format_bytes(rec_bytes),
            ratio_str,
            str(deps),
        )

    console.print(table)


def render_explain_table(
    df: pd.DataFrame, target: str, is_theoretical: bool = False
) -> None:
    """Render the dependency breakdown table for a specific package."""
    if df.empty:
        console.print(f"[green]'{target}' has no transitive dependencies.[/green]")
        return

    table = Table(
        title=f"[bold]Dependency Breakdown: {target}[/bold]",
        show_header=True,
        header_style="bold cyan",
    )

    size_col = "Archive_Bytes" if is_theoretical else "Core_Bytes"
    size_label = "Archive Size" if is_theoretical else "Core Size"

    table.add_column("Dependency", style="white", no_wrap=True)
    table.add_column(size_label, justify="right", style="dim")
    table.add_column("Shared By", justify="right", style="dim")
    table.add_column("Attributed Size", justify="right", style="magenta")

    for _, row in df.iterrows():
        # Requested packages are paid for by themselves, not their dependents.
        shared_by = (
            "[dim]requested[/dim]" if row["Is_Root"] else str(int(row["Shared_By"]))
        )
        table.add_row(
            str(row["Dependency"]),
            format_bytes(row[size_col]),
            shared_by,
            format_bytes(row["Attributed_Bytes"]),
        )

    console.print(table)


def render_explain_tree(
    df: pd.DataFrame, target: str, is_theoretical: bool = False
) -> None:
    """Render a target's dependencies as a tree with attributed sizes.

    A dependency reached by several paths is expanded the first time it
    appears and marked as shown above after that.
    """
    if df.empty:
        console.print(f"[green]'{target}' has no transitive dependencies.[/green]")
        return

    size_col = "Archive_Bytes" if is_theoretical else "Core_Bytes"
    rows = {str(r["Dependency"]): r for _, r in df.iterrows()}
    children: dict[str, list[str]] = {}
    for name, row in rows.items():
        for parent in row["Required_By"]:
            children.setdefault(parent, []).append(name)
    for kids in children.values():
        kids.sort(key=lambda n: (-rows[n]["Attributed_Bytes"], n))

    total = df["Attributed_Bytes"].sum()
    tree = Tree(f"[bold]{target}[/bold] [dim]· {format_bytes(total)} attributed[/dim]")
    seen: set[str] = set()

    def label(name: str) -> str:
        row = rows[name]
        size = format_bytes(row[size_col])
        if row["Is_Root"]:
            return f"{name} [dim]{size} · requested[/dim]"
        share = format_bytes(row["Attributed_Bytes"])
        shared = int(row["Shared_By"])
        detail = f" of {size}, shared by {shared}" if shared > 1 else ""
        return f"{name} [magenta]{share}[/magenta][dim]{detail}[/dim]"

    def add(branch: Tree, name: str) -> None:
        if name in seen:
            branch.add(f"[dim]{name} (shown above)[/dim]")
            return
        seen.add(name)
        node = branch.add(label(name))
        for child in children.get(name, []):
            add(node, child)

    for child in children.get(target, []):
        add(tree, child)

    console.print(tree)


def render_reclaim(
    freed: pd.DataFrame, blocked: dict[str, list[str]], targets: list[str]
) -> None:
    """Render what uninstalling the targets would free."""
    for target, dependents in blocked.items():
        console.print(
            f"[bold yellow]'{target}' can't be uninstalled:[/bold yellow] "
            f"required by {', '.join(dependents)}."
        )

    if freed.empty:
        console.print("[dim]Nothing would be freed.[/dim]")
        return

    table = Table(
        title=f"[bold]Reclaimable by removing {', '.join(targets)}[/bold]",
        show_header=True,
        header_style="bold cyan",
    )
    table.add_column("Package", style="white", no_wrap=True)
    table.add_column("Size", justify="right", style="magenta")
    table.add_column("Removed As", style="dim")

    for _, row in freed.iterrows():
        table.add_row(
            str(row["Package"]),
            format_bytes(row["Core_Bytes"]),
            "requested" if row["Is_Target"] else "dependency",
        )

    console.print(table)
    console.print(
        f"[bold]Total:[/bold] {format_bytes(freed['Core_Bytes'].sum())} "
        f"across {len(freed)} packages "
        "[dim](brew uninstall, then brew autoremove)[/dim]"
    )


def render_orphans(df: pd.DataFrame) -> None:
    """Render dependencies nothing requested needs any more."""
    if df.empty:
        console.print("[green]No orphaned dependencies.[/green]")
        return

    table = Table(
        title="[bold]Orphaned Dependencies[/bold]",
        show_header=True,
        header_style="bold cyan",
    )
    table.add_column("Package", style="white", no_wrap=True)
    table.add_column("Size", justify="right", style="magenta")
    table.add_column("Needed By", style="dim")

    for _, row in df.iterrows():
        needed_by = ", ".join(row["Required_By"]) or "—"
        table.add_row(str(row["Package"]), format_bytes(row["Core_Bytes"]), needed_by)

    console.print(table)
    console.print(
        f"[bold]Total:[/bold] {format_bytes(df['Core_Bytes'].sum())} "
        "[dim](reclaim with brew autoremove)[/dim]"
    )


def render_cleanup(df: pd.DataFrame) -> None:
    """Render older kegs left behind in the Cellar."""
    if df.empty:
        console.print("[green]No old versions in the Cellar.[/green]")
        return

    table = Table(
        title="[bold]Old Versions in the Cellar[/bold]",
        show_header=True,
        header_style="bold cyan",
    )
    table.add_column("Package", style="white", no_wrap=True)
    table.add_column("Version", style="dim")
    table.add_column("Size", justify="right", style="magenta")

    for _, row in df.iterrows():
        table.add_row(
            str(row["Package"]), str(row["Version"]), format_bytes(row["Bytes"])
        )

    console.print(table)
    console.print(
        f"[bold]Total:[/bold] {format_bytes(df['Bytes'].sum())} "
        "[dim](reclaim with brew cleanup)[/dim]"
    )


def render_diff(
    df: pd.DataFrame, top_n: int, old_total: float, new_total: float
) -> None:
    """Render the packages that changed between two snapshots."""
    console.print(
        f"[bold]Total core size:[/bold] {format_bytes(old_total)} → "
        f"{format_bytes(new_total)} ({format_delta(new_total - old_total)})"
    )
    if df.empty:
        console.print("[dim]No packages changed.[/dim]")
        return

    table = Table(
        title="[bold]Snapshot Diff[/bold]",
        show_header=True,
        header_style="bold cyan",
    )
    table.add_column("Package", style="white", no_wrap=True)
    table.add_column("Status", style="dim")
    table.add_column("Core Size", justify="right", style="dim")
    table.add_column("Core Δ", justify="right")
    table.add_column("Weighted Δ", justify="right")

    for _, row in df.head(top_n).iterrows():
        table.add_row(
            str(row["Package"]),
            str(row["Status"]),
            format_bytes(row["New_Core"] or row["Old_Core"]),
            format_delta(row["Core_Delta"]),
            format_delta(row["Weighted_Delta"]),
        )

    console.print(table)
    if len(df) > top_n:
        console.print(f"[dim]…and {len(df) - top_n} more.[/dim]")
