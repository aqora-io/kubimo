import functools
import json
import logging
import re
import threading
from pathlib import Path, PurePosixPath

import marimo
from marimo._server.api.auth import (
    CookieSession,
    CustomSessionMiddleware,
    RANDOM_SECRET,
    TOKEN_QUERY_PARAM,
)

from starlette.applications import Starlette
from starlette.datastructures import MutableHeaders
from starlette.middleware import Middleware
from starlette.routing import Mount, Route
from starlette.requests import Request
from starlette.responses import HTMLResponse, JSONResponse
from starlette.types import ASGIApp, Message, Receive, Scope, Send
from starlette.middleware.cors import CORSMiddleware

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

_MARIMO_CODE_BLOCK = re.compile(r"(<marimo-code[^>]*>)(.*?)(</marimo-code>)", re.DOTALL)
_SHOW_APP_CODE_PATTERN = re.compile(r'("showAppCode"\s*:\s*)(true|false)')
_CODE_FIELD_PATTERN = re.compile(r'("code"\s*:\s*)"(?:(?:\\.)|[^"\\])*"')
_JSON_SCRIPT_ESCAPES = {
    ord(">"): "\\u003E",
    ord("<"): "\\u003C",
    ord("&"): "\\u0026",
}
_LOG_LEVEL_CHOICES = ["debug", "info", "warning", "error", "critical"]


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


def _app_path(request_path: str, base_url: str, directory: Path) -> Path | None:
    request = PurePosixPath("/") / request_path.strip("/")
    base = PurePosixPath("/") / base_url.strip("/")
    rel_path = request.relative_to(base)
    app_path = (Path(directory) / rel_path).with_suffix(".py")
    if not app_path.name.startswith("_") and app_path.exists():
        return app_path
    return None


def _cached_export_path(file_path: Path) -> Path:
    return file_path.parent / "__marimo__" / file_path.with_suffix(".html").name


def _resolve_cached_html_path(relative_path: str, directory: Path) -> Path | None:
    app_path = _app_path(relative_path, "/", directory)
    if not app_path:
        return None
    return _cached_export_path(app_path)


def _is_cached_request_authorized(request: Request, auth_token: str | None) -> bool:
    if not auth_token:
        return True
    try:
        cookie_session = CookieSession(request.session)
    except AssertionError as exc:
        logger.warning("Session middleware missing for cached auth: %s", exc)
        return False
    if cookie_session.get_access_token() == auth_token:
        return True
    if request.query_params.get(TOKEN_QUERY_PARAM) == auth_token:
        cookie_session.set_access_token(auth_token)
        return True
    return False


def _find_mount_config_bounds(html: str) -> tuple[int, int] | None:
    marker = "window.__MARIMO_MOUNT_CONFIG__ ="
    marker_index = html.find(marker)
    if marker_index == -1:
        return None
    start = html.find("{", marker_index)
    if start == -1:
        return None
    depth = 0
    in_string = False
    escape = False
    for index in range(start, len(html)):
        char = html[index]
        if in_string:
            if escape:
                escape = False
            elif char == "\\":
                escape = True
            elif char == '"':
                in_string = False
            continue
        if char == '"':
            in_string = True
            continue
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return start, index + 1
    return None


def _strip_trailing_commas(text: str) -> str:
    output: list[str] = []
    in_string = False
    escape = False
    for index, char in enumerate(text):
        if in_string:
            output.append(char)
            if escape:
                escape = False
            elif char == "\\":
                escape = True
            elif char == '"':
                in_string = False
            continue
        if char == '"':
            in_string = True
            output.append(char)
            continue
        if char == ",":
            next_index = index + 1
            while next_index < len(text) and text[next_index].isspace():
                next_index += 1
            if next_index < len(text) and text[next_index] in "}]":
                continue
        output.append(char)
    return "".join(output)


def _parse_mount_config(
    html: str,
) -> tuple[dict | None, tuple[int, int] | None]:
    bounds = _find_mount_config_bounds(html)
    if bounds is None:
        return None, None
    start, end = bounds
    config_text = _strip_trailing_commas(html[start:end])
    try:
        return json.loads(config_text), bounds
    except json.JSONDecodeError as exc:
        logger.warning("Failed to parse mount config: %s", exc)
        return None, None


