from pathlib import Path

import pandas as pd
import pytest

from dark_matter_cli import core


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
        "dark_matter_cli.homebrew.get_directory_size",
        side_effect=mock_get_directory_size,
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
    mocker.patch("dark_matter_cli.homebrew.load_bottle_size_cache", return_value={})
    mocker.patch(
        "dark_matter_cli.homebrew.resolve_bottle_sizes",
        return_value={"mock-digest": 500},
    )
    mocker.patch("dark_matter_cli.homebrew.save_bottle_size_cache")

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


def test_build_compare_analysis_dataframe_reports_all_missing(mock_brew_metadata):
    """Verify comparisons name every missing target instead of dropping them."""
    with pytest.raises(core.PackageNotFoundError) as exc:
        core.build_compare_analysis_dataframe(
            mock_brew_metadata, Path("/mock"), ["pkg-b", "nope-1", "nope-2"]
        )
    assert exc.value.packages == ["nope-1", "nope-2"]
    assert exc.value.location == core.INSTALLED
    assert "Packages 'nope-1', 'nope-2' not found" in str(exc.value)


def test_build_compare_theoretical_dataframe_missing(mock_brew_metadata):
    """Verify catalog comparisons reject unknown packages."""
    with pytest.raises(core.PackageNotFoundError, match="not found in the catalog"):
        core.build_compare_theoretical_dataframe(mock_brew_metadata, ["pkg-a", "nope"])


def test_build_explain_analysis_dataframe_valid(
    mocker, mock_brew_metadata, mock_brew_sizes
):
    """Verify the localized dependency attribution breakdown calculations for single formulas."""

    def mock_get_directory_size(path: Path) -> int:
        return int(mock_brew_sizes.get(path.name, 0))

    mocker.patch(
        "dark_matter_cli.homebrew.get_directory_size",
        side_effect=mock_get_directory_size,
    )
    mocker.patch("pathlib.Path.exists", return_value=True)

    df = core.build_explain_analysis_dataframe(
        mock_brew_metadata, Path("/mock"), "pkg-a"
    )
    # pkg-a has dependencies pkg-b and pkg-c
    assert "pkg-b" in df["Dependency"].to_numpy()
    assert "pkg-c" in df["Dependency"].to_numpy()


def test_build_targeted_theoretical_dataframe_bounds(mock_brew_metadata):
    """Verify theoretical missing target validations throw catalog lookup errors."""
    with pytest.raises(ValueError, match="not found in the catalog"):
        core.build_targeted_theoretical_dataframe(
            mock_brew_metadata, "non-existent", "arm64_tahoe"
        )


def test_compare_keeps_accurate_leaf_flags(mocker, mock_brew_metadata):
    """Verify Is_Leaf reflects the graph rather than being forced for targets."""
    mocker.patch("dark_matter_cli.homebrew.get_directory_size", return_value=10)
    mocker.patch("pathlib.Path.exists", return_value=True)

    df = core.build_compare_analysis_dataframe(
        mock_brew_metadata, Path("/mock"), ["pkg-a", "pkg-d"]
    )
    flags = dict(zip(df["Package"], df["Is_Leaf"], strict=True))
    assert flags == {"pkg-a": True, "pkg-d": False}

    single = core.build_targeted_analysis_dataframe(
        mock_brew_metadata, Path("/mock"), "pkg-d"
    )
    assert not single.iloc[0]["Is_Leaf"]


@pytest.mark.parametrize(
    ("files", "tag", "expected"),
    [
        ({"arm64_tahoe": "exact", "all": "any"}, "arm64_tahoe", "exact"),
        ({"all": "any", "arm64_sequoia": "older"}, "arm64_tahoe", "any"),
        ({"sequoia": "intel", "arm64_sequoia": "older"}, "arm64_tahoe", "older"),
        ({"x86_64_linux": "linux"}, "arm64_tahoe", None),
        ({"arm64_sequoia": "mac-arm"}, "sequoia", None),
        (
            {"arm64_linux": "linux-arm", "x86_64_linux": "linux"},
            "x86_64_linux",
            "linux",
        ),
    ],
)
def test_select_bottle_file(files, tag, expected):
    """Verify bottle fallback never crosses CPU or OS families."""
    assert core._select_bottle_file(files, tag) == expected


