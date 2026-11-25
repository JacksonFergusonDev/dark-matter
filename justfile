set shell := ["bash", "-euc", "-o", "pipefail"]
set unstable
set quiet

# --- ANSI Colors ---

blue := '\033[1;34m'
green := '\033[1;32m'
nc := '\033[0m'

# Show available commands
default:
    @just --list

# Auto-format Rust and Markdown
format:
    @printf "\n{{ blue }}=== Formatting Code ==={{ nc }}\n"
    cargo fmt --all
    rumdl fmt .
    @printf "{{ green }}✔ Formatting complete{{ nc }}\n"

# Run linters and check formatting (Clippy, rustfmt, and Markdown)
lint:
    @printf "\n{{ blue }}=== Running Linters ==={{ nc }}\n"
    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings
    rumdl check .
    rumdl fmt --check .
    @printf "{{ green }}✔ Linting passed{{ nc }}\n"

# Run the full automated test suite
test:
    @printf "\n{{ blue }}=== Running Tests ==={{ nc }}\n"
    cargo test --all-targets
    @printf "{{ green }}✔ All tests passed{{ nc }}\n"

# Run tests with LLVM coverage (requires cargo-llvm-cov)
test-cov:
    @printf "\n{{ blue }}=== Running Tests with Coverage ==={{ nc }}\n"
    cargo llvm-cov --all-targets --fail-under-lines 85
    @printf "{{ green }}✔ Coverage run complete{{ nc }}\n"

# Run the live Homebrew smoke test
smoke:
    @printf "\n{{ blue }}=== Running Homebrew Smoke Test ==={{ nc }}\n"
    cargo test --test smoke -- --ignored
    @printf "{{ green }}✔ Smoke test passed{{ nc }}\n"

# Build an optimized release binary
build:
    @printf "\n{{ blue }}=== Building Release Binary ==={{ nc }}\n"
    cargo build --release
    @printf "{{ green }}✔ Build complete{{ nc }}\n"

# Run the exact pipeline executed by GitHub Actions
ci: lint test
    @printf "\n{{ green }}✔ Local CI pipeline completed successfully. Clear to push!{{ nc }}\n"

# Preview the release notes for unreleased commits
changelog:
    uvx git-cliff@2.14.2 --unreleased --strip all

# Bump project version (part: major, minor, patch), sync lockfile, commit, tag, and atomic push
bump part:
    uv run --refresh https://raw.githubusercontent.com/JacksonFergusonDev/ci-cd-release-infrastructure/refs/heads/main/scripts/release.py {{ part }}

# Remove build artifacts and temporary files
clean:
    @printf "\n{{ blue }}=== Cleaning Workspace ==={{ nc }}\n"
    cargo clean
    rm -rf \
        dist \
        target \
        coverage.lcov \
        lcov.info \
        *.profraw
    @printf "{{ green }}✔ Workspace cleaned{{ nc }}\n"
