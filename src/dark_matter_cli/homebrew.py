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
from dataclasses import dataclass, field
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
    logger.debug(f"Running: brew {' '.join(args)}")
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
                # Skip individual unreadable or vanishing entries without
                # discarding the rest of the directory.
                try:
                    if entry.is_file(follow_symlinks=False):
                        total_size += entry.stat(follow_symlinks=False).st_size
                    elif entry.is_dir(follow_symlinks=False):
                        total_size += get_directory_size(Path(entry.path))
                except OSError:
                    continue
    except OSError:
        # Silently ignore unreadable directories to prevent pipeline termination
        pass

    return total_size


def get_path_size(path: Path) -> int:
    """Calculate the physical byte size of a file or directory.

    Symlinks count as 0: cask `binary` and completion artifacts are links into
    the app bundle or Caskroom, which are measured separately.

    Args:
        path: The file or directory to measure.

    Returns:
        int: The total size in bytes, or 0 if it is missing or a symlink.
    """
    if path.is_symlink():
        return 0
    if path.is_dir():
        return get_directory_size(path)
    try:
        return path.stat().st_size if path.is_file() else 0
    except OSError:
        return 0


@dataclass(frozen=True)
class KegSizes:
    """A formula's footprint in the Cellar, split by keg.

    Attributes:
        current: Bytes in the keg Homebrew currently uses.
        stale: Bytes in each older keg `brew cleanup` would remove, by version.
            Older kegs that `brew cleanup` would keep are in neither field.
    """

    current: int
    stale: dict[str, int] = field(default_factory=dict)


def _current_keg(formula: dict[str, Any], prefix: Path) -> str | None:
    """Identify the keg (version directory) Homebrew currently uses.

    The `opt/<name>` link always points at the current keg, keg-only formulae
    included. Without it, fall back to the linked keg, then the newest
    installed version Homebrew reports.
    """
    name: str = formula["name"]
    opt = prefix / "opt" / name
    if opt.is_symlink():
        target = opt.resolve()
        if target.parent.name == name:
            return target.name

    if formula.get("linked_keg"):
        return str(formula["linked_keg"])

    installed = formula.get("installed") or []
    version = installed[-1].get("version") if installed else None
    return str(version) if version else None


def _latest_keg(formula: dict[str, Any]) -> str | None:
    """The keg name of the formula's newest stable version, e.g. '1.2_1'."""
    stable = (formula.get("versions") or {}).get("stable")
    if not stable:
        return None
    revision = formula.get("revision") or 0
    return f"{stable}_{revision}" if revision else str(stable)


def _keg_link_target(link: Path) -> str | None:
    """The keg name a Homebrew bookkeeping symlink points at, if any."""
    return link.resolve().name if link.is_symlink() else None


def _has_live_keepme_refs(keg: Path) -> bool:
    """Whether a keg's `.keepme` file names a path that still exists."""
    try:
        refs = (keg / ".keepme").read_text(encoding="utf-8").splitlines()
    except OSError:
        return False
    return any(ref.strip() and Path(ref.strip()).exists() for ref in refs)


def _cleanup_candidates(
    formula: dict[str, Any], prefix: Path, kegs: list[Path], current: str
) -> list[Path]:
    """The older kegs `brew cleanup` would actually remove.

    Mirrors Homebrew's `Formula#eligible_kegs_for_cleanup`: a formula is
    skipped until its newest version is installed, and a linked keg, the
    pinned keg, or a keg still referenced by a `.keepme` file is never
    removed. HEAD installs are skipped, since ordering them needs Homebrew's
    version logic; erring this way never overstates what cleanup frees.
    """
    names = {k.name for k in kegs}
    latest = _latest_keg(formula)
    if latest not in names or any(n.startswith("HEAD") for n in names):
        return []

    keep = {
        current,
        latest,
        formula.get("linked_keg"),
        _keg_link_target(prefix / "var" / "homebrew" / "pinned" / formula["name"]),
    }
    return [k for k in kegs if k.name not in keep and not _has_live_keepme_refs(k)]


def get_keg_sizes(formula: dict[str, Any], prefix: Path) -> KegSizes:
    """Measure a formula's current keg separately from any older kegs.

    When the current keg can't be identified, the whole Cellar directory is
    treated as current so nothing is reported as removable by mistake. Older
    kegs count as stale only if `brew cleanup` would remove them.

    Args:
        formula: A formula entry from `brew info --json=v2 --installed`.
        prefix: The Homebrew installation prefix.

    Returns:
        KegSizes: The current keg's size and each stale keg's size.
    """
    rack = prefix / "Cellar" / formula["name"]
    current = _current_keg(formula, prefix)
    try:
        kegs = sorted(p for p in rack.iterdir() if p.is_dir() and not p.is_symlink())
    except OSError:
        kegs = []

    if current is None or current not in {k.name for k in kegs}:
        return KegSizes(current=get_directory_size(rack))

    return KegSizes(
        current=get_directory_size(rack / current),
        stale={
            k.name: get_directory_size(k)
            for k in _cleanup_candidates(formula, prefix, kegs, current)
        },
    )


