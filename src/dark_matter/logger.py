import logging
import sys


def get_logger(name: str, verbose: bool = False) -> logging.Logger:
    """Create and configure a logger for the application.

    Args:
        name: Logger name.
        verbose: Whether DEBUG logging should be enabled.

    Returns:
        A configured logger instance.
    """
    logger = logging.getLogger(name)
    logger.setLevel(logging.DEBUG if verbose else logging.INFO)
    handler = logging.StreamHandler(sys.stderr)
    formatter = logging.Formatter("%(levelname)s: %(message)s")
    handler.setFormatter(formatter)
    logger.addHandler(handler)
    return logger
