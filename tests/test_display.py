import pandas as pd
import pytest

from dark_matter_cli import display


def test_format_bytes():
    """Verify scale shifting across byte magnitudes."""
    assert display.format_bytes(500) == "500 B"
    assert display.format_bytes(1024) == "1.0 KB"
    assert display.format_bytes(1024**2 * 2.5) == "2.5 MB"
    assert display.format_bytes(1024**3 * 4.123) == "4.12 GB"


def test_render_bloat_table_empty(capsys):
    """Verify empty DataFrame logging limits structural rendering execution gracefully."""
    df = pd.DataFrame()
    display.render_bloat_table(df)
    captured = capsys.readouterr()
    assert "No package data available to render." in captured.out


def test_render_bloat_table_with_severe_ratios(mocker):
    """Exercise layout components and ratio string styling ranges (severe vs clean)."""
    mock_console_print = mocker.patch("dark_matter_cli.display.console.print")
    df = pd.DataFrame(
        [
            {
                "Package": "bloated-pkg",
                "Core_Bytes": 100,
                "Weighted_Bytes": 1500,
                "Standard_Bytes": 1500,
                "Bloat_Ratio": 15.0,
                "Dep_Count": 5,
                "Is_Leaf": True,
            },
            {
                "Package": "clean-pkg",
                "Core_Bytes": 100,
                "Weighted_Bytes": 110,
                "Standard_Bytes": 110,
                "Bloat_Ratio": 1.1,
                "Dep_Count": 0,
                "Is_Leaf": True,
            },
        ]
    )
    display.render_bloat_table(df, sort_by="ratio")
    assert mock_console_print.called


def test_render_explain_table_empty(capsys):
    """Verify string notices for targets with clean terminal leaf structures (0 deps)."""
    df = pd.DataFrame()
    display.render_explain_table(df, target="leaf-node")
    captured = capsys.readouterr()
    assert "has no transitive dependencies" in captured.out


def _leaf_frame() -> pd.DataFrame:
    rows = [
        {
            "Package": name,
            "Is_Leaf": leaf,
            "Core_Bytes": 10,
            "Weighted_Bytes": 20,
            "Standard_Bytes": 20,
            "Bloat_Ratio": 2.0,
            "Dep_Count": 1,
        }
        for name, leaf in [("top-level", True), ("shared-lib", False)]
    ]
    return pd.DataFrame(rows)


def test_render_bloat_table_filters_non_leaves_by_default(mocker):
    """Verify the default leaderboard view only shows leaf packages."""
    add_row = mocker.patch("rich.table.Table.add_row")
    mocker.patch("dark_matter_cli.display.console.print")

    display.render_bloat_table(_leaf_frame())
    assert [c.args[0] for c in add_row.call_args_list] == ["top-level"]


def test_render_bloat_table_keeps_requested_targets(mocker):
    """Verify explicitly requested non-leaf targets are not dropped."""
    add_row = mocker.patch("rich.table.Table.add_row")
    mocker.patch("dark_matter_cli.display.console.print")

    display.render_bloat_table(_leaf_frame(), leaves_only=False)
    assert sorted(c.args[0] for c in add_row.call_args_list) == [
        "shared-lib",
        "top-level",
    ]


def test_render_bloat_table_standard_mode_and_severity(mocker):
    """Verify --standard sizes and every ratio severity band are rendered."""
    add_row = mocker.patch("rich.table.Table.add_row")
    mocker.patch("dark_matter_cli.display.console.print")
    df = pd.DataFrame(
        [
            {
                "Package": name,
                "Is_Leaf": True,
                "Core_Bytes": 1024,
                "Dep_Count": 1,
                "Weighted_Bytes": 1,
                "Standard_Bytes": 2048,
                "Bloat_Ratio": ratio,
            }
            for name, ratio in [("red", 12.0), ("yellow", 4.0), ("green", 1.5)]
        ]
    )

    display.render_bloat_table(df, fractional=False, is_theoretical=True)

    rows = {c.args[0]: c.args for c in add_row.call_args_list}
    assert rows["red"][2] == "2.0 KB"
    assert "bold red" in rows["red"][3]
    assert "bold yellow" in rows["yellow"][3]
    assert "green" in rows["green"][3]


@pytest.mark.parametrize(
    ("is_theoretical", "size_col"), [(True, "Archive_Bytes"), (False, "Core_Bytes")]
)
def test_render_explain_table(mocker, is_theoretical, size_col):
    """Verify explain rows render the right size column for each mode."""
    add_row = mocker.patch("rich.table.Table.add_row")
    mocker.patch("dark_matter_cli.display.console.print")
    df = pd.DataFrame(
        [
            {
                "Dependency": "openssl",
                size_col: 2048,
                "Shared_By": 4,
                "Attributed_Bytes": 512,
            }
        ]
    )

    display.render_explain_table(df, target="curl", is_theoretical=is_theoretical)
    add_row.assert_called_once_with("openssl", "2.0 KB", "4", "512 B")
