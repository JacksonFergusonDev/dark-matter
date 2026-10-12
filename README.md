<!-- rumdl-disable-file first-line-heading -->
<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/readme-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="assets/readme-light.svg">
  <img alt="Dark Matter Logo"
       src="assets/readme-light.svg"
       width="450"
       style="max-width:100%; height:auto;">
</picture>

**Dependency-graph-aware storage analysis for Homebrew.**

[![PyPI Version](https://img.shields.io/pypi/v/dark-matter-cli?style=flat-square&color=94a3b8&labelColor=0A0A0A&logo=pypi&logoColor=94a3b8)](https://pypi.org/project/dark-matter-cli/)
[![CI](https://img.shields.io/github/actions/workflow/status/JacksonFergusonDev/dark-matter/ci.yml?style=flat-square&color=94a3b8&labelColor=0A0A0A&label=CI)](https://github.com/JacksonFergusonDev/dark-matter/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/actions/workflow/status/JacksonFergusonDev/dark-matter/release.yml?style=flat-square&color=94a3b8&labelColor=0A0A0A&label=Release)](https://github.com/JacksonFergusonDev/dark-matter/actions/workflows/release.yml)
[![Python](https://img.shields.io/badge/python-3.12+-94a3b8?style=flat-square&labelColor=0A0A0A)](https://www.python.org/downloads/)
[![Ruff](https://img.shields.io/badge/style-ruff-94a3b8?style=flat-square&labelColor=0A0A0A)](https://github.com/astral-sh/ruff)
[![Mypy](https://img.shields.io/badge/mypy-checked-94a3b8?style=flat-square&labelColor=0A0A0A)](https://mypy-lang.org/)
[![prek](https://img.shields.io/badge/prek-enabled-94a3b8?style=flat-square&labelColor=0A0A0A)](https://github.com/j178/prek)
[![License](https://img.shields.io/badge/license-MIT-94a3b8?style=flat-square&labelColor=0A0A0A)](LICENSE)

</div>

## Why

Homebrew flattens every dependency into a single `Cellar` directory. Tools like `du` or `ncdu` can tell you a formula takes up 500 MB, but they have no concept of *why* — whether that mass belongs to the formula itself or to a shared runtime pulled in by five other packages you installed for unrelated reasons.

Dark Matter reconstructs the dependency graph Homebrew already knows about and uses it to answer a more useful question: for each package you explicitly installed, how much disk space does it actually cost you, once shared dependencies are fairly split across everything that depends on them?

## How it works

Dark Matter parses Homebrew's own JSON metadata (via `brew info --json=v2` or its local API cache, including Homebrew 7's per-platform format), rebuilds the dependency DAG, and walks it to compute two figures per package:

- **Core size** — the package's own on-disk footprint (or, in theoretical mode, its compressed bottle archive). For formulae this is the keg Homebrew currently uses; older versions waiting for `brew cleanup` are reported by `cleanup` instead.
- **Weighted recursive size** — the core size plus a *fair share* of every transitive dependency, where each shared dependency's cost is divided evenly across the root packages that need it.

The ratio between the two — the **Bloat Ratio** — is the headline number. A low ratio means a package is mostly self-contained; a high ratio means most of its footprint belongs to shared infrastructure it happens to require. Packages with no measurable core size (meta-packages, for example) have no meaningful ratio and show `—`.

### The attribution model

Costs are attributed to **roots**: the packages you asked for (formulae installed on request, and every cask), plus any package nothing else depends on, such as an orphaned dependency. In catalog mode nothing is "requested", so roots are the formulae no other formula depends on.

- A root owns its own core size in full. If you requested `python` and something else also depends on it, that dependent isn't charged for `python` — you'd have it either way.
- Every other package is split evenly across the roots whose dependency closure contains it. Intermediate dependencies never take a share.

Dependencies come from each formula's definition plus the runtime dependencies Homebrew recorded when the keg was installed, which is what `brew autoremove` relies on. A keg keeps linking against what it was built with even after the formula drops a dependency.

`analyze` and `leaderboard` list only roots by default, because a dependency's size is already counted in the roots that share it. That can hide large packages: in the catalog, `texlive` (about 2 GB) has a single dependent, so it shows up as that package's weighted size. Pass `--all` to list dependencies too; their weighted sizes overlap their roots', so don't add them up.

Every package is reachable from at least one root, so the shares always add up to what's on disk:

$$\sum_{r \in \text{roots}} W(r) = \sum_{p} m_{core}(p)$$

Run with `--verbose` to see this check for your own installation.

## Features

- **Comprehensive analysis suite**
  - `analyze` — measures what's actually on disk, using `brew info --json=v2 --installed` and direct filesystem traversal (`os.scandir`) for exact byte counts. Casks are measured where they are installed (e.g. `/Applications/Foo.app`), not just their `Caskroom` staging directory.
  - `leaderboard` — a theoretical mode that ranks Homebrew's *entire* formula catalog from the local API cache, without requiring anything to be installed. Casks ship no bottles, so they can only be measured once installed, with `analyze`.
  - `inspect` & `compare` — targeted analysis of individual or grouped packages. In theoretical mode, only the targets' dependency closure is resolved over the network rather than the entire catalog.
  - `explain` — breaks down a target package's bloat by attributing fractional byte costs to each of its transitive dependencies.
  - `export` — streams the underlying DataFrames to CSV or JSON for integration into external data pipelines.
- **Cleanup planning**
  - `reclaim` — how much space `brew uninstall <packages>` followed by `brew autoremove` would free, and which uninstalls Homebrew would refuse because something still needs the package.
  - `orphans` — dependencies nothing you requested still needs: what `brew autoremove` would remove, with sizes.
  - `cleanup` — old versions still sitting in the Cellar that `brew cleanup` would remove. Like Homebrew, it leaves alone formulae whose newest version isn't installed yet, and pinned or linked versions.
  - `diff` — compares two `export` snapshots to show what grew, shrank, appeared, or disappeared, e.g. after a `brew upgrade`.
- **Fractional Attribution Model** — shared dependencies (`openssl`, `zlib`, etc.) are divided evenly across the root packages that need them instead of being double-counted, and the shares add up to exactly the bytes measured. See [the attribution model](#the-attribution-model).
- **Daemon-free** — no background indexing, no persistent database. Every run is a fresh, on-demand computation.
- **Typed and tested** — fully type-annotated (strict `mypy`), linted with `ruff`, and covered by a `pytest` suite exercising the DAG traversal, fractional math, and network resolution logic. CI runs the full suite on macOS and Ubuntu across Python 3.12–3.14 and enforces a coverage floor.

## A note on theoretical measurements

`leaderboard`, `inspect`, `compare`, and `explain` rely on `Content-Length` headers from `ghcr.io` blob storage, which report *compressed* archive size, not the size a package occupies once unpacked to disk. The absolute numbers they report will therefore run lower than `analyze`'s physical measurements.

The Bloat Ratio, however, stays meaningful. Since most bottles compress with similar algorithms (gzip or zstd), the compression factor $c$ appears in both the numerator and denominator and cancels out:

$$R \approx \frac{c \cdot m_{recursive}}{c \cdot m_{core}} \approx \frac{m_{recursive}}{m_{core}}$$

So while theoretical modes shouldn't be read as precise disk-space forecasts, they are a reliable way to evaluate relative bloat without installing anything.

## Installation

With [Homebrew](https://brew.sh):

```bash
brew install jacksonfergusondev/tap/dark-matter
```

Or from [PyPI](https://pypi.org/project/dark-matter-cli/) with [`uv`](https://docs.astral.sh/uv/):

```bash
uv tool install dark-matter-cli
```

For development:

```bash
git clone https://github.com/jacksonfergusondev/dark-matter.git
cd dark-matter
uv tool install --editable .
```

## Usage

```bash
# Analyze what's actually installed
dark-matter analyze

# Rank the entire Homebrew catalog by theoretical bloat
dark-matter leaderboard

# Evaluate a specific formula instantly
dark-matter inspect uv

# Break down the dependency bloat of a specific package, as a table or a tree
dark-matter explain uv
dark-matter explain uv --tree

# Compare multiple packages side-by-side
dark-matter compare uv poetry pdm

# Export the entire graph to JSON for external analysis
dark-matter export --format json > homebrew_bloat.json

# See what removing packages would free, and what's already removable
dark-matter reclaim ffmpeg imagemagick
dark-matter orphans
dark-matter cleanup

# Snapshot today, then compare after an upgrade
dark-matter export --format json > snapshot-$(date +%F).json
dark-matter diff snapshot-2026-10-01.json snapshot-2026-10-12.json
```

Exports include every package. Filter on `Is_Root` to get the rows whose `Weighted_Bytes` sum to the total; `Is_Requested` marks the packages you installed on purpose. `Bloat_Ratio` is empty (CSV) or `null` (JSON) when a package has no core size.

All commands accept the global `--verbose` / `-v` flag for debug logging (written to stderr), and `--version` to print the installed version.

When `inspect`, `compare`, or `explain` are pointed at packages that aren't installed, Dark Matter offers to fall back to the theoretical catalog.

Theoretical modes read the catalog Homebrew caches for the current platform, so run `brew update` first if it's missing. Formulae without a bottle for the platform count as 0, and so do bottles whose size couldn't be fetched from ghcr.io; Dark Matter warns when that happens, and the next run retries them. `--arch` defaults to this machine's bottle tag (e.g. `arm64_tahoe`, `x86_64_linux`).

### `analyze`

| Flag | Default | Description |
| --- | --- | --- |
| `--sort` / `-s` | `ratio` | Sort by `ratio`, `core`, or `recursive` |
| `--top` / `-n` | `20` | Number of packages to display (at least 1) |
| `--fractional` / `--standard` | `--fractional` | Toggle the Fractional Attribution Model |
| `--all` / `-A` | off | Also list dependencies, marked `· dependency` |

### `leaderboard`

| Flag | Default | Description |
| --- | --- | --- |
| `--sort` / `-s` | `ratio` | Sort by `ratio`, `core`, or `recursive` |
| `--top` / `-n` | `20` | Number of packages to display (at least 1) |
| `--arch` / `-a` | detected | Target bottle tag |
| `--all` / `-A` | off | Also list dependencies, marked `· dependency` |

### `inspect`

| Argument/Flag | Default | Description |
| --- | --- | --- |
| `[PACKAGE]` | **Required** | The target package to analyze |
| `--source` / `-s` | `installed` | Data source to compute: `installed` or `catalog` |
| `--arch` / `-a` | detected | Target bottle tag |

### `compare`

| Argument/Flag | Default | Description |
| --- | --- | --- |
| `[PACKAGES]...` | **Required** | A space-separated list of packages to compare |
| `--sort` / `-s` | `ratio` | Sort by `ratio`, `core`, or `recursive` |
| `--source` | `installed` | Data source to compute: `installed` or `catalog` |
| `--arch` / `-a` | detected | Target bottle tag |

### `explain`

| Argument/Flag | Default | Description |
| --- | --- | --- |
| `[PACKAGE]` | **Required** | The specific package to analyze |
| `--source` / `-s` | `installed` | Data source to compute: `installed` or `catalog` |
| `--arch` / `-a` | detected | Target bottle tag |
| `--tree` / `-t` | off | Show the dependency tree instead of a table |

### `reclaim`

| Argument | Default | Description |
| --- | --- | --- |
| `[PACKAGES]...` | **Required** | The installed packages to (hypothetically) uninstall |

Only bytes the packages are responsible for are counted: existing orphans are left to `orphans`.

### `orphans` and `cleanup`

Neither takes options. Both measure the local installation only.

### `diff`

| Argument/Flag | Default | Description |
| --- | --- | --- |
| `[OLD]` | **Required** | The earlier `export` snapshot (`.csv`, or JSON otherwise) |
| `[NEW]` | **Required** | The later `export` snapshot |
| `--top` / `-n` | `20` | Number of packages to display (at least 1) |

### `export`

| Flag | Default | Description |
| --- | --- | --- |
| `--source` / `-s` | `installed` | Data source to compute: `installed` or `catalog` |
| `--format` / `-f` | `csv` | Output format: `csv` or `json` |
| `--arch` / `-a` | detected | Target bottle tag (for `catalog` source) |

## Development

The project uses [`just`](https://github.com/casey/just) to wrap common tasks:

```bash
just format       # ruff format + fix
just lint         # ruff + rumdl
just typecheck    # mypy
just test         # pytest
just test-cov     # pytest with coverage report
just ci           # the full pipeline CI runs, locally
```

## Contact

[![GitHub](https://img.shields.io/badge/GitHub-100000?style=for-the-badge&logo=github&logoColor=white)](https://github.com/JacksonFergusonDev)
[![LinkedIn](https://img.shields.io/badge/LinkedIn-0077B5?style=for-the-badge&logo=linkedin&logoColor=white)](https://www.linkedin.com/in/jackson--ferguson/)
[![Email](https://img.shields.io/badge/Email-D14836?style=for-the-badge&logo=gmail&logoColor=white)](mailto:jackson.ferguson0@gmail.com)

[![Website](https://raw.githubusercontent.com/JacksonFergusonDev/JacksonFergusonDev.github.io/refs/heads/main/.github/assets/badge.svg)](https://jacksonferguson.me)

## 📄 License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
