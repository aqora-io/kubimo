import marimo
from starlette.applications import Starlette
from starlette.routing import Mount, Route
from starlette.responses import JSONResponse
from starlette.middleware.cors import CORSMiddleware


async def health(_):
    return JSONResponse({"status": "healthy"})


def build_app(
    directory: str,
    *,
    base_url: str = "/",
    include_code: bool = False,
    token: str | None = None,
    skew_protection: bool = False,
    allow_origins: list[str] = [],
):
    marimo_app = (
        marimo.create_asgi_app(
            quiet=True,
            include_code=include_code,
            token=token,
            skew_protection=skew_protection,
        )
        .with_dynamic_directory(path=base_url, directory=directory)
        .build()
    )
    health_base = base_url[:-1] if base_url.endswith("/") else base_url
    app = Starlette(
        routes=[Route(f"{health_base}/_health", health), Mount("/", marimo_app)]
    )
    app.add_middleware(
        CORSMiddleware,
        allow_origins=allow_origins or ["*"],
        allow_methods=["*"],
        allow_headers=["*"],
    )
    return app


if __name__ == "__main__":
    import argparse
    import uvicorn

    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", default=8000, type=int)
    parser.add_argument("--include-code", action="store_true")
    parser.add_argument("--token")
    parser.add_argument("--skew-protection", action="store_true")
    parser.add_argument("--base-url", default="/")
    parser.add_argument(
        "--allow-origins",
        action="append",
        default=[],
        help="Specify an allowed CORS origin (can be used multiple times).",
    )
    parser.add_argument("directory", nargs="?", default=".")
    args = parser.parse_args()

    uvicorn.run(
        build_app(
            args.directory,
            base_url=args.base_url,
            include_code=args.include_code,
            token=args.token,
            skew_protection=args.skew_protection,
            allow_origins=args.allow_origins,
        ),
        host=args.host,
        port=args.port,
    )
