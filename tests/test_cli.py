import json
import re

import pandas as pd
import pytest
from typer.testing import CliRunner

from dark_matter_cli import PackageNotFoundError, __version__
from dark_matter_cli.cli import app

runner = CliRunner()


def test_version_flag():
    """Verify eager termination and correct version output."""
    result = runner.invoke(app, ["--version"])
    assert result.exit_code == 0
    assert __version__ in result.stdout


def test_analyze_command(mocker, mock_brew_metadata):
    """Verify successful execution of the primary CLI pipeline."""
    # Patch the references inside the cli module
    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(
        "dark_matter_cli.cli.get_brew_metadata", return_value=mock_brew_metadata
    )

    # Bypass structural DataFrame computation and rendering
    mocker.patch("dark_matter_cli.cli.build_analysis_dataframe")
    mocker.patch("dark_matter_cli.display.render_bloat_table")

    result = runner.invoke(app, ["analyze", "--top", "5"])
    assert result.exit_code == 0


@pytest.mark.parametrize(
    ("command", "builder", "source"),
    [
        ("analyze", "build_analysis_dataframe", "get_brew_metadata"),
        ("leaderboard", "build_theoretical_dataframe", "get_theoretical_catalog"),
    ],
)
@pytest.mark.parametrize(("flags", "roots_only"), [([], True), (["--all"], False)])
def test_all_flag_lists_dependencies(
    mocker, mock_brew_metadata, command, builder, source, flags, roots_only
):
    """Verify --all turns off the roots-only view."""
    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(f"dark_matter_cli.cli.{source}", return_value=mock_brew_metadata)
    mocker.patch(f"dark_matter_cli.cli.{builder}")
    render = mocker.patch("dark_matter_cli.display.render_bloat_table")

    result = runner.invoke(app, [command, *flags])
    assert result.exit_code == 0
    assert render.call_args.kwargs["roots_only"] is roots_only


def test_handle_pipeline_errors_catches_runtime_error():
    """Verify that handle_pipeline_errors gracefully catches and exits with code 1."""
    import typer
    from rich.console import Console

    from dark_matter_cli.cli import handle_pipeline_errors

    c = Console()
    with pytest.raises(typer.Exit) as exc, handle_pipeline_errors(c):
        raise RuntimeError("Simulated failure")
    assert exc.value.exit_code == 1


def test_leaderboard_command(mocker, mock_brew_metadata):
    """Verify leaderboard executes successfully and builds the theoretical df."""
    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")

    mocker.patch(
        "dark_matter_cli.cli.get_theoretical_catalog", return_value=mock_brew_metadata
    )
    mocker.patch("dark_matter_cli.cli.build_theoretical_dataframe")
    mocker.patch("dark_matter_cli.display.render_bloat_table")

    result = runner.invoke(app, ["leaderboard", "--arch", "arm64_tahoe"])
    assert result.exit_code == 0


def test_inspect_command_installed(mocker, mock_brew_metadata):
    """Verify targeted physical inspection paths."""
    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(
        "dark_matter_cli.cli.get_brew_metadata", return_value=mock_brew_metadata
    )
    mocker.patch("dark_matter_cli.cli.build_targeted_analysis_dataframe")
    mocker.patch("dark_matter_cli.display.render_bloat_table")

    result = runner.invoke(app, ["inspect", "pkg-a", "--source", "installed"])
    assert result.exit_code == 0


def test_compare_missing_packages_declined(mocker, mock_brew_metadata):
    """Verify declining the fallback with nothing installed exits non-zero."""
    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(
        "dark_matter_cli.cli.get_brew_metadata", return_value=mock_brew_metadata
    )
    catalog = mocker.patch("dark_matter_cli.cli.get_theoretical_catalog")

    result = runner.invoke(app, ["compare", "fake-1", "fake-2"], input="n\n")
    assert result.exit_code == 1
    assert "'fake-1', 'fake-2' are not installed" in result.stdout
    catalog.assert_not_called()


