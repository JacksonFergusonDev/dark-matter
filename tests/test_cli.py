import json

import pytest
from typer.testing import CliRunner

from dark_matter import __version__
from dark_matter.cli import app

runner = CliRunner()


def test_version_flag():
    """Verify eager termination and correct version output."""
    result = runner.invoke(app, ["--version"])
    assert result.exit_code == 0
    assert __version__ in result.stdout


def test_analyze_command(mocker, mock_brew_metadata):
    """Verify successful execution of the primary CLI pipeline."""
    # Patch the references inside the cli module
    mocker.patch("dark_matter.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch("dark_matter.cli.get_brew_metadata", return_value=mock_brew_metadata)

    # Bypass structural DataFrame computation and rendering
    mocker.patch("dark_matter.cli.build_analysis_dataframe")
    mocker.patch("dark_matter.display.render_bloat_table")

    result = runner.invoke(app, ["analyze", "--top", "5"])
    assert result.exit_code == 0


def test_handle_pipeline_errors_catches_runtime_error():
    """Verify that handle_pipeline_errors gracefully catches and exits with code 1."""
    import typer
    from rich.console import Console

    from dark_matter.cli import handle_pipeline_errors

    c = Console()
    with pytest.raises(typer.Exit) as exc, handle_pipeline_errors(c):
        raise RuntimeError("Simulated failure")
    assert exc.value.exit_code == 1


def test_leaderboard_command(mocker, mock_brew_metadata):
    """Verify leaderboard executes successfully and builds the theoretical df."""
    mocker.patch("dark_matter.cli.get_brew_prefix", return_value="/opt/homebrew")

    mocker.patch(
        "dark_matter.cli.get_theoretical_catalog", return_value=mock_brew_metadata
    )
    mocker.patch("dark_matter.cli.build_theoretical_dataframe")
    mocker.patch("dark_matter.display.render_bloat_table")

    result = runner.invoke(app, ["leaderboard", "--arch", "arm64_tahoe"])
    assert result.exit_code == 0


def test_inspect_command_installed(mocker, mock_brew_metadata):
    """Verify targeted physical inspection paths."""
    mocker.patch("dark_matter.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch("dark_matter.cli.get_brew_metadata", return_value=mock_brew_metadata)
    mocker.patch("dark_matter.cli.build_targeted_analysis_dataframe")
    mocker.patch("dark_matter.display.render_bloat_table")

    result = runner.invoke(app, ["inspect", "pkg-a", "--source", "installed"])
    assert result.exit_code == 0


def test_compare_command_missing_packages(mocker, mock_brew_metadata):
    """Verify compare handles non-existent package lists via pipeline error capture."""
    mocker.patch("dark_matter.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch("dark_matter.cli.get_brew_metadata", return_value=mock_brew_metadata)

    result = runner.invoke(app, ["compare", "fake-pkg-1", "fake-pkg-2"])
    assert result.exit_code == 1
    assert "Pipeline Error" in result.stdout


def test_export_command_json(mocker, mock_brew_metadata):
    """Verify the export command outputs accurate records to stdout streams."""
    import pandas as pd

    mocker.patch("dark_matter.cli.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch("dark_matter.cli.get_brew_metadata", return_value=mock_brew_metadata)

    mock_df = pd.DataFrame([{"Package": "pkg-a", "Bloat_Ratio": 1.0}])
    mocker.patch("dark_matter.cli.build_analysis_dataframe", return_value=mock_df)

    result = runner.invoke(app, ["export", "--format", "json"])
    assert result.exit_code == 0
    exported_data = json.loads(result.stdout)
    assert exported_data[0]["Package"] == "pkg-a"


def test_explain_command_with_theoretical_fallback(mocker, mock_brew_metadata):
    """Verify that an uninstalled package explain triggers the interactive API fallback."""
    mocker.patch("dark_matter.cli.get_brew_prefix", return_value="/opt/homebrew")
    # First call for 'installed' fails because pkg-not-here isn't in graph
    mocker.patch("dark_matter.cli.get_brew_metadata", return_value=mock_brew_metadata)
    mocker.patch(
        "dark_matter.cli.get_theoretical_catalog", return_value=mock_brew_metadata
    )
    mocker.patch("dark_matter.cli.build_explain_theoretical_dataframe")
    mocker.patch("dark_matter.display.render_explain_table")

    # Simulate hitting 'y' or Enter to the interactive fallback prompt
    result = runner.invoke(
        app, ["explain", "pkg-not-here", "--source", "installed"], input="y\n"
    )
    assert result.exit_code == 0
    assert "Fall back to the theoretical catalog?" in result.stdout
