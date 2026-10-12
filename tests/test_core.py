import math
from pathlib import Path

import pandas as pd
import pytest
from hypothesis import given
from hypothesis import strategies as st

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

    # pkg-a is the only root, so it is the only sharer of every dependency;
    # pkg-b and pkg-c reaching pkg-d don't dilute its share.
    # Weighted Recursive Size = 100 + 200 + 300 + 600 = 1200
    assert row_a["Weighted_Bytes"] == 1200

    # Bloat Ratio = Weighted / Core = 1200 / 100 = 12.0
    assert row_a["Bloat_Ratio"] == 12.0
    assert row_a["Is_Root"]
    assert not row_a["Is_Requested"]


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


def test_compare_keeps_accurate_root_flags(mocker, mock_brew_metadata):
    """Verify Is_Root reflects the graph rather than being forced for targets."""
    mocker.patch("dark_matter_cli.homebrew.get_directory_size", return_value=10)
    mocker.patch("pathlib.Path.exists", return_value=True)

    df = core.build_compare_analysis_dataframe(
        mock_brew_metadata, Path("/mock"), ["pkg-a", "pkg-d"]
    )
    flags = dict(zip(df["Package"], df["Is_Root"], strict=True))
    assert flags == {"pkg-a": True, "pkg-d": False}

    single = core.build_targeted_analysis_dataframe(
        mock_brew_metadata, Path("/mock"), "pkg-d"
    )
    assert not single.iloc[0]["Is_Root"]


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
    topo = core._build_global_topology(metadata, catalog=False)

    assert set(topo.graph) == {
        "docker",
        "ghostscript",
        "docker (cask)",
        "mactex-no-gui",
    }
    assert topo.graph["mactex-no-gui"] == ["ghostscript", "docker"]
    assert topo.pkg_data["docker (cask)"]["token"] == "docker"
    # Casks are always requested, even when another cask depends on them.
    assert topo.requested == {"docker (cask)", "mactex-no-gui"}


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

    # pkg-a is the only root, so it carries every dependency in full.
    assert list(df["Dependency"]) == ["pkg-d", "pkg-c", "pkg-b"]
    assert list(df["Archive_Bytes"]) == [600, 300, 200]
    assert list(df["Attributed_Bytes"]) == [600.0, 300.0, 200.0]
    assert list(df["Shared_By"]) == [1, 1, 1]


def test_build_explain_leaf_has_no_rows(mock_brew_metadata, theoretical_sizes):
    """Verify a package with no dependencies yields an empty breakdown."""
    df = core.build_explain_theoretical_dataframe(mock_brew_metadata, "pkg-d")
    assert df.empty


def _formula(name, deps=(), requested=None):
    """Build a formula entry; `requested` sets installed_on_request."""
    entry = {"name": name, "dependencies": list(deps)}
    if requested is not None:
        entry["installed"] = [{"installed_on_request": requested}]
    return entry


def _attribute(formulae, sizes, catalog=False):
    """Run the attribution math over every package, keyed by name."""
    topo = core._build_global_topology({"formulae": formulae}, catalog=catalog)
    df = core._compute_bloat_metrics(list(topo.graph), sizes, topo)
    return df.set_index("Package")


def test_attribution_splits_diamond_between_roots():
    """Verify a dependency shared by two roots is split evenly."""
    df = _attribute(
        [_formula("a", ["c"]), _formula("b", ["c"]), _formula("c")],
        {"a": 10, "b": 10, "c": 100},
    )
    assert df.loc["a", "Weighted_Bytes"] == 60
    assert df.loc["b", "Weighted_Bytes"] == 60


def test_attribution_ignores_intermediate_sharers():
    """Verify intermediate dependencies don't take a share they never report."""
    df = _attribute(
        [
            _formula("a", ["b"]),
            _formula("b", ["c"]),
            _formula("d", ["c"]),
            _formula("c"),
        ],
        {"a": 0, "b": 0, "d": 0, "c": 100},
    )
    # c is split between the roots a and d only; b is not a sharer.
    assert df.loc["a", "Weighted_Bytes"] == 50
    assert df.loc["d", "Weighted_Bytes"] == 50
    assert not df.loc["b", "Is_Root"]


