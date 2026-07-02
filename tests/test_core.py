from pathlib import Path

import pandas as pd
import pytest

from dark_matter import core


def test_get_all_dependencies():
    """Verify recursive traversal extracts all deeply nested dependencies."""
    graph = {
        "pkg-a": ["pkg-b", "pkg-c"],
        "pkg-b": ["pkg-d"],
        "pkg-c": ["pkg-d"],
        "pkg-d": [],
    }

    deps_a = core.get_all_dependencies("pkg-a", graph)
    assert deps_a == {"pkg-b", "pkg-c", "pkg-d"}

    deps_d = core.get_all_dependencies("pkg-d", graph)
    assert deps_d == set()


def test_build_analysis_dataframe(mocker, mock_brew_metadata, mock_brew_sizes):
    """Verify the mathematical integrity of the Fractional Attribution Model."""

    # Mock os.scandir wrapper to return predefined sizes
    def mock_get_directory_size(path: Path) -> int:
        pkg_name = path.name
        return int(mock_brew_sizes.get(pkg_name, 0))

    mocker.patch(
        "dark_matter.homebrew.get_directory_size", side_effect=mock_get_directory_size
    )

    # Ensure all mock directories appear to "exist" for the core logic
    mocker.patch("pathlib.Path.exists", return_value=True)

    df = core.build_analysis_dataframe(mock_brew_metadata, Path("/mock/prefix"))

    assert isinstance(df, pd.DataFrame)
    assert len(df) == 4

    row_a = df[df["Package"] == "pkg-a"].iloc[0]

    # Standard Recursive Size = Core(A) + Core(B) + Core(C) + Core(D)
    # 100 + 200 + 300 + 600 = 1200
    assert row_a["Standard_Bytes"] == 1200

    # Transitive in-degree map for dependencies in this DAG:
    # pkg-b: 1 (pkg-a)
    # pkg-c: 1 (pkg-a)
    # pkg-d: 3 (pkg-a, pkg-b, pkg-c)
    #
    # Weighted Recursive Size = 100 + (200/1) + (300/1) + (600/3) = 800
    assert row_a["Weighted_Bytes"] == 800

    # Bloat Ratio = Weighted / Core = 800 / 100 = 8.0
    assert row_a["Bloat_Ratio"] == 8.0
    assert row_a["Is_Leaf"]


def test_build_theoretical_dataframe(mocker, mock_brew_metadata):
    """Verify theoretical DAG execution uses resolved OCI blob sizes."""
    mocker.patch("dark_matter.homebrew.load_bottle_size_cache", return_value={})
    mocker.patch(
        "dark_matter.homebrew.resolve_bottle_sizes", return_value={"mock-digest": 500}
    )
    mocker.patch("dark_matter.homebrew.save_bottle_size_cache")

    # Inject fake bottle data to trigger the resolution logic
    mock_brew_metadata["formulae"][0]["bottle"] = {
        "stable": {
            "files": {
                "arm64_tahoe": {
                    "sha256": "mock-digest",
                    "url": "https://ghcr.io/v2/homebrew/core/pkg-a/blobs/sha256:mock-digest",
                }
            }
        }
    }

    df = core.build_theoretical_dataframe(mock_brew_metadata, arch="arm64_tahoe")
    assert not df.empty


def test_build_targeted_analysis_dataframe_missing(mock_brew_metadata):
    """Ensure ValueError is raised if targeted physical tracking misses the topology index."""
    with pytest.raises(ValueError, match="not found in the local installation"):
        core.build_targeted_analysis_dataframe(
            mock_brew_metadata, Path("/mock"), "missing-pkg"
        )


def test_build_compare_analysis_dataframe_partial_matches(
    mocker, mock_brew_metadata, mock_brew_sizes
):
    """Verify comparison structures filter invalid targets while correctly resolving remaining ones."""

    def mock_get_directory_size(path: Path) -> int:
        return int(mock_brew_sizes.get(path.name, 0))

    mocker.patch(
        "dark_matter.homebrew.get_directory_size", side_effect=mock_get_directory_size
    )
    mocker.patch("pathlib.Path.exists", return_value=True)

    df = core.build_compare_analysis_dataframe(
        mock_brew_metadata, Path("/mock"), ["pkg-b", "invalid-pkg"]
    )
    assert len(df) == 1
    assert df.iloc[0]["Package"] == "pkg-b"


def test_build_explain_analysis_dataframe_valid(
    mocker, mock_brew_metadata, mock_brew_sizes
):
    """Verify the localized dependency attribution breakdown calculations for single formulas."""

    def mock_get_directory_size(path: Path) -> int:
        return int(mock_brew_sizes.get(path.name, 0))

    mocker.patch(
        "dark_matter.homebrew.get_directory_size", side_effect=mock_get_directory_size
    )
    mocker.patch("pathlib.Path.exists", return_value=True)

    df = core.build_explain_analysis_dataframe(
        mock_brew_metadata, Path("/mock"), "pkg-a"
    )
    # pkg-a has dependencies pkg-b and pkg-c
    assert "pkg-b" in df["Dependency"].values
    assert "pkg-c" in df["Dependency"].values


def test_build_targeted_theoretical_dataframe_bounds(mock_brew_metadata):
    """Verify theoretical missing target validations throw catalog lookup errors."""
    with pytest.raises(ValueError, match="not found in the catalog"):
        core.build_targeted_theoretical_dataframe(
            mock_brew_metadata, "non-existent", "arm64_tahoe"
        )