def _json_script(data: object) -> str:
    return json.dumps(data, sort_keys=True).translate(_JSON_SCRIPT_ESCAPES)


def _strip_config_code(config: dict) -> None:
    if "code" in config:
        config["code"] = ""
    notebook = config.get("notebook")
    if isinstance(notebook, dict):
        cells = notebook.get("cells")
        if isinstance(cells, list):
            for cell in cells:
                if isinstance(cell, dict) and "code" in cell:
                    cell["code"] = ""
    session = config.get("session")
    if isinstance(session, dict):
        cells = session.get("cells")
        if isinstance(cells, list):
            for cell in cells:
                if isinstance(cell, dict) and "code" in cell:
                    cell["code"] = ""


def _apply_show_code(config: dict, include_code: bool, show_code: bool) -> None:
    view = config.get("view")
    if not isinstance(view, dict):
        view = {}
    view["showAppCode"] = bool(include_code and show_code)
    config["view"] = view
    if not include_code:
        _strip_config_code(config)


def _load_cached_mount_config(cache_path: Path) -> dict | None:
    try:
        html = cache_path.read_text(encoding="utf-8")
    except OSError as exc:
        logger.warning("Failed to read cached export %s: %s", cache_path, exc)
        return None
    config, _ = _parse_mount_config(html)
    return config


def _apply_cached_html(
    html: str,
    *,
    cached_config: dict | None,
    file_path: Path | None,
    include_code: bool,
    show_code: bool,
    include_session: bool,
) -> str:
    config, bounds = _parse_mount_config(html)
    if config is None or bounds is None:
        return html
    if include_session and isinstance(cached_config, dict):
        cached_session = cached_config.get("session")
        if cached_session is not None:
            config["session"] = cached_session
    notebook_snapshot = _resolve_notebook_snapshot(
        file_path, cached_config, include_code
    )
    if notebook_snapshot is not None:
        config["notebook"] = notebook_snapshot
    _apply_show_code(config, include_code, show_code)
    start, end = bounds
    config_text = _json_script(config)
    return f"{html[:start]}{config_text}{html[end:]}"


def _strip_cached_code(html: str) -> str:
    html = _MARIMO_CODE_BLOCK.sub(r"\1\3", html, count=1)
    bounds = _find_mount_config_bounds(html)
    if bounds is None:
        return html
    start, end = bounds
    config_text = html[start:end]
    config_text = _SHOW_APP_CODE_PATTERN.sub(r"\1false", config_text, count=1)
    config_text = _CODE_FIELD_PATTERN.sub(r'\1""', config_text)
    return f"{html[:start]}{config_text}{html[end:]}"


@functools.lru_cache(maxsize=128)
def _load_notebook_snapshot_with_code_cached(
    file_path: str, _mtime: float
) -> dict | None:
    try:
        from marimo._session.notebook import AppFileManager
        from marimo._utils.code import hash_code
        from marimo._version import __version__
    except Exception as exc:
        logger.warning("Notebook code loader unavailable: %s", exc)
        return None

    path = Path(file_path)
    try:
        app_manager = AppFileManager(path)
    except Exception as exc:
        logger.warning("Failed to load notebook %s: %s", path, exc)
        return None

    cells = []
    for cell in app_manager.app.cell_manager.cell_data():
        code = cell.code or ""
        code_hash = hash_code(code) if code else None
        cells.append(
            {
                "id": cell.cell_id,
                "code": code,
                "code_hash": code_hash,
                "name": cell.name,
                "config": cell.config.asdict(),
            }
        )

    return {
        "version": "1",
        "metadata": {"marimo_version": __version__},
        "cells": cells,
    }


def _load_notebook_snapshot_with_code(file_path: Path) -> dict | None:
    try:
        mtime = file_path.stat().st_mtime
    except OSError as exc:
        logger.warning("Failed to stat notebook %s: %s", file_path, exc)
        return None
    return _load_notebook_snapshot_with_code_cached(str(file_path), mtime)


