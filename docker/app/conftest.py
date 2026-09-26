def pytest_configure(config):
    config.addinivalue_line(
        "markers", "image: needs the built marimo image (pytest -m image)"
    )