def test_compare_declined_keeps_installed_subset(mocker, mock_brew_metadata):
    """Verify declining the fallback still compares the installed packages."""
    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(
        "dark_matter_cli.cli.get_brew_metadata", return_value=mock_brew_metadata
    )
    build = mocker.patch(
        "dark_matter_cli.cli.build_compare_analysis_dataframe",
        side_effect=[PackageNotFoundError(["fake"], "local installation"), "df"],
    )
    render = mocker.patch("dark_matter_cli.display.render_bloat_table")

    result = runner.invoke(app, ["compare", "pkg-a", "fake"], input="n\n")
    assert result.exit_code == 0
    assert build.call_args.kwargs["targets"] == ["pkg-a"]
    assert render.call_args.kwargs["is_theoretical"] is False


def test_inspect_falls_back_to_catalog(mocker, mock_brew_metadata):
    """Verify inspect offers the catalog for an uninstalled package."""
    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(
        "dark_matter_cli.cli.get_brew_metadata", return_value=mock_brew_metadata
    )
    mocker.patch(
        "dark_matter_cli.cli.get_theoretical_catalog", return_value=mock_brew_metadata
    )
    theoretical = mocker.patch(
        "dark_matter_cli.cli.build_targeted_theoretical_dataframe"
    )
    render = mocker.patch("dark_matter_cli.display.render_bloat_table")

    result = runner.invoke(app, ["inspect", "not-installed"], input="y\n")
    assert result.exit_code == 0
    assert theoretical.call_args.kwargs["target"] == "not-installed"
    assert render.call_args.kwargs["is_theoretical"] is True


def test_compare_deduplicates_packages(mocker, mock_brew_metadata):
    """Verify repeated package names are compared once."""
    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(
        "dark_matter_cli.cli.get_brew_metadata", return_value=mock_brew_metadata
    )
    build = mocker.patch("dark_matter_cli.cli.build_compare_analysis_dataframe")
    mocker.patch("dark_matter_cli.display.render_bloat_table")

    runner.invoke(app, ["compare", "pkg-a", "pkg-a", "pkg-b"])
    assert build.call_args.kwargs["targets"] == ["pkg-a", "pkg-b"]


def test_explain_declined_fallback_exits(mocker, mock_brew_metadata):
    """Verify declining explain's fallback exits non-zero."""
    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(
        "dark_matter_cli.cli.get_brew_metadata", return_value=mock_brew_metadata
    )

    result = runner.invoke(app, ["explain", "not-installed"], input="n\n")
    assert result.exit_code == 1


def test_verbose_enables_debug_logging(mocker, mock_brew_metadata):
    """Verify --verbose turns on debug logging for the package logger."""
    import logging

    from rich.logging import RichHandler

    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(
        "dark_matter_cli.cli.get_brew_metadata", return_value=mock_brew_metadata
    )
    mocker.patch("dark_matter_cli.cli.build_analysis_dataframe")
    mocker.patch("dark_matter_cli.display.render_bloat_table")
    pkg_logger = logging.getLogger("dark_matter_cli")

    runner.invoke(app, ["--verbose", "analyze"])
    assert pkg_logger.level == logging.DEBUG

    runner.invoke(app, ["analyze"])
    assert pkg_logger.level == logging.WARNING
    rich_handlers = [h for h in pkg_logger.handlers if isinstance(h, RichHandler)]
    assert len(rich_handlers) == 1


def test_export_command_json(mocker, mock_brew_metadata):
    """Verify the export command outputs accurate records to stdout streams."""
    import pandas as pd

    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(
        "dark_matter_cli.cli.get_brew_metadata", return_value=mock_brew_metadata
    )

    mock_df = pd.DataFrame([{"Package": "pkg-a", "Bloat_Ratio": 1.0}])
    mocker.patch("dark_matter_cli.cli.build_analysis_dataframe", return_value=mock_df)

    result = runner.invoke(app, ["export", "--format", "json"])
    assert result.exit_code == 0
    exported_data = json.loads(result.stdout)
    assert exported_data[0]["Package"] == "pkg-a"


