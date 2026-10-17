"""Snapshot selectors must never become path navigation, queries or fragments."""

from __future__ import annotations

import httpx
import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient
from tests.conftest import API_KEY, BASE_URL

NAMES = [
    ("target?dry_run=true", b"target%3Fdry_run%3Dtrue"),
    ("target#part", b"target%23part"),
    ("target/child", b"target%2Fchild"),
    ("literal%2Fname", b"literal%252Fname"),
    (".", b"%2E"),
    ("..", b"%2E%2E"),
    ("简单_snapshot", b"%E7%AE%80%E5%8D%95_snapshot"),
    ("simple_name", b"simple_name"),
]


def response_for(request: httpx.Request, encoded: bytes, operation: str) -> httpx.Response:
    suffix = b"/rollback" if operation == "rollback" else b""
    assert request.method == ("POST" if operation == "rollback" else "DELETE")
    assert request.url.raw_path == b"/v1/snapshots/" + encoded + suffix
    assert not request.url.query
    assert not request.url.fragment
    return httpx.Response(204)


@pytest.mark.parametrize("name,encoded", NAMES)
@pytest.mark.parametrize("operation", ["delete", "rollback"])
def test_sync_snapshot_selector_is_literal(
    httpx_mock: HTTPXMock, name: str, encoded: bytes, operation: str,
) -> None:
    httpx_mock.add_callback(lambda request: response_for(request, encoded, operation))
    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        getattr(client.snapshots, operation)(name)
    assert len(httpx_mock.get_requests()) == 1


@pytest.mark.parametrize("name,encoded", NAMES)
@pytest.mark.parametrize("operation", ["delete", "rollback"])
async def test_async_snapshot_selector_is_literal(
    httpx_mock: HTTPXMock, name: str, encoded: bytes, operation: str,
) -> None:
    httpx_mock.add_callback(lambda request: response_for(request, encoded, operation))
    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await getattr(client.snapshots, operation)(name)
    assert len(httpx_mock.get_requests()) == 1
