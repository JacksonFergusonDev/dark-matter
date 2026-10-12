import logging

from rich.console import Console
from rich.logging import RichHandler


def get_logger(name: str, verbose: bool = False) -> logging.Logger:
    """Create and configure a logger for the application.

    Logs go to stderr so they never corrupt `export` output on stdout.

    Args:
        name: Logger name.
        verbose: Whether DEBUG logging should be enabled.

    Returns:
        A configured logger instance.
    """
    logger = logging.getLogger(name)
    logger.setLevel(logging.DEBUG if verbose else logging.WARNING)

    # Configure the handler once so repeated calls don't duplicate output.
    if not any(isinstance(h, RichHandler) for h in logger.handlers):
        handler = RichHandler(
            console=Console(stderr=True), show_time=False, show_path=False
        )
        logger.addHandler(handler)
        logger.propagate = False

    return logger
