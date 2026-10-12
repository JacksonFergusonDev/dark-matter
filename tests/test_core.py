import math
from pathlib import Path

import pandas as pd
import pytest
from hypothesis import given
from hypothesis import strategies as st

from dark_matter_cli import core
from dark_matter_cli.homebrew import KegSizes


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


_CATALOG_WITH_CASK = {
    "formulae": [{"name": "ghostscript", "dependencies": []}],
    "casks": [{"token": "mactex", "depends_on": {"formula": ["ghostscript"]}}],
}


def test_catalog_topology_excludes_casks():
    """Verify unsized casks neither appear nor take shares in catalog mode."""
    topo = core._build_global_topology(_CATALOG_WITH_CASK, catalog=True)
    assert set(topo.graph) == {"ghostscript"}
    # With the cask gone, nothing depends on ghostscript: it pays for itself.
    assert topo.roots == {"ghostscript"}


@pytest.mark.parametrize(
    "build",
    [
        lambda m: core.build_targeted_theoretical_dataframe(m, "mactex"),
        lambda m: core.build_compare_theoretical_dataframe(
            m, ["ghostscript", "mactex"]
        ),
        lambda m: core.build_explain_theoretical_dataframe(m, "mactex"),
    ],
)
def test_catalog_targets_reject_casks(build):
    """Verify asking the catalog about a cask explains why it can't be sized."""
    with pytest.raises(ValueError, match="'mactex' is a cask") as exc:
        build(_CATALOG_WITH_CASK)
    assert not isinstance(exc.value, core.PackageNotFoundError)


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


def _installed(name, deps=(), requested=True, recorded=()):
    """Build an installed formula entry with a recorded keg."""
    keg = {
        "version": "1.0",
        "installed_on_request": requested,
        "runtime_dependencies": [{"full_name": d} for d in recorded],
    }
    return {"name": name, "dependencies": list(deps), "installed": [keg]}


@pytest.fixture
def sized(mocker):
    """Measure each package from a name-to-bytes map."""

    def use(sizes):
        mocker.patch(
            "dark_matter_cli.core._resolve_physical_sizes",
            side_effect=lambda pkgs, _: {n: sizes[n] for n in pkgs},
        )

    return use


def test_recorded_dependencies_extend_closures():
    """Verify a keg's recorded runtime deps count even if the formula dropped them."""
    metadata = {
        "formulae": [
            _installed("app", recorded=["homebrew/core/old-lib"]),
            {
                "name": "old-lib",
                "full_name": "homebrew/core/old-lib",
                "dependencies": [],
            }
            | {"installed": [{"installed_on_request": False}]},
        ]
    }
    topo = core._build_global_topology(metadata, catalog=False)

    assert topo.graph["app"] == []
    assert topo.trans_deps["app"] == {"old-lib"}
    assert topo.roots == {"app"}


def test_reclaim_frees_exclusive_dependencies(sized):
    """Verify only dependencies no other root needs are freed."""
    sized({"app": 10, "only": 20, "shared": 40})
    metadata = {
        "formulae": [
            _installed("app", ["only", "shared"]),
            _installed("other", ["shared"]),
            _installed("only", requested=False),
            _installed("shared", requested=False),
        ]
    }

    result = core.build_reclaim_dataframe(metadata, Path("/brew"), ["app"])

    assert list(result.freed["Package"]) == ["only", "app"]
    assert list(result.freed["Is_Target"]) == [False, True]
    assert result.blocked == {}


def test_reclaim_reports_blocked_targets(sized):
    """Verify targets other packages need are kept, naming their dependents."""
    sized({"app": 10})
    metadata = {
        "formulae": [
            _installed("app"),
            _installed("x", ["lib"]),
            _installed("y", recorded=["lib"]),
            _installed("lib"),
        ]
    }

    result = core.build_reclaim_dataframe(metadata, Path("/brew"), ["lib", "app"])

    assert result.blocked == {"lib": ["x"]}
    assert list(result.freed["Package"]) == ["app"]


def test_reclaim_unknown_package(mock_brew_metadata):
    """Verify reclaim rejects packages that aren't installed."""
    with pytest.raises(core.PackageNotFoundError):
        core.build_reclaim_dataframe(mock_brew_metadata, Path("/brew"), ["nope"])


def test_reclaim_nothing_freed_has_columns(sized):
    """Verify an empty result still has the expected columns."""
    sized({})
    metadata = {"formulae": [_installed("x", ["lib"]), _installed("lib")]}

    result = core.build_reclaim_dataframe(metadata, Path("/brew"), ["lib"])
    assert result.freed.empty
    assert list(result.freed.columns) == ["Package", "Core_Bytes", "Is_Target"]


