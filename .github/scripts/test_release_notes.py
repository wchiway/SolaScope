import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import urllib.error

from release_notes import (
    DEFAULT_MODEL, ENDPOINT, MAX_RESPONSE_BYTES, MAX_SUMMARY_BYTES, NoRedirect,
    NotesError, extract_summary, generate_summary, read_json, render_notes,
)


def facts():
    return {
        "tag": "v0.1.1", "version": "0.1.1", "source_sha": "a" * 40,
        "prerelease": False, "previous_tag": "v0.1.0", "commit_count": 1,
        "commits": [{"sha": "1234567", "subject": "fix: safe installer"}],
    }


def response(text="## Fixes\n- Add safe Windows installation."):
    return {
        "status": "completed", "error": None, "incomplete_details": None,
        "output": [
            {"type": "reasoning", "summary": [{"text": "private reasoning"}]},
            {"type": "message", "role": "assistant", "status": "completed",
             "content": [{"type": "output_text", "text": text}]},
        ],
    }


class FakeOpener:
    def __init__(self, encoded):
        self.encoded = encoded
        self.requests = []

    def open(self, request, timeout):
        self.requests.append((request, timeout))
        return io.BytesIO(self.encoded)


class ReleaseNotesTests(unittest.TestCase):
    def test_uses_responses_contract_and_never_includes_repository_or_authors(self):
        opener = FakeOpener(json.dumps(response()).encode())
        data = facts() | {"repository": "private-project", "author_email": "private@example.invalid"}
        summary = generate_summary(data, "test-only-key", DEFAULT_MODEL, opener)
        self.assertIn("## Fixes", summary)
        request, timeout = opener.requests[0]
        self.assertEqual(request.full_url, ENDPOINT)
        self.assertEqual(request.get_method(), "POST")
        self.assertEqual(request.get_header("Authorization"), "Bearer test-only-key")
        payload = json.loads(request.data)
        self.assertEqual(payload["model"], "deepseek-flash")
        self.assertEqual(payload["reasoning"], {"effort": "none"})
        self.assertFalse(payload["stream"])
        self.assertNotIn("store", payload)
        self.assertNotIn("messages", payload)
        for private in ("private-project", "private@example", "test-only-key"):
            self.assertNotIn(private, request.data.decode())
        self.assertLessEqual(timeout, 60)
        self.assertNotIn("private reasoning", summary)

    def test_parses_only_completed_assistant_output_text(self):
        output = response("## Features\n- Add installation.")
        output["output"].append({
            "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "- Preserve settings."}],
        })
        self.assertEqual(extract_summary(output), "## Features\n- Add installation.\n- Preserve settings.")

    def test_rejects_incomplete_failed_malformed_and_empty_responses(self):
        invalid = [None, [], {}, {"output_text": "not a wire contract"},
                   response("") | {"status": "incomplete"}, response() | {"status": "failed"},
                   response() | {"error": {"message": "secret"}},
                   response() | {"incomplete_details": {"reason": "max_output_tokens"}},
                   response(""), response("a" * (MAX_SUMMARY_BYTES + 1)),
                   {"status": "completed", "output": [None]},
                   {"status": "completed", "output": [{"type": "message", "role": "user", "content": []}]},
                   {"status": "completed", "output": [{"type": "message", "role": "assistant", "content": None}]},
                   {"status": "completed", "output": [{"type": "message", "role": "assistant", "content": [{"type": "refusal", "refusal": "no"}]}]}]
        for output in invalid:
            with self.subTest(output=output), self.assertRaises(NotesError):
                extract_summary(output)

    def test_rejects_publishable_links_mentions_html_controls_and_code(self):
        for text in ("@someone", "[link](https://example.invalid)", "<script>x</script>", "```powershell\nx\n```", "text\x1b[0m"):
            with self.subTest(text=text), self.assertRaises(NotesError):
                extract_summary(response(text))

    def test_rejects_oversized_and_non_json_provider_bodies(self):
        for encoded in (b"not json secret", b"a" * (MAX_RESPONSE_BYTES + 1)):
            with self.subTest(size=len(encoded)), self.assertRaises(NotesError) as error:
                generate_summary(facts(), "test-only-key", DEFAULT_MODEL, FakeOpener(encoded))
            self.assertNotIn("secret", str(error.exception))

    def test_retries_transient_http_errors_without_reading_or_logging_body(self):
        opener = FakeOpener(json.dumps(response()).encode())
        success = opener.open
        error = urllib.error.HTTPError(ENDPOINT, 429, "provider-secret", {}, io.BytesIO(b"sensitive body"))
        with patch.object(opener, "open", side_effect=[error, success(None, 45)]), patch("release_notes.time.sleep") as sleep:
            self.assertIn("## Fixes", generate_summary(facts(), "test-only-key", DEFAULT_MODEL, opener))
        sleep.assert_called_once()
        self.assertTrue(error.fp.closed)

    def test_sanitizes_http_connection_errors_and_rejects_redirects(self):
        for failure in (
            urllib.error.HTTPError(ENDPOINT, 401, "private-key", {}, io.BytesIO(b"private-key")),
            urllib.error.URLError("private-key"), TimeoutError("private-key"),
        ):
            opener = FakeOpener(b"")
            with patch.object(opener, "open", side_effect=failure), self.assertRaises(NotesError) as caught:
                generate_summary(facts(), "test-only-key", DEFAULT_MODEL, opener)
            self.assertNotIn("private-key", str(caught.exception))
        with self.assertRaisesRegex(NotesError, "redirect"):
            NoRedirect().redirect_request(None, None, 302, "move", {}, "https://example.invalid")

    def test_missing_key_or_invalid_model_never_makes_network_request(self):
        opener = FakeOpener(b"")
        for key, model in (("", DEFAULT_MODEL), ("test-only-key\n", DEFAULT_MODEL), ("key", "../escape")):
            with self.assertRaises(NotesError):
                generate_summary(facts(), key, model, opener)
        self.assertFalse(opener.requests)

    def test_rendered_facts_are_escaped_and_summary_never_replaces_changelog(self):
        data = facts()
        data["commits"][0]["subject"] = "fix: [link](x) <html> @someone *bold*"
        data["commit_count"] = 2
        notes = render_notes(data, "## Fixes\n- Preserve settings.")
        for expected in ("## Changes", "## Downloads", "&#64;someone", "&lt;html&gt;", "\\[link\\]", "latest 1 of 2", "a" * 40):
            self.assertIn(expected, notes)
        self.assertNotIn("@someone", notes)
        self.assertNotIn("<html>", notes)

    def test_isolated_cli_without_key_writes_factual_fallback_and_warns(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "facts.json"
            source.write_text(json.dumps(facts()), encoding="utf-8")
            script = Path(__file__).with_name("release_notes.py")
            import os
            environment = dict(os.environ, DEEPSEEK_API_KEY="")
            result = subprocess.run(
                [sys.executable, "-I", "-B", str(script), "--facts", str(source), "--output", str(root / "notes.md")],
                env=environment, capture_output=True, encoding="utf-8",
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("::warning::", result.stderr)
            self.assertIn("safe installer", (root / "notes.md").read_text())
            self.assertNotIn("Summary generated", (root / "notes.md").read_text())

    def test_invalid_unicode_partial_http_and_response_deadline_fall_back(self):
        with self.assertRaises(NotesError):
            extract_summary(response("\ud800"))
        import http.client
        opener = FakeOpener(b"")
        with patch.object(opener, "open", side_effect=http.client.IncompleteRead(b"private")), self.assertRaisesRegex(NotesError, "connection"):
            generate_summary(facts(), "test-only-key", DEFAULT_MODEL, opener)
        opener = FakeOpener(json.dumps(response()).encode())
        with patch("release_notes.time.monotonic", side_effect=[0, 46]), self.assertRaisesRegex(NotesError, "timed out"):
            generate_summary(facts(), "test-only-key", DEFAULT_MODEL, opener)

    def test_invalid_facts_fail_before_provider_or_output(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "facts.json"
            for data in ([], {}, facts() | {"source_sha": "bad"}, facts() | {"commits": [None]}, facts() | {"commit_count": 0}):
                source.write_text(json.dumps(data), encoding="utf-8")
                with self.assertRaises(NotesError):
                    read_json(source)


if __name__ == "__main__":
    unittest.main()
