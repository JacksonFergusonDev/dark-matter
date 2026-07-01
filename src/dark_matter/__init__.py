"""A programmatic pipeline for recursive storage profiling."""

import contextlib
import importlib.metadata

__version__ = "unknown"
with contextlib.suppress(importlib.metadata.PackageNotFoundError):
    __version__ = importlib.metadata.version("dark-matter")

from .core import build_analysis_dataframe, build_theoretical_dataframe
from .homebrew import get_brew_metadata, get_theoretical_catalog

__all__ = [
    "build_analysis_dataframe",
    "build_theoretical_dataframe",
    "get_brew_metadata",
    "get_theoretical_catalog",
]
