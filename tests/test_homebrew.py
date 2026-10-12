import json
import os
import subprocess
from pathlib import Path

import pytest
import requests

from dark_matter_cli import homebrew


def test_get_brew_prefix(mocker):
    """Verify prefix extraction from standard stdout."""
    mock_result = mocker.Mock()
    mock_result.stdout = "/opt/homebrew\n"
    mocker.patch("subprocess.run", return_value=mock_result)

    assert homebrew.get_brew_prefix() == Path("/opt/homebrew")


def test_get_brew_prefix_failure(mocker):
    """Verify exception raising on subprocess failure."""
    mocker.patch(
        "subprocess.run",
        side_effect=subprocess.CalledProcessError(1, "brew", stderr="error"),
    )

    with pytest.raises(RuntimeError, match="Failed to resolve Homebrew prefix"):
        homebrew.get_brew_prefix()


def test_get_directory_size(tmp_path):
    """Verify physical mass computation using actual I/O operations."""
    test_dir = tmp_path / "target"
    test_dir.mkdir()

    # Construct a 25-byte nested structure
    (test_dir / "file1.txt").write_bytes(b"1234567890")  # 10 bytes
    sub_dir = test_dir / "lib"
    sub_dir.mkdir()
    (sub_dir / "file2.bin").write_bytes(b"A" * 15)  # 15 bytes

    size = homebrew.get_directory_size(test_dir)
    assert size == 25


def test_fetch_blob_size(mocker):
    """Verify ghcr.io blob header parsing."""
    mock_resp = mocker.Mock()
    mock_resp.headers = {"Content-Length": "1048576"}
    mocker.patch("requests.head", return_value=mock_resp)

    url = "https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:123"
    assert homebrew._fetch_blob_size(url, "mock_token", timeout=1.0) == 1048576


@pytest.mark.parametrize(
    "headers", [{}, {"Content-Length": "0"}, {"Content-Length": "x"}]
)
def test_fetch_blob_size_unusable_headers(mocker, headers):
    """Verify missing, zero, or malformed sizes are reported as failures."""
    mock_resp = mocker.Mock()
    mock_resp.headers = headers
    mocker.patch("requests.head", return_value=mock_resp)

    assert homebrew._fetch_blob_size("https://ghcr.io/x", "t", timeout=1.0) is None


def test_get_directory_size_permission_error(tmp_path, mocker):
    """Verify PermissionError handles unreadable path contexts smoothly."""
    test_dir = tmp_path / "protected"
    test_dir.mkdir()
    mocker.patch("os.scandir", side_effect=PermissionError)

    assert homebrew.get_directory_size(test_dir) == 0


def test_parse_repository_edge_cases():
    """Verify OCI repository signature extraction rules on matching boundaries."""
    assert homebrew._parse_repository("https://invalid.url/blob") is None
    assert (
        homebrew._parse_repository(
            "https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:abc"
        )
        == "homebrew/core/wget"
    )


def test_fetch_ghcr_token_failure(mocker):
    """Verify authentication requests fail down gracefully inside standard networks."""
    mocker.patch("requests.get", side_effect=requests.RequestException)
    assert homebrew._fetch_ghcr_token(["homebrew/core/wget"], timeout=1.0) is None


def test_load_api_cache_jws_handling(tmp_path):
    """Verify decryption workflows extract proper payload list metadata configurations."""
    jws_file = tmp_path / "formula.jws.json"
    json_file = tmp_path / "formula.json"

    # Write a mock valid JWS wrapper structure
    valid_payload = json.dumps([{"name": "test-pkg", "dependencies": []}])
    jws_file.write_text(json.dumps({"payload": valid_payload}), encoding="utf-8")

    data = homebrew._load_api_cache(json_file, jws_file)
    assert len(data) == 1
    assert data[0]["name"] == "test-pkg"


def test_load_api_cache_corrupt_payload(tmp_path):
    """Verify fallback actions drop invalid lists gracefully."""
    jws_file = tmp_path / "formula.jws.json"
    json_file = tmp_path / "formula.json"

    jws_file.write_text(json.dumps({"payload": "{corrupt-json"}), encoding="utf-8")
    assert homebrew._load_api_cache(json_file, jws_file) == []