def test_explain_command_with_theoretical_fallback(mocker, mock_brew_metadata):
    """Verify that an uninstalled package explain triggers the interactive API fallback."""
    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    # First call for 'installed' fails because pkg-not-here isn't in graph
    mocker.patch(
        "dark_matter_cli.cli.get_brew_metadata", return_value=mock_brew_metadata
    )
    mocker.patch(
        "dark_matter_cli.cli.get_theoretical_catalog", return_value=mock_brew_metadata
    )
    mocker.patch("dark_matter_cli.cli.build_explain_theoretical_dataframe")
    mocker.patch("dark_matter_cli.display.render_explain_table")

    # Simulate hitting 'y' or Enter to the interactive fallback prompt
    result = runner.invoke(
        app, ["explain", "pkg-not-here", "--source", "installed"], input="y\n"
    )
    assert result.exit_code == 0
    assert "Fall back to the theoretical catalog?" in result.stdout


def test_compare_short_sort_flag(mocker, mock_brew_metadata):
    """Verify -s maps to --sort (not --source) on compare."""
    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(
        "dark_matter_cli.cli.get_brew_metadata", return_value=mock_brew_metadata
    )
    mocker.patch("dark_matter_cli.cli.build_compare_analysis_dataframe")
    render = mocker.patch("dark_matter_cli.display.render_bloat_table")

    result = runner.invoke(app, ["compare", "pkg-a", "pkg-b", "-s", "core"])
    assert result.exit_code == 0
    assert render.call_args.kwargs["sort_by"] == "core"


def test_invalid_sort_key_rejected():
    """Verify unknown sort metrics fail fast instead of silently falling back."""
    result = runner.invoke(app, ["analyze", "--sort", "ratoi"])
    assert result.exit_code == 2


def test_non_positive_top_rejected():
    """Verify --top rejects values below 1."""
    result = runner.invoke(app, ["analyze", "--top", "0"])
    assert result.exit_code == 2


def test_missing_brew_reports_pipeline_error(mocker):
    """Verify a missing brew executable surfaces as a clean pipeline error."""
    mocker.patch("subprocess.run", side_effect=FileNotFoundError("brew"))

    result = runner.invoke(app, ["analyze"])
    assert result.exit_code == 1
    assert "not found on PATH" in result.stdout


def test_catalog_source_passes_arch(mocker, mock_brew_metadata):
    """Verify --arch selects which cached catalog is loaded."""
    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    catalog = mocker.patch(
        "dark_matter_cli.cli.get_theoretical_catalog", return_value=mock_brew_metadata
    )
    mocker.patch("dark_matter_cli.cli.build_targeted_theoretical_dataframe")
    mocker.patch("dark_matter_cli.display.render_bloat_table")

    result = runner.invoke(
        app, ["inspect", "pkg-a", "--source", "catalog", "--arch", "x86_64_linux"]
    )
    assert result.exit_code == 0
    catalog.assert_called_once_with(tag="x86_64_linux")


def test_export_catalog_csv(mocker, mock_brew_metadata):
    """Verify catalog export builds the theoretical frame and writes CSV."""
    import pandas as pd

    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(
        "dark_matter_cli.cli.get_theoretical_catalog", return_value=mock_brew_metadata
    )
    mocker.patch(
        "dark_matter_cli.cli.build_theoretical_dataframe",
        return_value=pd.DataFrame([{"Package": "pkg-a", "Bloat_Ratio": 2.0}]),
    )

    result = runner.invoke(app, ["export", "--source", "catalog"])
    assert result.exit_code == 0
    assert result.stdout.splitlines() == ["Package,Bloat_Ratio", "pkg-a,2.0"]


@pytest.fixture
def installed_context(mocker, mock_brew_metadata):
    """Serve the mock installation to installed-mode commands."""
    mocker.patch("dark_matter_cli.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(
        "dark_matter_cli.cli.get_brew_metadata", return_value=mock_brew_metadata
    )


def test_reclaim_command(mocker, installed_context):
    """Verify reclaim deduplicates targets and renders the result."""
    from dark_matter_cli.core import Reclaim

    result_obj = Reclaim(freed=pd.DataFrame(), blocked={"pkg-d": ["pkg-b"]})
    build = mocker.patch(
        "dark_matter_cli.cli.build_reclaim_dataframe", return_value=result_obj
    )
    render = mocker.patch("dark_matter_cli.display.render_reclaim")

    result = runner.invoke(app, ["reclaim", "pkg-a", "pkg-d", "pkg-a"])

    assert result.exit_code == 0
    assert build.call_args.args[2] == ["pkg-a", "pkg-d"]
    render.assert_called_once_with(
        result_obj.freed, {"pkg-d": ["pkg-b"]}, ["pkg-a", "pkg-d"]
    )