def test_theoretical_defaults_to_detected_tag(mocker, mock_brew_metadata):
    """Verify an omitted arch resolves via bottle tag detection."""
    mocker.patch(
        "dark_matter_cli.homebrew.detect_bottle_tag", return_value="x86_64_linux"
    )
    resolve = mocker.patch(
        "dark_matter_cli.homebrew.resolve_bottle_sizes", return_value={}
    )
    mocker.patch("dark_matter_cli.homebrew.load_bottle_size_cache", return_value={})
    mocker.patch("dark_matter_cli.homebrew.save_bottle_size_cache")
    mock_brew_metadata["formulae"][0]["bottle"] = {
        "stable": {
            "files": {
                "arm64_tahoe": {"sha256": "mac", "url": "u-mac"},
                "x86_64_linux": {"sha256": "linux", "url": "u-linux"},
            }
        }
    }

    core.build_targeted_theoretical_dataframe(mock_brew_metadata, "pkg-a")
    assert resolve.call_args.args[0] == {"linux": "u-linux"}


def test_topology_keys_casks_by_token_with_dependencies():
    """Verify casks use their token, formula/cask deps, and avoid collisions."""
    metadata = {
        "formulae": [
            {"name": "docker", "dependencies": []},
            {"name": "ghostscript", "dependencies": []},
        ],
        "casks": [
            {"token": "docker", "name": ["Docker Desktop"]},
            {
                "token": "mactex-no-gui",
                "name": ["MacTeX"],
                "depends_on": {"formula": ["ghostscript"], "cask": ["docker"]},
            },
        ],
    }
    graph, _, _, pkg_map = core._build_global_topology(metadata)

    assert set(graph) == {"docker", "ghostscript", "docker (cask)", "mactex-no-gui"}
    assert graph["mactex-no-gui"] == ["ghostscript", "docker"]
    assert pkg_map["docker (cask)"]["token"] == "docker"


def test_physical_sizes_route_casks_and_formulae(mocker):
    """Verify formulae are measured in the Cellar and casks via their artifacts."""
    dir_size = mocker.patch(
        "dark_matter_cli.homebrew.get_directory_size", return_value=10
    )
    cask_size = mocker.patch("dark_matter_cli.homebrew.get_cask_size", return_value=99)
    pkg_map = {"wget": {"name": "wget"}, "brave-browser": {"token": "brave-browser"}}

    sizes = core._resolve_physical_sizes(pkg_map, Path("/brew"))
    assert sizes == {"wget": 10, "brave-browser": 99}
    dir_size.assert_called_once_with(Path("/brew/Cellar/wget"))
    cask_size.assert_called_once_with({"token": "brave-browser"}, Path("/brew"))


@pytest.fixture
def theoretical_sizes(mocker, mock_brew_sizes):
    """Resolve each package's theoretical size from mock_brew_sizes."""
    return mocker.patch(
        "dark_matter_cli.core._resolve_theoretical_sizes",
        side_effect=lambda names, *_: {n: mock_brew_sizes[n] for n in names},
    )


def test_build_compare_theoretical_dataframe_resolves_union(
    mock_brew_metadata, theoretical_sizes
):
    """Verify only the targets' combined closure is resolved."""
    df = core.build_compare_theoretical_dataframe(
        mock_brew_metadata, ["pkg-b", "pkg-c"]
    )

    assert theoretical_sizes.call_args.args[0] == {"pkg-b", "pkg-c", "pkg-d"}
    assert set(df["Package"]) == {"pkg-b", "pkg-c"}


def test_build_explain_theoretical_dataframe_sorted(
    mock_brew_metadata, theoretical_sizes
):
    """Verify theoretical explain rows use archive sizes, sorted by attribution."""
    df = core.build_explain_theoretical_dataframe(mock_brew_metadata, "pkg-a")

    # pkg-b and pkg-d tie at 200 attributed bytes; ties sort by name.
    assert list(df["Dependency"]) == ["pkg-c", "pkg-b", "pkg-d"]
    assert list(df["Archive_Bytes"]) == [300, 200, 600]
    assert list(df["Attributed_Bytes"]) == [300.0, 200.0, 200.0]


def test_build_explain_leaf_has_no_rows(mock_brew_metadata, theoretical_sizes):
    """Verify a package with no dependencies yields an empty breakdown."""
    df = core.build_explain_theoretical_dataframe(mock_brew_metadata, "pkg-d")
    assert df.empty
