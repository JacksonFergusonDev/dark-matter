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

- **Core size** — the package's own on-disk footprint (or, in theoretical mode, its compressed bottle archive).
- **Weighted recursive size** — the core size plus a *fair share* of every transitive dependency, where each shared dependency's cost is divided evenly across all the packages that depend on it.

The ratio between the two — the **Bloat Ratio** — is the headline number. A low ratio means a package is mostly self-contained; a high ratio means most of its footprint belongs to shared infrastructure it happens to require.

## Features

- **Comprehensive analysis suite**
  - `analyze` — measures what's actually on disk, using `brew info --json=v2 --installed` and direct filesystem traversal (`os.scandir`) for exact byte counts. Casks are measured where they are installed (e.g. `/Applications/Foo.app`), not just their `Caskroom` staging directory.
  - `leaderboard` — a theoretical mode that ranks Homebrew's *entire* formula and cask catalog from the local API cache, without requiring anything to be installed.
  - `inspect` & `compare` — targeted analysis of individual or grouped packages. In theoretical mode, only the targets' dependency closure is resolved over the network rather than the entire catalog.
  - `explain` — breaks down a target package's bloat by attributing fractional byte costs to each of its transitive dependencies.
  - `export` — streams the underlying DataFrames to CSV or JSON for integration into external data pipelines.
- **Fractional Attribution Model** — shared dependencies (`openssl`, `python`, etc.) are divided proportionally across all parent packages instead of being double-counted, giving an honest per-package cost.
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

# Break down the dependency bloat of a specific package
dark-matter explain uv

# Compare multiple packages side-by-side
dark-matter compare uv poetry pdm

# Export the entire graph to JSON for external analysis
dark-matter export --format json > homebrew_bloat.json
```

All commands accept the global `--verbose` / `-v` flag for debug logging (written to stderr), and `--version` to print the installed version.

When `inspect`, `compare`, or `explain` are pointed at packages that aren't installed, Dark Matter offers to fall back to the theoretical catalog.

Theoretical modes read the catalog Homebrew caches for the current platform, so run `brew update` first if it's missing. `--arch` defaults to this machine's bottle tag (e.g. `arm64_tahoe`, `x86_64_linux`).

### `analyze`

| Flag | Default | Description |
| --- | --- | --- |
| `--sort` / `-s` | `ratio` | Sort by `ratio`, `core`, or `recursive` |
| `--top` / `-n` | `20` | Number of packages to display (at least 1) |
| `--fractional` / `--standard` | `--fractional` | Toggle the Fractional Attribution Model |

### `leaderboard`

| Flag | Default | Description |
| --- | --- | --- |
| `--sort` / `-s` | `ratio` | Sort by `ratio`, `core`, or `recursive` |
| `--top` / `-n` | `20` | Number of packages to display (at least 1) |
| `--arch` / `-a` | detected | Target bottle tag |

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
