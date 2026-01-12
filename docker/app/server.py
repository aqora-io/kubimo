import json
import logging
import re
import threading
from pathlib import Path

import marimo
from marimo._session.model import SessionMode
from marimo._session.types import KernelState

from starlette.applications import Starlette
from starlette.datastructures import QueryParams
from starlette.middleware import Middleware
from starlette.routing import Mount, Route
from starlette.responses import HTMLResponse, JSONResponse
from starlette.types import ASGIApp, Message, Receive, Scope, Send
from starlette.middleware.cors import CORSMiddleware

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

_AUTOSWITCH_SCRIPT_PATH = Path(__file__).resolve().parent / "marimo_autoswitch.js"
_WS_REPLAY_PATCHED = False
_MARIMO_CODE_BLOCK = re.compile(r"(<marimo-code[^>]*>)(.*?)(</marimo-code>)", re.DOTALL)
_SHOW_APP_CODE_PATTERN = re.compile(r'("showAppCode"\s*:\s*)(true|false)')
_CODE_FIELD_PATTERN = re.compile(r'("code"\s*:\s*)"(?:(?:\\.)|[^"\\])*"')


def _patch_run_mode_reconnect() -> None:
    global _WS_REPLAY_PATCHED
    if _WS_REPLAY_PATCHED:
        return
    try:
        from marimo._server.api.endpoints.ws_endpoint import WebSocketHandler
    except Exception as exc:
        logger.warning("Failed to patch marimo reconnect behavior: %s", exc)
        return

    original = WebSocketHandler._reconnect_session

    def patched(self, session, replay: bool) -> None:
        force_replay = self.websocket.query_params.get("force_replay") == "1"
        if self.mode == SessionMode.RUN and force_replay:
            replay = True
        return original(self, session, replay)

    WebSocketHandler._reconnect_session = patched
    _WS_REPLAY_PATCHED = True


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


def _strip_base_url(request_path: str, base_url: str) -> str | None:
    base = base_url.rstrip("/")
    if not base:
        return request_path.lstrip("/")
    if not request_path.startswith(f"{base}/"):
        return None
    return request_path[len(base) + 1 :]


def _find_app_file(directory: Path, relative_path: str) -> tuple[Path, str] | None:
    if not relative_path or Path(relative_path).suffix:
        return None

    direct_match = directory / f"{relative_path}.py"
    if direct_match.exists() and not direct_match.name.startswith("_"):
        return direct_match, ""

    parts = relative_path.split("/")
    for i in range(len(parts), 0, -1):
        prefix = parts[:i]
        remaining = parts[i:]
        candidate = directory.joinpath(*prefix).with_suffix(".py")
        if candidate.exists() and not candidate.name.startswith("_"):
            return candidate, "/".join(remaining)
    return None


def _resolve_app_file(request_path: str, base_url: str, directory: Path) -> Path | None:
    relative_path = _strip_base_url(request_path, base_url)
    if relative_path is None:
        return None
    relative_path = relative_path.lstrip("/").rstrip("/")
    if not relative_path or relative_path.startswith("_"):
        return None
    match = _find_app_file(directory, relative_path)
    if not match:
        return None
    file_path, remaining = match
    if remaining:
        return None
    return file_path


def _cached_export_path(file_path: Path) -> Path:
    return file_path.parent / "__marimo__" / file_path.with_suffix(".html").name


def _get_dynamic_directory(app: ASGIApp) -> ASGIApp | None:
    if hasattr(app, "_app_cache") and hasattr(app, "directory"):
        return app
    return None


def _get_session(marimo_app: ASGIApp, file_path: Path, session_id: str | None = None):
    dynamic = _get_dynamic_directory(marimo_app)
    if dynamic is None:
        return None
    cache_key = str(file_path)
    try:
        cached_app = dynamic._app_cache.get(cache_key)
    except Exception:
        return None
    if cached_app is None:
        return None
    session_manager = getattr(
        getattr(cached_app, "state", None), "session_manager", None
    )
    if session_manager is None:
        return None
    if session_id:
        session = session_manager.get_session(session_id)
        if session is None:
            return None
        if session.app_file_manager.path != str(file_path.absolute()):
            return None
    else:
        session = session_manager.get_session_by_file_key(str(file_path.absolute()))
    return session