def test_get_brew_metadata_missing_executable(mocker):
    """Verify a missing brew executable raises a RuntimeError, not FileNotFoundError."""
    mocker.patch("subprocess.run", side_effect=FileNotFoundError("brew"))

    with pytest.raises(RuntimeError, match="not found on PATH"):
        homebrew.get_brew_metadata()


def _blob_url(repo: str, digest: str) -> str:
    return f"https://ghcr.io/v2/homebrew/core/{repo}/blobs/sha256:{digest}"


def test_resolve_bottle_sizes_skips_network_for_cached(mocker):
    """Verify cached digests never trigger token or HEAD requests."""
    token = mocker.patch("dark_matter_cli.homebrew._fetch_ghcr_token")
    head = mocker.patch("dark_matter_cli.homebrew._fetch_blob_size")

    result = homebrew.resolve_bottle_sizes({"d1": _blob_url("wget", "d1")}, {"d1": 42})
    assert result == {"d1": 42}
    token.assert_not_called()
    head.assert_not_called()


def test_resolve_bottle_sizes_does_not_cache_failures(mocker):
    """Verify failed lookups resolve to 0 but are not persisted."""
    mocker.patch("dark_matter_cli.homebrew._fetch_ghcr_token", return_value="t")
    mocker.patch(
        "dark_matter_cli.homebrew._fetch_blob_size",
        side_effect=lambda url, *_: None if "bad" in url else 100,
    )
    cache: dict[str, int] = {}
    bottles = {"ok": _blob_url("wget", "ok"), "bad": _blob_url("jq", "bad")}

    result = homebrew.resolve_bottle_sizes(bottles, cache)
    assert result == {"ok": 100, "bad": 0}
    assert cache == {"ok": 100}


def test_resolve_bottle_sizes_token_failure_not_cached(mocker):
    """Verify a failed token request zeroes its batch without caching."""
    mocker.patch("dark_matter_cli.homebrew._fetch_ghcr_token", return_value=None)
    head = mocker.patch("dark_matter_cli.homebrew._fetch_blob_size")
    cache: dict[str, int] = {}

    result = homebrew.resolve_bottle_sizes({"d1": _blob_url("wget", "d1")}, cache)
    assert result == {"d1": 0}
    assert cache == {}
    head.assert_not_called()


def test_resolve_bottle_sizes_batches_token_scopes(mocker):
    """Verify one token request covers a batch of repositories."""
    token = mocker.patch("dark_matter_cli.homebrew._fetch_ghcr_token", return_value="t")
    mocker.patch("dark_matter_cli.homebrew._fetch_blob_size", return_value=1)
    count = homebrew._TOKEN_SCOPE_BATCH_SIZE + 1
    bottles = {f"d{i}": _blob_url(f"pkg{i}", f"d{i}") for i in range(count)}

    homebrew.resolve_bottle_sizes(bottles, {})
    assert token.call_count == 2
    scoped = sorted(len(call.args[0]) for call in token.call_args_list)
    assert scoped == [1, homebrew._TOKEN_SCOPE_BATCH_SIZE]


def test_resolve_bottle_sizes_unparseable_url(mocker):
    """Verify non-ghcr URLs resolve to 0 without network access."""
    token = mocker.patch("dark_matter_cli.homebrew._fetch_ghcr_token")

    result = homebrew.resolve_bottle_sizes({"d1": "https://example.com/x"}, {})
    assert result == {"d1": 0}
    token.assert_not_called()


@pytest.fixture
def cache_path(tmp_path, monkeypatch):
    path = tmp_path / "cache" / "bottle_sizes.json"
    monkeypatch.setattr(homebrew, "_BOTTLE_SIZE_CACHE_PATH", path)
    return path


def test_bottle_size_cache_roundtrip(cache_path):
    """Verify the cache persists and reloads without leaving temp files."""
    homebrew.save_bottle_size_cache({"a": 1, "b": 2})
    assert homebrew.load_bottle_size_cache() == {"a": 1, "b": 2}
    assert [p.name for p in cache_path.parent.iterdir()] == ["bottle_sizes.json"]