def test_reclaim_unknown_package_errors(installed_context):
    """Verify reclaim exits non-zero for packages that aren't installed."""
    result = runner.invoke(app, ["reclaim", "nope"])
    assert result.exit_code == 1
    assert "not found in the local installation" in result.stdout


@pytest.mark.parametrize(
    ("command", "builder", "renderer"),
    [
        ("orphans", "build_orphans_dataframe", "render_orphans"),
        ("cleanup", "build_cleanup_dataframe", "render_cleanup"),
    ],
)
def test_installed_report_commands(
    mocker, installed_context, command, builder, renderer
):
    """Verify orphans and cleanup build from the installation and render."""
    build = mocker.patch(f"dark_matter_cli.cli.{builder}", return_value="df")
    render = mocker.patch(f"dark_matter_cli.display.{renderer}")

    result = runner.invoke(app, [command])

    assert result.exit_code == 0
    assert build.call_args.args[1] == "/opt/homebrew"
    render.assert_called_once_with("df")


def test_explain_tree_flag(mocker, installed_context):
    """Verify --tree switches explain to the tree renderer."""
    mocker.patch("dark_matter_cli.cli.build_explain_analysis_dataframe")
    tree = mocker.patch("dark_matter_cli.display.render_explain_tree")
    table = mocker.patch("dark_matter_cli.display.render_explain_table")

    result = runner.invoke(app, ["explain", "pkg-a", "--tree"])

    assert result.exit_code == 0
    tree.assert_called_once()
    table.assert_not_called()


def test_diff_command(mocker, tmp_path):
    """Verify diff reads CSV and JSON exports and reports totals."""
    old = tmp_path / "old.csv"
    old.write_text("Package,Core_Bytes,Weighted_Bytes\na,10,20\n")
    new = tmp_path / "new.json"
    new.write_text(
        json.dumps([{"Package": "a", "Core_Bytes": 30, "Weighted_Bytes": 40}])
    )
    render = mocker.patch("dark_matter_cli.display.render_diff")

    result = runner.invoke(app, ["diff", str(old), str(new), "-n", "3"])

    assert result.exit_code == 0
    df = render.call_args.args[0]
    assert list(df["Core_Delta"]) == [20]
    assert render.call_args.kwargs == {"top_n": 3, "old_total": 10, "new_total": 30}


def test_diff_unreadable_snapshot(tmp_path):
    """Verify a missing or malformed snapshot is a clean pipeline error."""
    bad = tmp_path / "bad.json"
    bad.write_text("not json")

    result = runner.invoke(app, ["diff", str(bad), str(tmp_path / "missing.csv")])

    assert result.exit_code == 1
    assert "Could not read snapshot" in result.stdout


def test_help_offers_completion_install():
    """Verify shell completion can be installed from the CLI."""
    result = runner.invoke(app, ["--help"])
    # Rich styles help output when it detects CI, splitting the option name.
    plain = re.sub(r"\x1b\[[0-9;]*m", "", result.stdout)
    assert "--install-completion" in plain


def test_complete_installed_lists_formulae_and_casks(monkeypatch, tmp_path):
    """Verify completion offers installed formulae and casks by prefix."""
    from dark_matter_cli.cli import complete_installed

    for path in ("Cellar/wget", "Cellar/wavpack", "Cellar/jq", "Caskroom/warp"):
        (tmp_path / path).mkdir(parents=True)
    monkeypatch.setenv("HOMEBREW_PREFIX", str(tmp_path))

    assert complete_installed("wa") == ["warp", "wavpack"]
    assert complete_installed("") == ["jq", "warp", "wavpack", "wget"]


def test_complete_installed_without_brew(mocker, monkeypatch):
    """Verify completion offers nothing, rather than failing, without Homebrew."""
    from dark_matter_cli.cli import complete_installed

    monkeypatch.delenv("HOMEBREW_PREFIX", raising=False)
    mocker.patch(
        "dark_matter_cli.cli.get_brew_prefix", side_effect=RuntimeError("no brew")
    )

    assert complete_installed("w") == []
