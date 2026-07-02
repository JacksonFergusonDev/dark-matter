"""A programmatic pipeline for recursive storage profiling."""

import contextlib
import importlib.metadata

__version__ = "unknown"
with contextlib.suppress(importlib.metadata.PackageNotFoundError):
    __version__ = importlib.metadata.version("dark-matter")

from .core import (
    build_analysis_dataframe,
    build_compare_analysis_dataframe,
    build_compare_theoretical_dataframe,
    build_explain_analysis_dataframe,
    build_explain_theoretical_dataframe,
    build_targeted_analysis_dataframe,
    build_targeted_theoretical_dataframe,
    build_theoretical_dataframe,
)
from .homebrew import get_brew_metadata, get_brew_prefix, get_theoretical_catalog

__all__ = [
    "__version__",
    "build_analysis_dataframe",
    "build_compare_analysis_dataframe",
    "build_compare_theoretical_dataframe",
    "build_explain_analysis_dataframe",
    "build_explain_theoretical_dataframe",
    "build_targeted_analysis_dataframe",
    "build_targeted_theoretical_dataframe",
    "build_theoretical_dataframe",
    "get_brew_metadata",
    "get_brew_prefix",
    "get_theoretical_catalog",
]