def _is_run_complete(session) -> bool:
    cell_ids = list(session.app_file_manager.app.cell_manager.cell_ids())
    if not cell_ids:
        return True
    for cell_id in cell_ids:
        cell_notification = session.session_view.cell_notifications.get(cell_id)
        if cell_notification is None or cell_notification.status is None:
            return False
        if cell_notification.status in ("queued", "running"):
            return False
    return True


def _session_cell_statuses(session) -> dict[str, str | None]:
    cell_ids = list(session.app_file_manager.app.cell_manager.cell_ids())
    statuses: dict[str, str | None] = {}
    for cell_id in cell_ids:
        cell_notification = session.session_view.cell_notifications.get(cell_id)
        statuses[str(cell_id)] = cell_notification.status if cell_notification else None
    return statuses


def _load_autoswitch_script(
    ready_path: str, skip_param: str, include_code: bool
) -> str | None:
    try:
        template = _AUTOSWITCH_SCRIPT_PATH.read_text(encoding="utf-8")
    except FileNotFoundError:
        logger.warning("Auto-switch script missing: %s", _AUTOSWITCH_SCRIPT_PATH)
        return None
    return (
        template.replace("__READY_PATH__", json.dumps(ready_path))
        .replace("__SKIP_PARAM__", json.dumps(skip_param))
        .replace("__INCLUDE_CODE__", json.dumps(include_code))
        .strip()
    )


def _inject_auto_switch(html: str, script: str) -> str:
    script_tag = f"\n<script>\n{script}\n</script>\n"
    marker = "</body>"
    if marker in html:
        return html.replace(marker, f"{script_tag}{marker}", 1)
    return f"{html}{script_tag}"


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


class CachedExportFallback:
    def __init__(
        self,
        app: ASGIApp,
        *,
        directory: str,
        base_url: str,
        ready_path: str,
        include_code: bool,
    ) -> None:
        self.app = app
        self.directory = Path(directory)
        self.base_url = base_url
        self.ready_path = ready_path
        self.include_code = include_code
        self.skip_param = "__marimo_skip_cache"

    async def __call__(self, scope: Scope, receive: Receive, send: Send):
        if scope["type"] != "http":
            return await self.app(scope, receive, send)
        if scope["method"] != "GET":
            return await self.app(scope, receive, send)

        query = QueryParams(scope.get("query_string", b"").decode())
        if query.get(self.skip_param) is not None:
            return await self.app(scope, receive, send)

        request_path = scope["path"]
        file_path = _resolve_app_file(request_path, self.base_url, self.directory)
        if file_path is None:
            return await self.app(scope, receive, send)

        cache_path = _cached_export_path(file_path)
        if not cache_path.exists():
            return await self.app(scope, receive, send)

        html = cache_path.read_text(encoding="utf-8")
        if not self.include_code:
            html = _strip_cached_code(html)
        script = _load_autoswitch_script(
            ready_path=self.ready_path,
            skip_param=self.skip_param,
            include_code=self.include_code,
        )
        if script:
            html = _inject_auto_switch(html, script)
        response = HTMLResponse(html)
        response.headers["Cache-Control"] = "no-store"
        await response(scope, receive, send)


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
    _patch_run_mode_reconnect()

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
    ready_path = f"{root_url}/_marimo_ready" if root_url else "/_marimo_ready"
    fallback_app = CachedExportFallback(
        marimo_app,
        directory=directory,
        base_url=base_url,
        ready_path=ready_path,
        include_code=include_code,
    )

    async def marimo_ready(request):
        request_path = request.query_params.get("path", "")
        session_id = request.query_params.get("session_id")
        file_path = _resolve_app_file(request_path, base_url, Path(directory))
        if file_path is None:
            return JSONResponse({"ready": False, "cell_statuses": {}})
        session = _get_session(marimo_app, file_path, session_id=session_id)
        if session is None:
            return JSONResponse({"ready": False, "cell_statuses": {}})
        ready = session.kernel_state() == KernelState.RUNNING and _is_run_complete(
            session
        )
        return JSONResponse(
            {
                "ready": ready,
                "cell_statuses": _session_cell_statuses(session),
            }
        )

    middleware = [
        Middleware(ActiveConnectionsMiddleware),
    ]
    app = Starlette(
        routes=[
            Route(f"{root_url}/_health", health),
            Route(f"{root_url}/_marimo_ready", marimo_ready),
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
            Mount("/", fallback_app),
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
    parser.add_argument("--token-password")
    parser.add_argument("--no-token", action="store_true")
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
        ),
        host=args.host,
        port=args.port,
    )