def test_load_bottle_size_cache_drops_poisoned_entries(cache_path):
    """Verify zero-size entries cached by older versions are discarded."""
    cache_path.parent.mkdir(parents=True)
    cache_path.write_text(json.dumps({"good": 10, "zero": 0, "junk": "x"}))
    assert homebrew.load_bottle_size_cache() == {"good": 10}


@pytest.mark.parametrize("content", ["{not json", "[1, 2]"])
def test_load_bottle_size_cache_invalid(cache_path, content):
    """Verify corrupt or wrongly-shaped caches load as empty."""
    cache_path.parent.mkdir(parents=True)
    cache_path.write_text(content)
    assert homebrew.load_bottle_size_cache() == {}


def test_load_bottle_size_cache_missing(cache_path):
    """Verify a missing cache loads as empty."""
    assert homebrew.load_bottle_size_cache() == {}


def test_save_bottle_size_cache_evicts_oldest(cache_path, monkeypatch):
    """Verify the cache is capped by dropping the oldest entries."""
    monkeypatch.setattr(homebrew, "_BOTTLE_SIZE_CACHE_MAX_ENTRIES", 2)
    homebrew.save_bottle_size_cache({"old": 1, "mid": 2, "new": 3})
    assert homebrew.load_bottle_size_cache() == {"mid": 2, "new": 3}


def test_save_bottle_size_cache_unwritable(cache_path, mocker, caplog):
    """Verify write failures are logged instead of crashing the pipeline."""
    mocker.patch("tempfile.NamedTemporaryFile", side_effect=OSError("read-only"))
    homebrew.save_bottle_size_cache({"a": 1})
    assert "Could not write bottle size cache" in caplog.text


def test_get_brew_cache(mocker):
    """Verify cache directory extraction from `brew --cache`."""
    run = mocker.patch("subprocess.run")
    run.return_value.stdout = "/home/user/.cache/Homebrew\n"

    assert homebrew.get_brew_cache() == Path("/home/user/.cache/Homebrew")
    assert run.call_args.args[0] == ["brew", "--cache"]


@pytest.mark.parametrize(
    ("system", "machine", "mac_release", "expected"),
    [
        ("darwin", "arm64", "26.6.2", "arm64_tahoe"),
        ("darwin", "x86_64", "15.1", "sequoia"),
        ("darwin", "arm64", "27.0", "arm64_tahoe"),
        ("darwin", "arm64", "10.15", "arm64_big_sur"),
        ("linux", "x86_64", "", "x86_64_linux"),
        ("linux", "aarch64", "", "arm64_linux"),
    ],
)
def test_detect_bottle_tag(mocker, system, machine, mac_release, expected):
    """Verify bottle tags across platforms, CPUs, and unknown macOS releases."""
    mocker.patch("sys.platform", system)
    mocker.patch("platform.machine", return_value=machine)
    mocker.patch("platform.mac_ver", return_value=(mac_release, ("", "", ""), ""))

    assert homebrew.detect_bottle_tag() == expected


def test_get_theoretical_catalog_uses_brew_cache(mocker, tmp_path):
    """Verify the catalog is located via `brew --cache` when no dir is given."""
    api_dir = tmp_path / "api"
    api_dir.mkdir()
    (api_dir / "formula.json").write_text(json.dumps([{"name": "wget"}]))
    mocker.patch("dark_matter_cli.homebrew.get_brew_cache", return_value=tmp_path)

    catalog = homebrew.get_theoretical_catalog()
    assert catalog == {"formulae": [{"name": "wget"}], "casks": []}


def test_get_theoretical_catalog_missing(tmp_path):
    """Verify a missing cache names the searched directory."""
    with pytest.raises(RuntimeError, match=str(tmp_path)):
        homebrew.get_theoretical_catalog(tmp_path)


def _write_jws(path: Path, payload) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"payload": json.dumps(payload), "signatures": []}))


INTERNAL_PAYLOAD = {
    "formulae": {
        "wget": {
            "stable_dependencies": ["openssl@4", {"pkgconf": ":build"}],
            "stable_uses_from_macos": [["zlib"], [{"bison": ":build"}]],
            "bottle_checksum": "abc",
        },
        "python@3.14": {"bottle_checksum": "def", "bottle_tag": ":all"},
        "afl++": {"stable_dependencies": [{"llvm": [":build", ":test"]}]},
    },
    "casks": {
        "brave-browser": {"names": ["Brave"], "depends_on_args": {":macos": ":any"}},
        "azure-cli": {
            "names": ["Azure CLI"],
            "depends_on_args": {":formula": "python@3.14"},
        },
    },
}


