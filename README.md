# Dark Matter

[![CI](https://img.shields.io/github/actions/workflow/status/JacksonFergusonDev/dark-matter/ci.yml?style=flat-square&color=white&labelColor=0A0A0A&label=CI)](https://github.com/JacksonFergusonDev/dark-matter/actions/workflows/ci.yml)
[![Python](https://img.shields.io/badge/python-3.12+-white?style=flat-square&labelColor=0A0A0A)](https://www.python.org/downloads/)
[![Ruff](https://img.shields.io/badge/style-ruff-white?style=flat-square&labelColor=0A0A0A)](https://github.com/astral-sh/ruff)
[![Mypy](https://img.shields.io/badge/mypy-checked-white?style=flat-square&labelColor=0A0A0A)](https://mypy-lang.org/)
[![pre-commit](https://img.shields.io/badge/pre--commit-enabled-white?style=flat-square&labelColor=0A0A0A)](https://github.com/pre-commit/pre-commit)
[![License](https://img.shields.io/badge/license-MIT-white?style=flat-square&labelColor=0A0A0A)](LICENSE)

**A dependency-graph-aware storage profiler for Homebrew.**

## Why

Homebrew flattens every dependency into a single `Cellar` directory. Tools like `du` or `ncdu` can tell you a formula takes up 500 MB, but they have no concept of *why* — whether that mass belongs to the formula itself or to a shared runtime pulled in by five other packages you installed for unrelated reasons.

Dark Matter reconstructs the dependency graph Homebrew already knows about and uses it to answer a more useful question: for each package you explicitly installed, how much disk space does it actually cost you, once shared dependencies are fairly split across everything that depends on them?

## How it works

Dark Matter parses Homebrew's own JSON metadata (via `brew info --json=v2` or its local API cache), rebuilds the dependency DAG, and walks it to compute two figures per package:

- **Core size** — the package's own on-disk footprint (or, in theoretical mode, its compressed bottle archive).
- **Weighted recursive size** — the core size plus a *fair share* of every transitive dependency, where each shared dependency's cost is divided evenly across all the packages that depend on it.

The ratio between the two — the **Bloat Ratio** — is the headline number. A low ratio means a package is mostly self-contained; a high ratio means most of its footprint belongs to shared infrastructure it happens to require.

## Features

- **Two analysis modes**
  - `analyze` — measures what's actually on disk, using `brew info --json=v2 --installed` and direct filesystem traversal (`os.scandir`) for exact byte counts.
  - `leaderboard` — a theoretical mode that ranks Homebrew's *entire* formula and cask catalog from the local API cache, without requiring anything to be installed. Bottle sizes are resolved by issuing concurrent, cached `HEAD` requests against `ghcr.io`'s OCI blob storage, so nothing is downloaded.
- **Fractional Attribution Model** — shared dependencies (`openssl`, `python`, etc.) are divided proportionally across all parent packages instead of being double-counted, giving an honest per-package cost.
- **Daemon-free** — no background indexing, no persistent database. Every run is a fresh, on-demand computation.
- **Typed and tested** — fully type-annotated (strict `mypy`), linted with `ruff`, and covered by a `pytest` suite exercising the DAG traversal, fractional math, and network resolution logic. CI runs the full suite on macOS and Ubuntu across Python 3.12 and 3.14.

## A note on theoretical measurements

`leaderboard` relies on `Content-Length` headers from `ghcr.io` blob storage, which report *compressed* archive size, not the size a package occupies once unpacked to disk. The absolute numbers it reports will therefore run lower than `analyze`'s physical measurements.

The Bloat Ratio, however, stays meaningful. Since most bottles compress with similar algorithms (gzip or zstd), the compression factor $c$ appears in both the numerator and denominator and cancels out:

$$R \approx \frac{c \cdot m_{recursive}}{c \cdot m_{core}} \approx \frac{m_{recursive}}{m_{core}}$$

So while `leaderboard` shouldn't be read as a precise disk-space forecast, it's a reliable way to rank the entire ecosystem by relative bloat without installing anything.

## Installation

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
```

### `analyze`

| Flag | Default | Description |
| --- | --- | --- |
| `--sort` / `-s` | `ratio` | Sort by `ratio`, `core`, or `recursive` |
| `--top` / `-n` | `20` | Number of packages to display |
| `--fractional` / `--standard` | `--fractional` | Toggle the Fractional Attribution Model |

**Example:**

```bash
dark-matter analyze --sort recursive --top 15
```

### `leaderboard`

| Flag | Default | Description |
| --- | --- | --- |
| `--sort` / `-s` | `ratio` | Sort by `ratio`, `core`, or `recursive` |
| `--top` / `-n` | `20` | Number of packages to display |
| `--arch` / `-a` | `arm64_tahoe` | Target bottle architecture |

Both commands accept the global `--verbose` / `-v` flag for debug logging, and `--version` to print the installed version.

## Development

The project uses [`just`](https://github.com/casey/just) to wrap common tasks:

```bash
just format       # ruff format + fix
just lint         # ruff + markdownlint
just typecheck    # mypy
just test         # pytest
just test-cov     # pytest with coverage report
just ci           # the full pipeline CI runs, locally
```

## 📧 Contact

[![GitHub](https://img.shields.io/badge/GitHub-100000?style=for-the-badge&logo=github&logoColor=white)](https://github.com/JacksonFergusonDev)
[![LinkedIn](https://img.shields.io/badge/LinkedIn-0077B5?style=for-the-badge&logo=linkedin&logoColor=white)](https://www.linkedin.com/in/jackson--ferguson/)
[![Email](https://img.shields.io/badge/Email-D14836?style=for-the-badge&logo=gmail&logoColor=white)](mailto:jackson.ferguson0@gmail.com)

## 📄 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
