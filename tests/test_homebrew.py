import subprocess
from pathlib import Path

import pytest

from dark_matter import homebrew


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


def test_fetch_bottle_size(mocker):
    """Verify ghcr.io blob header parsing."""
    mocker.patch("dark_matter.homebrew._fetch_ghcr_token", return_value="mock_token")

    mock_resp = mocker.Mock()
    mock_resp.headers = {"Content-Length": "1048576"}
    mock_resp.raise_for_status = mocker.Mock()

    mocker.patch("requests.head", return_value=mock_resp)

    url = "https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:123"
    size = homebrew._fetch_bottle_size(url)

    assert size == 1048576