def test_runtime_dependencies_excludes_build_and_test():
    """Verify only runtime dependencies survive normalization."""
    entries = ["a", {"b": ":build"}, {"c": [":build", ":test"]}, {"d": ":recommended"}]
    assert homebrew._runtime_dependencies(entries) == ["a", "d"]


def test_load_internal_catalog_formulae(tmp_path):
    """Verify Homebrew 7 formulae are normalized to the legacy shape."""
    path = tmp_path / "internal" / "packages.arm64_tahoe.jws.json"
    _write_jws(path, INTERNAL_PAYLOAD)

    formulae = {
        f["name"]: f
        for f in homebrew._load_internal_catalog(path, "arm64_tahoe")["formulae"]
    }

    assert formulae["wget"]["dependencies"] == ["openssl@4"]
    assert formulae["wget"]["bottle"]["stable"]["files"] == {
        "arm64_tahoe": {
            "sha256": "abc",
            "url": "https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:abc",
        }
    }
    python_files = formulae["python@3.14"]["bottle"]["stable"]["files"]
    assert python_files["all"]["url"].startswith(
        "https://ghcr.io/v2/homebrew/core/python/3.14/blobs/"
    )
    assert formulae["afl++"]["dependencies"] == []
    assert "bottle" not in formulae["afl++"]


def test_load_internal_catalog_linux_includes_uses_from_macos(tmp_path):
    """Verify macOS-provided libraries become dependencies on Linux."""
    path = tmp_path / "packages.x86_64_linux.jws.json"
    _write_jws(path, INTERNAL_PAYLOAD)

    formulae = homebrew._load_internal_catalog(path, "x86_64_linux")["formulae"]
    wget = next(f for f in formulae if f["name"] == "wget")
    assert wget["dependencies"] == ["openssl@4", "zlib"]


def test_load_internal_catalog_casks(tmp_path):
    """Verify casks keep their token and formula dependencies."""
    path = tmp_path / "packages.arm64_tahoe.jws.json"
    _write_jws(path, INTERNAL_PAYLOAD)

    casks = homebrew._load_internal_catalog(path, "arm64_tahoe")["casks"]
    assert casks == [
        {"token": "brave-browser", "name": ["Brave"], "depends_on": {}},
        {
            "token": "azure-cli",
            "name": ["Azure CLI"],
            "depends_on": {"formula": ["python@3.14"]},
        },
    ]


def test_load_internal_catalog_malformed(tmp_path):
    """Verify an unparseable internal cache raises a pipeline error."""
    path = tmp_path / "packages.arm64_tahoe.jws.json"
    _write_jws(path, ["not", "a", "dict"])

    with pytest.raises(RuntimeError, match="Failed to parse"):
        homebrew._load_internal_catalog(path, "arm64_tahoe")


def test_get_theoretical_catalog_prefers_internal(tmp_path):
    """Verify the Homebrew 7 file wins over legacy caches."""
    _write_jws(
        tmp_path / "internal" / "packages.arm64_tahoe.jws.json", INTERNAL_PAYLOAD
    )
    _write_jws(tmp_path / "formula.jws.json", [{"name": "legacy"}])

    catalog = homebrew.get_theoretical_catalog(tmp_path, tag="arm64_tahoe")
    assert {f["name"] for f in catalog["formulae"]} == {"wget", "python@3.14", "afl++"}


def test_get_theoretical_catalog_falls_back_to_legacy(tmp_path):
    """Verify legacy caches are used when no internal file matches."""
    _write_jws(tmp_path / "internal" / "packages.sequoia.jws.json", INTERNAL_PAYLOAD)
    _write_jws(tmp_path / "formula.jws.json", [{"name": "legacy"}])

    catalog = homebrew.get_theoretical_catalog(tmp_path, tag="arm64_tahoe")
    assert catalog == {"formulae": [{"name": "legacy"}], "casks": []}