# Where casks install apps when Homebrew does not report a resolved target.
_DEFAULT_APPDIR = Path("/Applications")
_APP_ARTIFACT_KINDS = ("app", "suite")


def _cask_artifact_paths(cask: dict[str, Any]) -> list[Path]:
    """List the on-disk locations a cask's artifacts were installed to.

    Homebrew 7+ reports a resolved `target` for each installed artifact. Older
    releases do not, so app bundles are assumed to live in /Applications
    (honoring a per-artifact `target` override). Artifacts installed by `pkg`
    installers have no recorded location and cannot be measured.

    Args:
        cask: A cask entry from `brew info --json=v2 --installed`.

    Returns:
        list[Path]: Unique artifact paths, in declaration order.
    """
    paths: list[Path] = []
    for artifact in cask.get("artifacts", []):
        if not isinstance(artifact, dict):
            continue

        target = artifact.get("target")
        if isinstance(target, str):
            paths.append(Path(target).expanduser())
            continue

        for kind in _APP_ARTIFACT_KINDS:
            spec = artifact.get(kind)
            if not spec or not isinstance(spec[0], str):
                continue
            options = spec[1] if len(spec) > 1 and isinstance(spec[1], dict) else {}
            name = options.get("target", Path(spec[0]).name)
            paths.append(_DEFAULT_APPDIR / Path(name).expanduser())

    return list(dict.fromkeys(paths))


def get_cask_size(cask: dict[str, Any], prefix: Path) -> int:
    """Calculate the installed footprint of a cask.

    The Caskroom only holds staging metadata; the bulk of a cask lives in its
    installed artifacts (e.g. /Applications/Foo.app, ~/Library/Fonts/*.otf).

    Args:
        cask: A cask entry from `brew info --json=v2 --installed`.
        prefix: The Homebrew installation prefix.

    Returns:
        int: The Caskroom size plus the size of every installed artifact.
    """
    total = get_directory_size(prefix / "Caskroom" / cask["token"])
    return total + sum(get_path_size(path) for path in _cask_artifact_paths(cask))


def _read_jws_payload(jws_path: Path) -> Any:
    """Decode the JSON payload embedded in a Homebrew JWS cache file.

    Args:
        jws_path: Path to a `*.jws.json` file.

    Returns:
        Any: The decoded payload, or None if the file or payload is malformed.
    """
    try:
        with open(jws_path, encoding="utf-8") as f:
            jws_data = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        logger.error(f"Failed to read {jws_path}: {e}")
        return None

    payload = jws_data.get("payload") if isinstance(jws_data, dict) else None
    if not isinstance(payload, str):
        logger.error(f"JWS payload in {jws_path} is not a string.")
        return None

    try:
        return json.loads(payload)
    except json.JSONDecodeError as e:
        logger.error(f"Failed to parse JWS payload: {e}")
        return None


def _load_api_cache(json_path: Path, jws_path: Path) -> list[dict[str, Any]]:
    """Load a legacy (pre-Homebrew 7) formula or cask API cache."""
    if jws_path.exists():
        data = _read_jws_payload(jws_path)
    elif json_path.exists():
        try:
            with open(json_path, encoding="utf-8") as f:
                data = json.load(f)
        except (OSError, json.JSONDecodeError) as e:
            logger.error(f"Failed to read {json_path}: {e}")
            return []
    else:
        return []

    if not isinstance(data, list):
        logger.error("API payload is not a list.")
        return []

    return data


_GHCR_CORE_BLOB_URL = "https://ghcr.io/v2/homebrew/core/{image}/blobs/sha256:{digest}"

# Dependency tags that `brew info` leaves out of a formula's runtime
# `dependencies` list.
_NON_RUNTIME_DEPENDENCY_TAGS = {":build", ":test", ":optional"}


def _runtime_dependencies(entries: list[Any]) -> list[str]:
    """Extract runtime dependency names from Homebrew 7 dependency entries.

    Entries are either a bare name or a `{name: tag}` mapping, where the tag
    is a string or list such as ':build'.

    Args:
        entries: The raw dependency entries.

    Returns:
        list[str]: Names of dependencies required at runtime.
    """
    deps: list[str] = []
    for entry in entries:
        if isinstance(entry, str):
            deps.append(entry)
        elif isinstance(entry, dict):
            for name, tags in entry.items():
                tag_set = {tags} if isinstance(tags, str) else set(tags)
                if not tag_set & _NON_RUNTIME_DEPENDENCY_TAGS:
                    deps.append(name)
    return deps


def _ghcr_image_name(formula: str) -> str:
    """Map a formula name to its ghcr.io image name, as Homebrew does."""
    return formula.replace("@", "/").replace("+", "x")