def test_orphans_match_autoremove(sized):
    """Verify orphans are deps no requested package reaches, recorded deps included."""
    sized({"o": 5, "p": 7})
    metadata = {
        "formulae": [
            _installed("app", recorded=["kept-by-record"]),
            _installed("kept-by-record", requested=False),
            _installed("o", ["p"], requested=False),
            _installed("p", requested=False),
        ],
        "casks": [{"token": "some-app", "depends_on": {"formula": ["cask-dep"]}}],
    }
    metadata["formulae"].append(_installed("cask-dep", requested=False))

    df = core.build_orphans_dataframe(metadata, Path("/brew"))

    assert list(df["Package"]) == ["p", "o"]
    assert list(df["Required_By"]) == [["o"], []]


def test_cleanup_lists_stale_kegs(mocker):
    """Verify each stale keg becomes a row, largest first."""
    sizes = {
        "a": KegSizes(current=1, stale={"1.0": 5}),
        "b": KegSizes(current=1, stale={"0.9": 50, "0.8": 5}),
        "c": KegSizes(current=1),
    }
    mocker.patch(
        "dark_matter_cli.homebrew.get_keg_sizes",
        side_effect=lambda f, _: sizes[f["name"]],
    )
    metadata = {"formulae": [{"name": n} for n in sizes]}

    df = core.build_cleanup_dataframe(metadata, Path("/brew"))

    assert df.to_dict("records") == [
        {"Package": "b", "Version": "0.9", "Bytes": 50},
        {"Package": "a", "Version": "1.0", "Bytes": 5},
        {"Package": "b", "Version": "0.8", "Bytes": 5},
    ]


def test_cleanup_none(mocker):
    """Verify an installation without stale kegs yields an empty frame."""
    mocker.patch(
        "dark_matter_cli.homebrew.get_keg_sizes",
        return_value=KegSizes(current=1),
    )
    df = core.build_cleanup_dataframe({"formulae": [{"name": "a"}]}, Path("/b"))
    assert df.empty


def test_physical_sizes_exclude_stale_kegs(mocker):
    """Verify a formula's core size is its current keg only."""
    mocker.patch(
        "dark_matter_cli.homebrew.get_keg_sizes",
        return_value=KegSizes(current=10, stale={"0.9": 99}),
    )
    assert core._resolve_physical_sizes({"a": {"name": "a"}}, Path("/b")) == {"a": 10}


def _snapshot(rows):
    return pd.DataFrame(rows, columns=["Package", "Core_Bytes", "Weighted_Bytes"])


def test_diff_snapshots():
    """Verify added, removed, and changed packages are reported, unchanged ones not."""
    old = _snapshot([("same", 5, 9), ("grew", 10, 20), ("gone", 3, 4)])
    new = _snapshot([("same", 5, 9), ("grew", 30, 45), ("new", 7, 8)])

    df = core.diff_snapshots(old, new)

    assert df.to_dict("records") == [
        {
            "Package": "grew",
            "Status": "changed",
            "Old_Core": 10.0,
            "New_Core": 30.0,
            "Core_Delta": 20.0,
            "Weighted_Delta": 25.0,
        },
        {
            "Package": "new",
            "Status": "added",
            "Old_Core": 0.0,
            "New_Core": 7.0,
            "Core_Delta": 7.0,
            "Weighted_Delta": 8.0,
        },
        {
            "Package": "gone",
            "Status": "removed",
            "Old_Core": 3.0,
            "New_Core": 0.0,
            "Core_Delta": -3.0,
            "Weighted_Delta": -4.0,
        },
    ]


def test_diff_snapshots_requires_export_columns():
    """Verify snapshots missing size columns are rejected with a hint."""
    with pytest.raises(ValueError, match="old snapshot is missing Weighted_Bytes"):
        core.diff_snapshots(
            pd.DataFrame({"Package": [], "Core_Bytes": []}), _snapshot([])
        )


def test_explain_required_by_falls_back_to_target():
    """Verify deps known only from a keg record hang off the target."""
    metadata = {
        "formulae": [
            _installed("app", ["lib"], recorded=["lib", "ghost"]),
            _installed("lib", requested=False),
            _installed("ghost", requested=False),
        ]
    }
    topo = core._build_global_topology(metadata, catalog=False)
    df = core._compute_explain_metrics("app", {}, topo, is_theoretical=False)

    required_by = dict(zip(df["Dependency"], df["Required_By"], strict=True))
    assert required_by == {"lib": ["app"], "ghost": ["app"]}
