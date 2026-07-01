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
    mocker.patch("dark_matter.homebrew.get_brew_prefix", return_value="/opt/homebrew")
    mocker.patch(
        "dark_matter.homebrew.get_brew_metadata", return_value=mock_brew_metadata
    )

    # Bypass structural DataFrame computation and rendering
    mocker.patch("dark_matter.core.build_analysis_dataframe")
    mocker.patch("dark_matter.display.render_bloat_table")

    result = runner.invoke(app, ["analyze", "--top", "5"])
    assert result.exit_code == 0
    assert "Initializing Dark Matter pipeline" in result.stdout