def _normalize_internal_formula(
    name: str, formula: dict[str, Any], tag: str
) -> dict[str, Any]:
    """Convert a Homebrew 7 formula entry into the legacy `brew info` shape.

    Args:
        name: The formula name.
        formula: The raw entry from the internal packages payload.
        tag: The bottle tag the payload was generated for.

    Returns:
        dict[str, Any]: An entry with `name`, `dependencies`, and (when a
            bottle exists) `bottle.stable.files`.
    """
    deps = _runtime_dependencies(formula.get("stable_dependencies", []))

    # macOS ships these libraries; on Linux they are real dependencies.
    if tag.endswith("_linux"):
        deps += _runtime_dependencies(
            [spec[0] for spec in formula.get("stable_uses_from_macos", []) if spec]
        )

    entry: dict[str, Any] = {"name": name, "dependencies": deps}

    checksum = formula.get("bottle_checksum")
    if checksum:
        # `bottle_tag` is only present when it differs from the payload's tag.
        bottle_tag = str(formula.get("bottle_tag", tag)).removeprefix(":")
        url = _GHCR_CORE_BLOB_URL.format(image=_ghcr_image_name(name), digest=checksum)
        entry["bottle"] = {
            "stable": {"files": {bottle_tag: {"sha256": checksum, "url": url}}}
        }

    return entry


def _normalize_internal_cask(token: str, cask: dict[str, Any]) -> dict[str, Any]:
    """Convert a Homebrew 7 cask entry into the legacy `brew info` shape.

    Args:
        token: The cask token.
        cask: The raw entry from the internal packages payload.

    Returns:
        dict[str, Any]: An entry with `token`, `name`, and `depends_on`.
    """
    formulae = cask.get("depends_on_args", {}).get(":formula", [])
    if isinstance(formulae, str):
        formulae = [formulae]

    return {
        "token": token,
        "name": cask.get("names", []),
        "depends_on": {"formula": list(formulae)} if formulae else {},
    }


def _load_internal_catalog(jws_path: Path, tag: str) -> dict[str, list[dict[str, Any]]]:
    """Load Homebrew 7's single-file `packages.<tag>.jws.json` catalog.

    Args:
        jws_path: Path to the internal packages JWS file.
        tag: The bottle tag the file was generated for.

    Returns:
        dict: A dictionary containing 'formulae' and 'casks' lists in the
            legacy shape.

    Raises:
        RuntimeError: If the file cannot be parsed.
    """
    data = _read_jws_payload(jws_path)
    if not isinstance(data, dict):
        raise RuntimeError(f"Failed to parse Homebrew API cache at {jws_path}.")

    formulae = data.get("formulae", {})
    casks = data.get("casks", {})

    return {
        "formulae": [
            _normalize_internal_formula(name, f, tag) for name, f in formulae.items()
        ],
        "casks": [_normalize_internal_cask(token, c) for token, c in casks.items()],
    }


def get_theoretical_catalog(
    api_dir: Path | None = None, tag: str | None = None
) -> dict[str, list[dict[str, Any]]]:
    """Directly ingests Homebrew's local API JSON caches, bypassing the CLI entirely.

    Homebrew 7+ stores a single per-platform `internal/packages.<tag>.jws.json`.
    Older releases store `formula.jws.json` and `cask.jws.json` covering every
    platform; these are used when the internal file is absent.

    Args:
        api_dir: The directory holding Homebrew's API cache. Defaults to the
            `api` folder inside `brew --cache`.
        tag: The bottle tag to load. Defaults to this machine's.

    Returns:
        dict: A dictionary containing 'formulae' and 'casks' lists.

    Raises:
        RuntimeError: If no usable API cache is present.
    """
    if api_dir is None:
        api_dir = get_brew_cache() / "api"
    tag = tag or detect_bottle_tag()

    internal_dir = api_dir / "internal"
    internal_path = internal_dir / f"packages.{tag}.jws.json"
    if internal_path.exists():
        logger.debug(f"Loading Homebrew 7 catalog from {internal_path}")
        return _load_internal_catalog(internal_path, tag)

    formula_api_path = api_dir / "formula.json"
    formula_jws_path = api_dir / "formula.jws.json"
    if formula_jws_path.exists() or formula_api_path.exists():
        logger.debug(f"Loading legacy catalog from {api_dir}")
        return {
            "formulae": _load_api_cache(formula_api_path, formula_jws_path),
            "casks": _load_api_cache(api_dir / "cask.json", api_dir / "cask.jws.json"),
        }

    cached_tags = sorted(
        p.name.removeprefix("packages.").removesuffix(".jws.json")
        for p in internal_dir.glob("packages.*.jws.json")
    )
    if cached_tags:
        raise RuntimeError(
            f"No Homebrew catalog cached for '{tag}' (available: "
            f"{', '.join(cached_tags)}). Homebrew only caches the current "
            "platform's catalog; omit --arch or pass a cached tag."
        )

    raise RuntimeError(
        f"Homebrew API cache not found in {api_dir}. Please run `brew update` first."
    )


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

    logger.debug(
        f"Bottle sizes: {len(bottles) - len(pending)} cached or unresolvable, "
        f"{len(pending)} to fetch from ghcr.io"
    )
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

    failed = sum(1 for digest, _, _ in pending if digest not in cache)
    if failed:
        logger.debug(f"Failed to resolve {failed} bottle size(s); they count as 0")

    return resolved
