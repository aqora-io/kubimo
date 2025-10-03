import os
import threading

import marimo
from starlette.applications import Starlette
from starlette.middleware import Middleware
from starlette.routing import Mount, Route
from starlette.responses import JSONResponse
from starlette.types import ASGIApp, Message, Receive, Scope, Send
from starlette.middleware.cors import CORSMiddleware

script_dir = os.path.dirname(os.path.abspath(__file__))
script_js_path = os.path.join(script_dir, "script.js")
with open(script_js_path, "rb") as f:
    script_js_bytes = f.read()


class AtomicInteger:
    def __init__(self, value: int = 0):
        self._value = value
        self._lock = threading.Lock()

    def inc(self, d: int = 1):
        with self._lock:
            self._value += int(d)
            return self._value

    def dec(self, d: int = 1):
        return self.inc(-d)

    @property
    def value(self):
        with self._lock:
            return self._value

    @value.setter
    def value(self, v):
        with self._lock:
            self._value = int(v)
            return self._value


active_connections = AtomicInteger()


class ActiveConnectionsMiddleware:
    def __init__(self, app: ASGIApp):
        self.app = app

    async def __call__(self, scope: Scope, receive: Receive, send: Send):
        if scope["type"] != "websocket":
            return await self.app(scope, receive, send)

        async def wrapped_receive() -> Message:
            message = await receive()
            if message["type"] == "websocket.connect":
                active_connections.inc()
            if message["type"] == "websocket.disconnect":
                active_connections.dec()
            return message

        await self.app(scope, wrapped_receive, send)


class InjectScriptMiddleware:
    def __init__(self, app: ASGIApp):
        self.app = app
        self.content_type = None

    async def __call__(self, scope: Scope, receive: Receive, send: Send):
        if scope["type"] != "http":
            await self.app(scope, receive, send)
            return

        async def send_wrapper(message: Message):
            if message["type"] == "http.response.start":
                headers = []
                for k, v in message["headers"]:
                    if k.lower() == b"content-type":
                        self.content_type = v
                    # Drop content-length for now; we'll fix it later
                    if k.lower() != b"content-length":
                        headers.append((k, v))

                message["headers"] = headers
                await send(message)

            elif message["type"] == "http.response.body":
                if self.content_type and b"text/html" in self.content_type:
                    message["body"] = message["body"].replace(
                        b"<head>",
                        b"<head><script>" + script_js_bytes + b"</script>",
                        1,
                    )

                await send(message)

        await self.app(scope, receive, send_wrapper)


async def connections(_):
    return JSONResponse({"active": active_connections.value})


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
    middleware = [
        Middleware(ActiveConnectionsMiddleware),
        Middleware(InjectScriptMiddleware),
    ]
    root_url = base_url.rstrip("/")
    app = Starlette(
        routes=[
            Route(f"{root_url}/_health", health),
            Mount(
                f"{root_url}/_api",
                routes=[
                    Mount(
                        "/status",
                        routes=[
                            Route("/connections", connections),
                        ],
                    ),
                ],
            ),
            Mount("/", marimo_app),
        ],
        middleware=middleware,
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
