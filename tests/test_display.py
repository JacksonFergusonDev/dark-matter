from dark_matter import display


def test_format_bytes():
    """Verify scale shifting across byte magnitudes."""
    assert display.format_bytes(500) == "500 B"
    assert display.format_bytes(1024) == "1.0 KB"
    assert display.format_bytes(1024**2 * 2.5) == "2.5 MB"
    assert display.format_bytes(1024**3 * 4.123) == "4.12 GB"
