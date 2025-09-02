import marimo


def build_app(
    directory: str,
    *,
    path: str = "/",
    include_code: bool = False,
    token: str | None = None,
    skew_protection: bool = False,
):
    return (
        marimo.create_asgi_app(
            quiet=True,
            include_code=include_code,
            token=token,
            skew_protection=skew_protection,
        )
        .with_dynamic_directory(path=path, directory=directory)
        .build()
    )


if __name__ == "__main__":
    import argparse
    import uvicorn

    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", default=8000, type=int)
    parser.add_argument("--include-code", action="store_true")
    parser.add_argument("--token")
    parser.add_argument("--skew-protection", action="store_true")
    parser.add_argument("--path", default="/")
    parser.add_argument("directory", nargs="?", default=".")
    args = parser.parse_args()

    uvicorn.run(
        build_app(
            args.directory,
            path=args.path,
            include_code=args.include_code,
            token=args.token,
            skew_protection=args.skew_protection,
        ),
        host=args.host,
        port=args.port,
    )
