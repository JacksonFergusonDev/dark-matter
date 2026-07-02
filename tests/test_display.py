import pandas as pd

from dark_matter import display


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
    mock_console_print = mocker.patch("dark_matter.display.console.print")
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
