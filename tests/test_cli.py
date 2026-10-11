import json

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
