import json

import httpx
import pytest
from conftest import eventually, plugin
from test_provider import call


def write_messages(subject, *, tool="memoria_store", success=True, memory_id="a" * 32):
    return [
        {"role": "user", "content": "I like rainy days and drink black coffee."},
        {
            "role": "assistant",
            "tool_calls": [{"id": "save-1", "function": {"name": tool}}],
        },
        {
            "role": "tool",
            "tool_call_id": "save-1",
            "content": json.dumps(
                {
                    "success": success,
                    "result": {"memory_id": memory_id, "subject_id": subject},
                }
            ),
        },
        {"role": "assistant", "content": "Saved your rainy-day preference."},
    ]


def test_successful_store_excludes_only_saved_ids_and_preserves_whole_turn(factory):
    make, server = factory
    p = make(auto_capture=True)
    saved = call(p, "store", content="User likes rainy days", memory_type="profile")["result"]
    messages = write_messages(p._subject, memory_id=saved["memory_id"])
    p.sync_turn(messages[0]["content"], messages[-1]["content"], messages=messages)
    eventually(lambda: p._outbox.counts(p._binding) == {"done": 1})
    observes = [(path, body) for _, path, body, _ in server.calls if "/observe" in path]
    assert len(observes) == 1
    path, payload = observes[0]
    assert path == "/v1/observe/deduplicated"
    assert payload["exclude_memory_ids"] == [saved["memory_id"]]
    assert payload["messages"] == [messages[0], messages[-1]]
    assert "black coffee" in payload["messages"][0]["content"]
    assert "tool_calls" not in json.dumps(payload)
    # The capture receipt, not a mutable in-memory flag, suppresses repeated callbacks.
    p.sync_turn(messages[0]["content"], messages[-1]["content"], messages=messages)
    assert p._outbox.counts(p._binding) == {"done": 1}


@pytest.mark.parametrize("tool", ["memoria_store", "memoria_update"])
def test_current_turn_write_ids_are_correlated_and_deduplicated(tool):
    messages = write_messages("subject", tool=tool)
    messages.append(messages[-2])
    assert plugin._turn_saved_ids(messages, "subject") == ["a" * 32]
    messages.append({"role": "user", "content": "A different next turn"})
    assert plugin._turn_saved_ids(messages, "subject") == []


@pytest.mark.parametrize("case", ["failed", "other_subject", "search", "unmatched", "malformed"])
def test_capture_does_not_trust_unsuccessful_or_unrelated_tool_results(case):
    messages = write_messages("subject")
    if case == "failed":
        messages = write_messages("subject", success=False)
    elif case == "other_subject":
        messages = write_messages("different")
    elif case == "search":
        messages = write_messages("subject", tool="memoria_search")
    elif case == "unmatched":
        messages[2]["tool_call_id"] = "not-the-save-call"
    else:
        messages[2]["content"] = "invalid json"
    assert plugin._turn_saved_ids(messages, "subject") == []


def test_old_server_rejection_never_falls_back_to_plain_observe(factory):
    make, server = factory
    p = make(auto_capture=True)
    server.observe_handler = lambda request, body: httpx.Response(404)
    messages = write_messages(p._subject)
    p.sync_turn(messages[0]["content"], "saved", messages=messages)
    eventually(lambda: p._outbox.counts(p._binding) == {"failed": 1})
    assert [path for _, path, _, _ in server.calls] == ["/v1/observe/deduplicated"]
    with p._outbox.db() as db:
        row = db.execute("SELECT payload, failure_kind FROM events").fetchone()
        assert json.loads(row["payload"])["exclude_memory_ids"] == ["a" * 32]
        assert row["failure_kind"] == "rejected"


def test_next_turn_without_explicit_write_uses_existing_observe(factory):
    make, server = factory
    p = make(auto_capture=True)
    messages = write_messages(p._subject) + [{"role": "user", "content": "I use Rust."}]
    p.sync_turn("I use Rust.", "Okay", messages=messages)
    eventually(lambda: p._outbox.counts(p._binding) == {"done": 1})
    _, path, body, _ = server.calls[-1]
    assert path == "/v1/observe" and "exclude_memory_ids" not in body


def test_malformed_call_lists_do_not_prevent_capture():
    messages = [None, {"role": "user", "content": "hello"}, {"role": "assistant", "tool_calls": 12}]
    assert plugin._turn_saved_ids(messages, "subject") == []


def test_exclusions_survive_capture_worker_restart(factory):
    make, server = factory
    p = make(auto_capture=True)
    server.observe_handler = lambda request, body: (_ for _ in ()).throw(
        httpx.ConnectError("offline")
    )
    messages = write_messages(p._subject)
    p.sync_turn(messages[0]["content"], "saved", messages=messages)
    eventually(lambda: len(server.calls) >= 1 and p._outbox.counts(p._binding) == {"pending": 1})
    p.shutdown()
    server.observe_handler = None
    restarted = make(home=p._home, auto_capture=True)
    eventually(lambda: restarted._outbox.counts(restarted._binding) == {"done": 1})
    assert all(path == "/v1/observe/deduplicated" for _, path, _, _ in server.calls)
    assert server.calls[-1][2]["exclude_memory_ids"] == ["a" * 32]