def _resolve_notebook_snapshot(
    file_path: Path | None,
    cached_config: dict | None,
    include_code: bool,
) -> dict | None:
    if include_code and file_path is not None:
        notebook_snapshot = _load_notebook_snapshot_with_code(file_path)
        if notebook_snapshot is not None:
            return notebook_snapshot
    if isinstance(cached_config, dict):
        notebook = cached_config.get("notebook")
        if isinstance(notebook, dict):
            return notebook
    return None


def _seed_session_view_from_cached_export(session, file_path: Path) -> bool:
    cache_path = _cached_export_path(file_path)
    if not cache_path.exists():
        return False
    cached_config = _load_cached_mount_config(cache_path)
    if not isinstance(cached_config, dict):
        return False
    session_snapshot = cached_config.get("session")
    if not isinstance(session_snapshot, dict):
        return False
    try:
        app = session.app_file_manager.app
        cell_data = tuple(app.cell_manager.cell_data())
        if not cell_data:
            return False
        from marimo._session.state.serialize import deserialize_session
        from marimo._utils.code import hash_code
    except Exception as exc:
        logger.warning("Failed to prepare cached session for %s: %s", file_path, exc)
        return False

    code_hash_to_cell_id: dict[str, str] = {}
    for cell in cell_data:
        if cell.code:
            code_hash_to_cell_id[hash_code(cell.code)] = cell.cell_id
    try:
        session_view = deserialize_session(session_snapshot, code_hash_to_cell_id)
    except Exception as exc:
        logger.warning("Failed to load cached session for %s: %s", file_path, exc)
        return False

    session.session_view = session_view
    return True


_CACHED_SESSION_SEEDER_INSTALLED = False


def _install_cached_session_seeder() -> None:
    global _CACHED_SESSION_SEEDER_INSTALLED
    if _CACHED_SESSION_SEEDER_INSTALLED:
        return
    try:
        from marimo._server.session_manager import SessionManager
    except Exception as exc:
        logger.warning("Failed to install cached session seeder: %s", exc)
        return

    original_create_session = SessionManager.create_session

    @functools.wraps(original_create_session)
    def create_session_with_cache(
        self, session_id, session_consumer, query_params, file_key, auto_instantiate
    ):
        session = original_create_session(
            self,
            session_id=session_id,
            session_consumer=session_consumer,
            query_params=query_params,
            file_key=file_key,
            auto_instantiate=auto_instantiate,
        )
        try:
            file_path = Path(file_key)
        except TypeError:
            return session
        if file_path.is_file() and session.session_view.is_empty():
            _seed_session_view_from_cached_export(session, file_path)
        return session

    SessionManager.create_session = create_session_with_cache
    _CACHED_SESSION_SEEDER_INSTALLED = True


