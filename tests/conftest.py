import pytest


@pytest.fixture
def mock_brew_metadata():
    """Simulates a minimal Homebrew API JSON payload."""
    return {
        "formulae": [
            {"name": "pkg-a", "dependencies": ["pkg-b", "pkg-c"]},
            {"name": "pkg-b", "dependencies": ["pkg-d"]},
            {"name": "pkg-c", "dependencies": ["pkg-d"]},
            {"name": "pkg-d", "dependencies": []},
        ],
        "casks": [],
    }


@pytest.fixture
def mock_brew_sizes():
    """Maps packages to mock physical byte sizes."""
    return {
        "pkg-a": 100,
        "pkg-b": 200,
        "pkg-c": 300,
        "pkg-d": 600,
    }