def test_get_theoretical_catalog_wrong_tag(tmp_path):
    """Verify a mismatched --arch lists which tags are cached."""
    _write_jws(
        tmp_path / "internal" / "packages.arm64_tahoe.jws.json", INTERNAL_PAYLOAD
    )

    with pytest.raises(RuntimeError, match=r"x86_64_linux.*available: arm64_tahoe"):
        homebrew.get_theoretical_catalog(tmp_path, tag="x86_64_linux")


def test_get_theoretical_catalog_detects_tag(tmp_path, mocker):
    """Verify the catalog defaults to this machine's bottle tag."""
    mocker.patch(
        "dark_matter_cli.homebrew.detect_bottle_tag", return_value="arm64_tahoe"
    )
    _write_jws(
        tmp_path / "internal" / "packages.arm64_tahoe.jws.json", INTERNAL_PAYLOAD
    )

    assert len(homebrew.get_theoretical_catalog(tmp_path)["casks"]) == 2


@pytest.mark.parametrize("content", ["{not json", json.dumps({"payload": 5})])
def test_read_jws_payload_malformed(tmp_path, content):
    """Verify malformed JWS files are reported as None."""
    path = tmp_path / "x.jws.json"
    path.write_text(content)
    assert homebrew._read_jws_payload(path) is None


def test_load_api_cache_plain_json(tmp_path):
    """Verify the legacy non-JWS cache is read, and non-lists rejected."""
    json_file = tmp_path / "formula.json"
    json_file.write_text(json.dumps([{"name": "wget"}]))
    assert homebrew._load_api_cache(json_file, tmp_path / "missing") == [
        {"name": "wget"}
    ]

    json_file.write_text(json.dumps({"name": "wget"}))
    assert homebrew._load_api_cache(json_file, tmp_path / "missing") == []


def test_get_path_size(tmp_path):
    """Verify files, directories, symlinks, and missing paths are measured correctly."""
    app = tmp_path / "Foo.app"
    app.mkdir()
    (app / "bin").write_bytes(b"x" * 30)
    font = tmp_path / "Font.otf"
    font.write_bytes(b"x" * 7)
    link = tmp_path / "foo"
    link.symlink_to(app / "bin")

    assert homebrew.get_path_size(app) == 30
    assert homebrew.get_path_size(font) == 7
    assert homebrew.get_path_size(link) == 0
    assert homebrew.get_path_size(tmp_path / "missing") == 0


def test_get_directory_size_skips_unreadable_entries(tmp_path, mocker):
    """Verify one failing entry doesn't discard its siblings."""
    (tmp_path / "ok.bin").write_bytes(b"x" * 5)
    (tmp_path / "bad.bin").write_bytes(b"x" * 9)

    real_stat = os.DirEntry.stat

    def flaky_stat(entry, *args, **kwargs):
        if entry.name == "bad.bin":
            raise FileNotFoundError(entry.path)
        return real_stat(entry, *args, **kwargs)

    mocker.patch.object(os.DirEntry, "stat", flaky_stat)
    assert homebrew.get_directory_size(tmp_path) == 5


def test_get_cask_size_uses_resolved_targets(tmp_path):
    """Verify Homebrew 7 artifact targets are measured, skipping symlinks."""
    prefix = tmp_path / "brew"
    caskroom = prefix / "Caskroom" / "brave-browser" / "1.0"
    caskroom.mkdir(parents=True)
    (caskroom / "meta.json").write_bytes(b"x" * 3)

    app = tmp_path / "Applications" / "Brave Browser.app"
    app.mkdir(parents=True)
    (app / "binary").write_bytes(b"x" * 100)
    link = prefix / "bin" / "brave"
    link.parent.mkdir()
    link.symlink_to(app / "binary")

    cask = {
        "token": "brave-browser",
        "artifacts": [
            {"app": ["Brave Browser.app"], "target": str(app)},
            {"binary": [str(app / "binary")], "target": str(link)},
            {"uninstall": [{"quit": "com.brave.Browser"}]},
            "unexpected",
        ],
    }
    assert homebrew.get_cask_size(cask, prefix) == 103


