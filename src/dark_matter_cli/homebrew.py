import json
import logging
import os
import platform
import re
import subprocess
import sys
import tempfile
from collections.abc import Sequence
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from typing import Any

import requests

logger = logging.getLogger("dark_matter_cli")

_BREW_NOT_FOUND = "Homebrew (`brew`) was not found on PATH. Is Homebrew installed?"


def _run_brew(args: list[str], failure: str) -> str:
    """Run a brew subcommand and return its stdout.

    Args:
        args: Arguments passed to the brew executable.
        failure: Message prefix used if the command exits non-zero.

    Returns:
        str: The command's standard output.

    Raises:
        RuntimeError: If brew is not installed or the command fails.
    """
    try:
        result = subprocess.run(
            ["brew", *args],
            capture_output=True,
            text=True,
            check=True,
        )
    except FileNotFoundError as e:
        raise RuntimeError(_BREW_NOT_FOUND) from e
    except subprocess.CalledProcessError as e:
        raise RuntimeError(f"{failure}: {e.stderr}") from e

    return result.stdout


def get_brew_prefix() -> Path:
    """Retrieve the local Homebrew installation prefix.

    Returns:
        Path: The absolute path to the Homebrew prefix (e.g., /opt/homebrew).

    Raises:
        RuntimeError: If the brew prefix command fails.
    """
    return Path(_run_brew(["--prefix"], "Failed to resolve Homebrew prefix").strip())


def get_brew_cache() -> Path:
    """Retrieve Homebrew's cache directory, honoring HOMEBREW_CACHE.

    Returns:
        Path: The cache directory (e.g., ~/Library/Caches/Homebrew on macOS,
            ~/.cache/Homebrew on Linux).

    Raises:
        RuntimeError: If the brew cache command fails.
    """
    return Path(_run_brew(["--cache"], "Failed to resolve Homebrew cache").strip())


def get_brew_metadata() -> dict[str, Any]:
    """Execute the brew info API to extract installed package metadata.

    Returns:
        dict[str, Any]: The parsed JSON payload containing formulae and casks.

    Raises:
        RuntimeError: If the brew info command fails or returns invalid JSON.
    """
    stdout = _run_brew(
        ["info", "--json=v2", "--installed"], "Failed to execute brew API"
    )
    try:
        data: dict[str, Any] = json.loads(stdout)
    except json.JSONDecodeError as e:
        raise RuntimeError("Failed to parse Homebrew JSON output.") from e
    return data


# Homebrew bottle tags by macOS major version.
_MACOS_CODENAMES = {
    11: "big_sur",
    12: "monterey",
    13: "ventura",
    14: "sonoma",
    15: "sequoia",
    26: "tahoe",
}


def detect_bottle_tag() -> str:
    """Infer the Homebrew bottle tag for the current machine.

    Unknown macOS releases map to the newest known codename at or below them,
    mirroring how Homebrew falls back to bottles built for older systems.

    Returns:
        str: A bottle tag such as 'arm64_tahoe', 'sequoia', or 'x86_64_linux'.
    """
    is_arm = platform.machine().lower() in {"arm64", "aarch64"}

    if sys.platform != "darwin":
        return "arm64_linux" if is_arm else "x86_64_linux"

    release = platform.mac_ver()[0]
    major = int(release.split(".")[0]) if release else max(_MACOS_CODENAMES)
    eligible = [v for v in _MACOS_CODENAMES if v <= major] or [min(_MACOS_CODENAMES)]
    codename = _MACOS_CODENAMES[max(eligible)]

    return f"arm64_{codename}" if is_arm else codename


def get_directory_size(path: Path) -> int:
    """Recursively calculate the aggregate physical byte size of a directory.

    Utilizes os.scandir for highly optimized filesystem traversal.

    Args:
        path: The directory path to calculate.

    Returns:
        int: The total size in bytes. Returns 0 if the path does not exist.
    """
    if not path.exists() or not path.is_dir():
        return 0

    total_size = 0
    try:
        with os.scandir(path) as it:
            for entry in it:
                if entry.is_file(follow_symlinks=False):
                    total_size += entry.stat().st_size
                elif entry.is_dir(follow_symlinks=False):
                    total_size += get_directory_size(Path(entry.path))
    except PermissionError:
        # Silently ignore unreadable directories to prevent pipeline termination
        pass

    return total_size


