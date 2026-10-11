import json
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