class CachedSnapshotInjector:
    def __init__(
        self,
        app: ASGIApp,
        *,
        directory: str,
        base_url: str,
        include_code: bool,
        auth_token: str | None,
    ) -> None:
        self.app = app
        self.directory = Path(directory)
        self.base_url = base_url
        self.include_code = include_code
        self.auth_token = auth_token

    async def __call__(self, scope: Scope, receive: Receive, send: Send):
        if scope["type"] != "http":
            return await self.app(scope, receive, send)
        if scope["method"] != "GET":
            return await self.app(scope, receive, send)

        request = Request(scope, receive)
        request_path = scope["path"]
        file_path = _app_path(request_path, self.base_url, self.directory)
        cached_config = None
        if file_path is not None:
            cache_path = _cached_export_path(file_path)
            if cache_path.exists() and _is_cached_request_authorized(
                request, self.auth_token
            ):
                cached_config = _load_cached_mount_config(cache_path)
        show_code = request.query_params.get("show-code") == "true"

        response_start: Message | None = None
        body_chunks: list[bytes] = []

        async def send_wrapper(message: Message) -> None:
            nonlocal response_start, body_chunks
            if message["type"] == "http.response.start":
                response_start = message
                return
            if message["type"] == "http.response.body":
                body_chunks.append(message.get("body", b""))
                if message.get("more_body", False):
                    return
                body = b"".join(body_chunks)
                if response_start is None:
                    await send(message)
                    return
                headers = MutableHeaders(scope=response_start)
                content_type = headers.get("content-type", "")
                if "text/html" in content_type and cached_config is not None:
                    try:
                        html = body.decode("utf-8")
                    except UnicodeDecodeError:
                        pass
                    else:
                        html = _apply_cached_html(
                            html,
                            cached_config=cached_config,
                            file_path=file_path,
                            include_code=self.include_code,
                            show_code=show_code,
                            include_session=True,
                        )
                        body = html.encode("utf-8")
                        headers["content-length"] = str(len(body))
                        headers["cache-control"] = "no-store"
                await send(response_start)
                await send(
                    {
                        "type": "http.response.body",
                        "body": body,
                        "more_body": False,
                    }
                )
                return
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
    debug_cached: bool = False,
):
    _install_cached_session_seeder()
    auth_token = token or ""
    auth_enabled = bool(auth_token)

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
    root_url = base_url.rstrip("/")
    marimo_app_with_cache = CachedSnapshotInjector(
        marimo_app,
        directory=directory,
        base_url=base_url,
        include_code=include_code,
        auth_token=auth_token,
    )

    async def cached_export(request):
        if not _is_cached_request_authorized(request, auth_token):
            return HTMLResponse("Unauthorized", status_code=401)
        relative_path = request.path_params.get("path", "")
        file_path = _app_path(relative_path, "/", Path(directory))
        cache_path = _resolve_cached_html_path(relative_path, Path(directory))
        if cache_path is None or not cache_path.exists():
            return HTMLResponse("Cached export not found.", status_code=404)
        html = cache_path.read_text(encoding="utf-8")
        show_code = request.query_params.get("show-code") == "true"
        if not include_code:
            html = _strip_cached_code(html)
        else:
            html = _apply_cached_html(
                html,
                cached_config=None,
                file_path=file_path,
                include_code=include_code,
                show_code=show_code,
                include_session=False,
            )
        response = HTMLResponse(html)
        response.headers["Cache-Control"] = "no-store"
        return response

    middleware = []
    if auth_enabled:
        middleware.append(Middleware(CustomSessionMiddleware, secret_key=RANDOM_SECRET))
    middleware.append(Middleware(ActiveConnectionsMiddleware))
    routes = [
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
        Mount("/", marimo_app_with_cache),
    ]
    if debug_cached:
        routes.insert(-1, Route(root_url + "/_cached/{path:path}", cached_export))

    app = Starlette(
        routes=routes,
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
    parser.add_argument(
        "--debug-cached",
        action="store_true",
        help="Expose the cached HTML route for debugging.",
    )
    parser.add_argument("--token-password")
    parser.add_argument("--no-token", action="store_true")
    parser.add_argument("--skew-protection", action="store_true")
    parser.add_argument("--base-url", default="/")
    parser.add_argument(
        "--log-level",
        default="info",
        choices=_LOG_LEVEL_CHOICES,
        help="Log level.",
    )
    parser.add_argument(
        "--allow-origins",
        action="append",
        default=[],
        help="Specify an allowed CORS origin (can be used multiple times).",
    )
    parser.add_argument("directory", nargs="?", default=".")
    args = parser.parse_args()

    logging.getLogger().setLevel(args.log_level.upper())

    if args.token_password and args.no_token:
        raise ValueError("Cannot specify both --token and --no-token")
    if not args.token_password and not args.no_token:
        logger.warning("No token specified")

    uvicorn.run(
        build_app(
            args.directory,
            base_url=args.base_url,
            include_code=args.include_code,
            token=args.token_password,
            skew_protection=args.skew_protection,
            allow_origins=args.allow_origins,
            debug_cached=args.debug_cached,
        ),
        host=args.host,
        port=args.port,
        log_level=args.log_level,
    )