def _load_api_cache(json_path: Path, jws_path: Path) -> list[dict[str, Any]]:
    if jws_path.exists():
        with open(jws_path, encoding="utf-8") as f:
            jws_data = json.load(f)

        payload = jws_data.get("payload", "")

        if not isinstance(payload, str):
            logger.error("JWS payload is not a string!")
            return []

        try:
            data = json.loads(payload)
        except json.JSONDecodeError as e:
            logger.error(f"Failed to parse JWS payload: {e}")
            return []

        if not isinstance(data, list):
            logger.error("API payload is not a list.")
            return []

        return data

    if json_path.exists():
        with open(json_path, encoding="utf-8") as f:
            data = json.load(f)

        if not isinstance(data, list):
            logger.error("API cache is not a list.")
            return []

        return data

    return []


def get_theoretical_catalog(
    api_dir: Path | None = None,
) -> dict[str, list[dict[str, Any]]]:
    """Directly ingests Homebrew's local API JSON caches, bypassing the CLI entirely.

    Args:
        api_dir: The directory holding Homebrew's API cache. Defaults to the
            `api` folder inside `brew --cache`.

    Returns:
        dict: A dictionary containing 'formulae' and 'casks' lists.

    Raises:
        RuntimeError: If no API cache is present.
    """
    if api_dir is None:
        api_dir = get_brew_cache() / "api"

    formula_api_path = api_dir / "formula.json"
    formula_jws_path = api_dir / "formula.jws.json"

    if not formula_jws_path.exists() and not formula_api_path.exists():
        raise RuntimeError(
            f"Homebrew API cache not found in {api_dir}. Please run `brew update` first."
        )

    formulae = _load_api_cache(formula_api_path, formula_jws_path)
    casks = _load_api_cache(api_dir / "cask.json", api_dir / "cask.jws.json")

    return {"formulae": formulae, "casks": casks}


_GHCR_TOKEN_URL = "https://ghcr.io/token"  # noqa: S105
_BLOB_URL_PATTERN = re.compile(r"^https://ghcr\.io/v2/(?P<repo>.+)/blobs/sha256:")
_BOTTLE_SIZE_CACHE_PATH = Path.home() / ".cache" / "dark-matter" / "bottle_sizes.json"

# Homebrew publishes every formula as its own ghcr.io repository, so a token
# per bottle doubles the request count. Batching scopes keeps the token URL
# well under common length limits while cutting token requests ~50x.
_TOKEN_SCOPE_BATCH_SIZE = 50

# Bound the on-disk cache. Superseded bottle digests are never looked up
# again, so the oldest entries are evicted first.
_BOTTLE_SIZE_CACHE_MAX_ENTRIES = 50_000


def _parse_repository(url: str) -> str | None:
    """Extract the ghcr.io repository path from a bottle blob URL.

    Args:
        url: The full blob URL, e.g.
            'https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:...'.

    Returns:
        str | None: The repository path (e.g. 'homebrew/core/wget'), or None
            if the URL does not match the expected ghcr.io blob format.
    """
    match = _BLOB_URL_PATTERN.match(url)
    return match.group("repo") if match else None


def _fetch_ghcr_token(repositories: Sequence[str], timeout: float) -> str | None:
    """Request an anonymous pull token scoped to a batch of ghcr.io repositories.

    Args:
        repositories: The repository paths (e.g. ['homebrew/core/wget']).
        timeout: Per-request timeout in seconds.

    Returns:
        str | None: A bearer token, or None if the request failed.
    """
    params = [("service", "ghcr.io")] + [
        ("scope", f"repository:{repo}:pull") for repo in repositories
    ]
    try:
        resp = requests.get(_GHCR_TOKEN_URL, params=params, timeout=timeout)
        resp.raise_for_status()
        token = resp.json().get("token")
    except (requests.RequestException, ValueError):
        return None

    return token if isinstance(token, str) else None


def _fetch_blob_size(url: str, token: str, timeout: float) -> int | None:
    """Resolve the compressed size of an OCI blob via a HEAD request.

    Args:
        url: The full blob URL.
        token: A bearer token scoped to the blob's repository.
        timeout: Per-request timeout in seconds.

    Returns:
        int | None: The size in bytes, or None if the size could not be
            determined.
    """
    headers = {"Authorization": f"Bearer {token}"}
    try:
        resp = requests.head(
            url, headers=headers, timeout=timeout, allow_redirects=True
        )
        resp.raise_for_status()
        size = int(resp.headers["Content-Length"])
    except (requests.RequestException, KeyError, ValueError):
        return None

    return size if size > 0 else None