def test_cask_artifact_paths_infers_legacy_app_locations():
    """Verify app paths are inferred when Homebrew omits resolved targets."""
    cask = {
        "artifacts": [
            {"app": ["qbittorrent.app", {"target": "qBittorrent.app"}]},
            {"suite": ["Adobe Suite"]},
            {"app": ["Absolute.app", {"target": "/Opt/Absolute.app"}]},
            {"pkg": ["Installer.pkg"]},
        ]
    }
    assert homebrew._cask_artifact_paths(cask) == [
        Path("/Applications/qBittorrent.app"),
        Path("/Applications/Adobe Suite"),
        Path("/Opt/Absolute.app"),
    ]


def test_get_brew_metadata_invalid_json(mocker):
    """Verify unparseable brew output raises a RuntimeError."""
    mocker.patch("subprocess.run").return_value.stdout = "not json"

    with pytest.raises(RuntimeError, match="Failed to parse Homebrew JSON"):
        homebrew.get_brew_metadata()


def test_fetch_ghcr_token_success(mocker):
    """Verify every repository is sent as its own scope parameter."""
    get = mocker.patch("requests.get")
    get.return_value.json.return_value = {"token": "abc"}

    token = homebrew._fetch_ghcr_token(["homebrew/core/a", "homebrew/core/b"], 1.0)
    assert token == "abc"
    assert get.call_args.kwargs["params"] == [
        ("service", "ghcr.io"),
        ("scope", "repository:homebrew/core/a:pull"),
        ("scope", "repository:homebrew/core/b:pull"),
    ]


def _make_rack(prefix, name, kegs):
    """Create Cellar/<name>/<version>/payload files of the given sizes."""
    for version, size in kegs.items():
        keg = prefix / "Cellar" / name / version
        keg.mkdir(parents=True)
        (keg / "payload").write_bytes(b"x" * size)


def test_get_keg_sizes_follows_opt_link(tmp_path):
    """Verify the opt link picks the current keg and the rest are stale."""
    _make_rack(tmp_path, "foo", {"1.0": 10, "2.0": 20, "2.0_1": 30})
    (tmp_path / "opt").mkdir()
    (tmp_path / "opt" / "foo").symlink_to("../Cellar/foo/2.0_1")

    kegs = homebrew.get_keg_sizes({"name": "foo"}, tmp_path)
    assert kegs == homebrew.KegSizes(current=30, stale={"1.0": 10, "2.0": 20})


@pytest.mark.parametrize(
    "formula",
    [
        {"name": "foo", "linked_keg": "2.0"},
        {"name": "foo", "installed": [{"version": "1.0"}, {"version": "2.0"}]},
    ],
)
def test_get_keg_sizes_without_opt_link(tmp_path, formula):
    """Verify the linked keg, then the newest installed version, are used."""
    _make_rack(tmp_path, "foo", {"1.0": 10, "2.0": 20})

    kegs = homebrew.get_keg_sizes(formula, tmp_path)
    assert kegs == homebrew.KegSizes(current=20, stale={"1.0": 10})


@pytest.mark.parametrize(
    "formula", [{"name": "foo"}, {"name": "foo", "linked_keg": "9.9"}]
)
def test_get_keg_sizes_unknown_current_counts_everything(tmp_path, formula):
    """Verify nothing is called stale when the current keg can't be found."""
    _make_rack(tmp_path, "foo", {"1.0": 10, "2.0": 20})

    assert homebrew.get_keg_sizes(formula, tmp_path) == homebrew.KegSizes(current=30)


def test_get_keg_sizes_ignores_foreign_opt_link(tmp_path):
    """Verify an opt link pointing outside the formula's rack is ignored."""
    _make_rack(tmp_path, "foo", {"1.0": 10})
    (tmp_path / "opt").mkdir()
    (tmp_path / "opt" / "foo").symlink_to(tmp_path)

    kegs = homebrew.get_keg_sizes({"name": "foo", "linked_keg": "1.0"}, tmp_path)
    assert kegs == homebrew.KegSizes(current=10)


def test_get_keg_sizes_missing_rack(tmp_path):
    """Verify a formula with no Cellar directory measures as empty."""
    kegs = homebrew.get_keg_sizes({"name": "gone", "linked_keg": "1.0"}, tmp_path)
    assert kegs == homebrew.KegSizes(current=0)