def test_attribution_requested_dependency_pays_for_itself():
    """Verify a requested package isn't charged to the packages needing it."""
    df = _attribute(
        [
            _formula("x", ["python"], requested=True),
            _formula("python", ["openssl"], requested=True),
            _formula("openssl", requested=False),
        ],
        {"x": 10, "python": 400, "openssl": 100},
    )
    assert df.loc["python", "Is_Root"]
    assert df.loc["python", "Is_Requested"]
    # x pays half of openssl and nothing for python.
    assert df.loc["x", "Weighted_Bytes"] == 60
    assert df.loc["python", "Weighted_Bytes"] == 450


def test_attribution_orphans_become_roots():
    """Verify dependencies whose dependents are gone still own their bytes."""
    df = _attribute(
        [
            _formula("app", requested=True),
            _formula("o", ["p"], requested=False),
            _formula("p", requested=False),
        ],
        {"app": 5, "o": 10, "p": 20},
    )
    assert df.loc["o", "Is_Root"]
    assert not df.loc["o", "Is_Requested"]
    assert df.loc["o", "Weighted_Bytes"] == 30
    assert not df.loc["p", "Is_Root"]


def test_catalog_mode_ignores_request_flags():
    """Verify catalog roots are only packages nothing depends on."""
    df = _attribute(
        [_formula("x", ["lib"], requested=True), _formula("lib", requested=True)],
        {"x": 1, "lib": 10},
        catalog=True,
    )
    assert not df.loc["lib", "Is_Root"]
    assert df.loc["x", "Weighted_Bytes"] == 11


def test_zero_core_ratio_is_undefined():
    """Verify packages with no measurable core get a NaN ratio, not 1.0."""
    df = _attribute([_formula("meta", ["lib"]), _formula("lib")], {"meta": 0, "lib": 5})
    assert math.isnan(df.loc["meta", "Bloat_Ratio"])
    assert df.loc["lib", "Bloat_Ratio"] == 1.0


def test_explain_sums_to_weighted_size():
    """Verify a target's core plus its explained shares equals its weighted size."""
    formulae = [
        _formula("x", ["python", "zlib"], requested=True),
        _formula("y", ["zlib"], requested=True),
        _formula("python", ["openssl", "zlib"], requested=True),
        _formula("openssl", requested=False),
        _formula("zlib", requested=False),
    ]
    sizes = {"x": 7, "y": 3, "python": 400, "openssl": 90, "zlib": 30}
    topo = core._build_global_topology({"formulae": formulae}, catalog=False)

    weighted = core._compute_bloat_metrics(["x"], sizes, topo).iloc[0]
    explained = core._compute_explain_metrics("x", sizes, topo, is_theoretical=False)

    assert math.isclose(
        sizes["x"] + explained["Attributed_Bytes"].sum(), weighted["Weighted_Bytes"]
    )
    python = explained.set_index("Dependency").loc["python"]
    assert python["Is_Root"]
    assert python["Shared_By"] == 0
    assert python["Attributed_Bytes"] == 0


@st.composite
def _random_installation(draw):
    """Draw a random DAG with sizes and request flags.

    Edges only point from lower to higher indices, so the graph is acyclic.
    """
    n = draw(st.integers(min_value=1, max_value=12))
    names = [f"p{i}" for i in range(n)]
    formulae = []
    for i, name in enumerate(names):
        deps = (
            draw(st.lists(st.sampled_from(names[i + 1 :]), unique=True))
            if i < n - 1
            else []
        )
        formulae.append(_formula(name, deps, requested=draw(st.booleans())))
    sizes = {name: draw(st.integers(min_value=0, max_value=10**9)) for name in names}
    return formulae, sizes, draw(st.booleans())


@given(_random_installation())
def test_attribution_conserves_mass(installation):
    """Verify the roots' weighted sizes account for exactly every byte."""
    formulae, sizes, catalog = installation
    df = _attribute(formulae, sizes, catalog=catalog)
    roots = df[df["Is_Root"]]

    assert math.isclose(
        roots["Weighted_Bytes"].sum(), sum(sizes.values()), rel_tol=1e-9
    )
    assert (roots["Weighted_Bytes"] >= roots["Core_Bytes"]).all()