def load_bottle_size_cache() -> dict[str, int]:
    """Load the persisted sha256-to-size cache from disk.

    Non-positive sizes are discarded: earlier versions cached failed lookups
    as 0, and a real bottle is never empty.

    Returns:
        dict[str, int]: A mapping of bottle sha256 digests to byte sizes.
            Returns an empty dict if no cache exists or it cannot be parsed.
    """
    try:
        with open(_BOTTLE_SIZE_CACHE_PATH, encoding="utf-8") as f:
            data = json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}

    if not isinstance(data, dict):
        return {}

    return {
        digest: size
        for digest, size in data.items()
        if isinstance(size, int) and not isinstance(size, bool) and size > 0
    }


def save_bottle_size_cache(cache: dict[str, int]) -> None:
    """Persist the sha256-to-size cache to disk atomically.

    The cache is written to a temporary file and moved into place, so an
    interrupted or concurrent run never leaves a truncated file. Failures are
    logged rather than raised, since the cache is only an optimization.

    Args:
        cache: A mapping of bottle sha256 digests to byte sizes, in insertion
            order (oldest first).
    """
    entries = list(cache.items())[-_BOTTLE_SIZE_CACHE_MAX_ENTRIES:]
    tmp_path: Path | None = None
    try:
        _BOTTLE_SIZE_CACHE_PATH.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(
            "w",
            encoding="utf-8",
            dir=_BOTTLE_SIZE_CACHE_PATH.parent,
            prefix=f".{_BOTTLE_SIZE_CACHE_PATH.name}.",
            delete=False,
        ) as f:
            tmp_path = Path(f.name)
            json.dump(dict(entries), f)
        os.replace(tmp_path, _BOTTLE_SIZE_CACHE_PATH)
    except OSError as e:
        logger.warning(f"Could not write bottle size cache: {e}")
        if tmp_path is not None:
            tmp_path.unlink(missing_ok=True)


def resolve_bottle_sizes(
    bottles: dict[str, str],
    cache: dict[str, int],
    max_workers: int = 16,
    timeout: float = 5.0,
) -> dict[str, int]:
    """Resolve compressed bottle sizes for a batch of sha256-keyed blob URLs.

    Cached digests are returned immediately without touching the network.
    Uncached digests are resolved concurrently: one token request per batch of
    repositories, then one HEAD request per blob. `cache` is updated in place
    with successfully-resolved sizes only, so transient failures are retried
    on the next run instead of being persisted as 0.

    Args:
        bottles: A mapping of bottle sha256 digest to its blob URL.
        cache: The sha256-to-size cache, updated in place with new results.
        max_workers: The maximum number of concurrent network requests.
        timeout: Per-request timeout in seconds.

    Returns:
        dict[str, int]: A mapping of sha256 digest to resolved byte size,
            covering every digest present in `bottles`. Unresolvable digests
            map to 0.
    """
    resolved: dict[str, int] = {}
    pending: list[tuple[str, str, str]] = []

    for digest, url in bottles.items():
        if digest in cache:
            resolved[digest] = cache[digest]
            continue

        repository = _parse_repository(url)
        if repository is None:
            resolved[digest] = 0
        else:
            pending.append((digest, url, repository))

    if not pending:
        return resolved

    batches = [
        pending[i : i + _TOKEN_SCOPE_BATCH_SIZE]
        for i in range(0, len(pending), _TOKEN_SCOPE_BATCH_SIZE)
    ]

    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        tokens = list(
            executor.map(
                lambda batch: _fetch_ghcr_token(
                    sorted({repo for _, _, repo in batch}), timeout
                ),
                batches,
            )
        )

        future_to_digest = {}
        for batch, token in zip(batches, tokens, strict=True):
            for digest, url, _ in batch:
                if token is None:
                    resolved[digest] = 0
                else:
                    future = executor.submit(_fetch_blob_size, url, token, timeout)
                    future_to_digest[future] = digest

        for future in as_completed(future_to_digest):
            digest = future_to_digest[future]
            size = future.result()
            if size is None:
                resolved[digest] = 0
            else:
                resolved[digest] = size
                cache[digest] = size

    return resolved
