import json
import logging
import os
import re
import subprocess
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from typing import Any

import requests

logger = logging.getLogger("dark_matter")


def get_brew_prefix() -> Path:
    """Retrieve the local Homebrew installation prefix.

    Returns:
        Path: The absolute path to the Homebrew prefix (e.g., /opt/homebrew).

    Raises:
        RuntimeError: If the brew prefix command fails.
    """
    try:
        result = subprocess.run(
            ["brew", "--prefix"],
            capture_output=True,
            text=True,
            check=True,
        )
        return Path(result.stdout.strip())
    except subprocess.CalledProcessError as e:
        raise RuntimeError(f"Failed to resolve Homebrew prefix: {e.stderr}") from e


def get_brew_metadata() -> dict[str, Any]:
    """Execute the brew info API to extract installed package metadata.

    Returns:
        dict[str, Any]: The parsed JSON payload containing formulae and casks.

    Raises:
        RuntimeError: If the brew info command fails or returns invalid JSON.
    """
    try:
        result = subprocess.run(
            ["brew", "info", "--json=v2", "--installed"],
            capture_output=True,
            text=True,
            check=True,
        )
        data: dict[str, Any] = json.loads(result.stdout)
        return data
    except subprocess.CalledProcessError as e:
        raise RuntimeError(f"Failed to execute brew API: {e.stderr}") from e
    except json.JSONDecodeError as e:
        raise RuntimeError("Failed to parse Homebrew JSON output.") from e


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


def get_theoretical_catalog(prefix: Path) -> dict[str, list[dict[str, Any]]]:
    """Directly ingests Homebrew's local API JSON caches, bypassing the CLI entirely.

    Args:
        prefix: The base physical installation path for Homebrew.

    Returns:
        dict: A dictionary containing 'formulae' and 'casks' lists.
    """
    # Standard location where Homebrew mirrors its online API cache locally
    cache_dir = Path.home() / "Library/Caches/Homebrew/api"

    formula_api_path = cache_dir / "formula.json"
    formula_jws_path = cache_dir / "formula.jws.json"

    cask_api_path = cache_dir / "cask.json"
    cask_jws_path = cache_dir / "cask.jws.json"

    # Fallback to internal cellar var paths if user settings vary
    if not formula_jws_path.exists() and not formula_api_path.exists():
        formula_api_path = prefix / "var/homebrew/api/formula.json"
        formula_jws_path = prefix / "var/homebrew/api/formula.jws.json"
        cask_api_path = prefix / "var/homebrew/api/cask.json"
        cask_jws_path = prefix / "var/homebrew/api/cask.jws.json"

    if not formula_jws_path.exists() and not formula_api_path.exists():
        raise RuntimeError(
            "Local Homebrew API cache not found. Please run `brew update` first."
        )

    formulae = _load_api_cache(formula_api_path, formula_jws_path)
    casks = _load_api_cache(cask_api_path, cask_jws_path)

    return {"formulae": formulae, "casks": casks}


_GHCR_TOKEN_URL = "https://ghcr.io/token"
_BLOB_URL_PATTERN = re.compile(r"^https://ghcr\.io/v2/(?P<repo>.+)/blobs/sha256:")
_BOTTLE_SIZE_CACHE_PATH = Path.home() / ".cache" / "dark-matter" / "bottle_sizes.json"


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


def _fetch_ghcr_token(repository: str, timeout: float) -> str | None:
    """Request an anonymous pull token scoped to a single ghcr.io repository.

    Args:
        repository: The repository path (e.g. 'homebrew/core/wget').
        timeout: Per-request timeout in seconds.

    Returns:
        str | None: A bearer token, or None if the request failed.
    """
    params = {"service": "ghcr.io", "scope": f"repository:{repository}:pull"}
    try:
        resp = requests.get(_GHCR_TOKEN_URL, params=params, timeout=timeout)
        resp.raise_for_status()
        token = resp.json().get("token")
    except requests.RequestException:
        return None

    return token if isinstance(token, str) else None


def _fetch_blob_size(url: str, token: str, timeout: float) -> int:
    """Resolve the compressed size of an OCI blob via a HEAD request.

    Args:
        url: The full blob URL.
        token: A bearer token scoped to the blob's repository.
        timeout: Per-request timeout in seconds.

    Returns:
        int: The size in bytes, or 0 if the size could not be determined.
    """
    headers = {"Authorization": f"Bearer {token}"}
    try:
        resp = requests.head(
            url, headers=headers, timeout=timeout, allow_redirects=True
        )
        resp.raise_for_status()
        return int(resp.headers.get("Content-Length", "0"))
    except (requests.RequestException, ValueError):
        return 0


def _fetch_bottle_size(url: str, timeout: float = 5.0) -> int:
    """Resolve a single bottle's compressed byte size from ghcr.io.

    Args:
        url: The full blob URL for the bottle.
        timeout: Per-request timeout in seconds.

    Returns:
        int: The size in bytes, or 0 if any stage of the lookup failed.
    """
    repository = _parse_repository(url)
    if repository is None:
        return 0

    token = _fetch_ghcr_token(repository, timeout)
    if token is None:
        return 0

    return _fetch_blob_size(url, token, timeout)


def load_bottle_size_cache() -> dict[str, int]:
    """Load the persisted sha256-to-size cache from disk.

    Returns:
        dict[str, int]: A mapping of bottle sha256 digests to byte sizes.
            Returns an empty dict if no cache exists or it cannot be parsed.
    """
    if not _BOTTLE_SIZE_CACHE_PATH.exists():
        return {}

    try:
        with open(_BOTTLE_SIZE_CACHE_PATH, encoding="utf-8") as f:
            data: dict[str, int] = json.load(f)
        return data
    except (OSError, json.JSONDecodeError):
        return {}


def save_bottle_size_cache(cache: dict[str, int]) -> None:
    """Persist the sha256-to-size cache to disk.

    Args:
        cache: A mapping of bottle sha256 digests to byte sizes.
    """
    _BOTTLE_SIZE_CACHE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with open(_BOTTLE_SIZE_CACHE_PATH, "w", encoding="utf-8") as f:
        json.dump(cache, f)


def resolve_bottle_sizes(
    bottles: dict[str, str],
    cache: dict[str, int],
    max_workers: int = 16,
    timeout: float = 5.0,
) -> dict[str, int]:
    """Resolve compressed bottle sizes for a batch of sha256-keyed blob URLs.

    Cached digests are returned immediately without touching the network.
    Uncached digests are resolved concurrently via ghcr.io HEAD requests, and
    `cache` is updated in place with any newly-resolved sizes so the caller
    can persist it afterward.

    Args:
        bottles: A mapping of bottle sha256 digest to its blob URL.
        cache: The sha256-to-size cache, updated in place with new results.
        max_workers: The maximum number of concurrent network requests.
        timeout: Per-request timeout in seconds.

    Returns:
        dict[str, int]: A mapping of sha256 digest to resolved byte size,
            covering every digest present in `bottles`.
    """
    resolved: dict[str, int] = {}
    pending: dict[str, str] = {}

    for digest, url in bottles.items():
        if digest in cache:
            resolved[digest] = cache[digest]
        else:
            pending[digest] = url

    if not pending:
        return resolved

    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        future_to_digest = {
            executor.submit(_fetch_bottle_size, url, timeout): digest
            for digest, url in pending.items()
        }
        for future in as_completed(future_to_digest):
            digest = future_to_digest[future]
            size = future.result()
            resolved[digest] = size
            cache[digest] = size

    return resolved
