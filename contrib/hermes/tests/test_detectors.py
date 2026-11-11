"""Detector tests, ported from Alexandria's pi companion test suite
(contrib/pi/extensions/alexandria/tests/detectors-*.test.ts). Case-for-case where the
semantics transfer; Hermes naming added for the tool tracker."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
_hermes_src = Path.home() / ".hermes" / "hermes-agent"  # real ABC when Hermes is installed here
if _hermes_src.is_dir():
    sys.path.insert(0, str(_hermes_src))
try:
    import agent.memory_provider  # noqa: F401
except ImportError:
    sys.path.insert(0, str(Path(__file__).resolve().parent / "_hermes_stub"))

from alexandria.detectors import (  # noqa: E402
    ErrorTracker,
    SessionDedupBuffer,
    bare_tool_name,
    detect_correction,
    detect_preference,
    track_tool_store,
)


class TestCorrection(unittest.TestCase):
    def detect(self, prompt):
        return detect_correction(prompt, SessionDedupBuffer())

    def test_each_pattern_captures_the_corrected_statement(self):
        cases = [
            ("No, use tabs not spaces.", "tabs not spaces"),
            ("That's wrong, the port is 8080.", "the port is 8080"),
            ("Actually, the config lives under XDG.", "the config lives under XDG"),
            ("I meant the storage crate.", "the storage crate"),
            ("Not sqlite, use surrealdb please.", "surrealdb please"),
            ("Wrong - the default is 300 seconds.", "the default is 300 seconds"),
            ("Incorrect — retries are off by default!", "retries are off by default"),
        ]
        for prompt, expected in cases:
            with self.subTest(prompt=prompt):
                self.assertEqual(
                    self.detect(prompt),
                    {"content": f"User correction: {expected}",
                     "tags": ["correction", "auto-detected", "source:regex"]},
                )

    def test_returns_null_when_nothing_matches(self):
        self.assertIsNone(self.detect("Please add a test for the parser."))
        self.assertIsNone(self.detect("Use ripgrep instead of grep."))
        # A prohibition, not a correction: the preference detector owns it.
        self.assertIsNone(self.detect("Don't use tabs, use spaces."))

    def test_skips_prompts_over_500_characters(self):
        self.assertIsNone(self.detect("Actually, " + "x " * 250))

    def test_skips_short_corrected_statement(self):
        self.assertIsNone(self.detect("No, use a b"))  # two words: only the length rule rejects

    def test_skips_one_word_corrected_statement(self):
        self.assertIsNone(self.detect("No, it's completed."))  # state report, not a fact

    def test_dedups_repeated_corrections(self):
        buffer = SessionDedupBuffer()
        self.assertIsNotNone(detect_correction("Actually, the port is 8080.", buffer))
        self.assertIsNone(detect_correction("actually,  THE port is 8080", buffer))


class TestPreference(unittest.TestCase):
    def detect(self, prompt):
        return detect_preference(prompt, SessionDedupBuffer())

    def stored(self, prompt):
        found = self.detect(prompt)
        return found["content"] if found else None

    def test_prohibition_stored_with_its_negation(self):
        self.assertEqual(self.stored("Never commit Cargo.lock here."),
                         "User preference: Never commit Cargo.lock here")
        self.assertEqual(self.stored("Don't ever force push main."),
                         "User preference: Don't ever force push main")
        self.assertEqual(self.stored("Never run recursive searches without scoping first."),
                         "User preference: Never run recursive searches without scoping first")
        self.assertEqual(self.stored("Use jaq instead of jq."),
                         "User preference: Use jaq instead of jq")

    def test_statements_start_at_their_trigger(self):
        cases = [
            ("Always run just lint before pushing.", "Always run just lint before pushing"),
            ("I prefer tabs over spaces.", "I prefer tabs over spaces"),
            ("I like ripgrep better than grep.", "I like ripgrep better than grep"),
            ("Default to the stable toolchain.", "Default to the stable toolchain"),
            ("Make sure to run the tests.", "Make sure to run the tests"),
            ("Please don't push without explicit go-ahead.", "don't push without explicit go-ahead"),
            ("Do not amend pushed commits!", "Do not amend pushed commits"),
            ("Don't use tabs, use spaces.", "Don't use tabs, use spaces"),
            ("ok. I don't ever want force pushes", "don't ever want force pushes"),
            ("From now on, use conventional commits.", "use conventional commits"),
            ("Going forward, never squash review fixes.", "never squash review fixes"),
        ]
        for prompt, expected in cases:
            with self.subTest(prompt=prompt):
                self.assertEqual(
                    self.detect(prompt),
                    {"content": f"User preference: {expected}",
                     "tags": ["preference", "auto-detected", "source:regex"]},
                )

    def test_negation_before_trigger_is_never_dropped(self):
        self.assertEqual(self.stored("Please don't always rebase onto main."),
                         "User preference: don't always rebase onto main")
        # No pattern can keep this negation, so nothing is stored.
        self.assertIsNone(self.detect("It's not that I prefer tabs over spaces."))
        self.assertIsNone(self.detect("You shouldn't default to the nightly toolchain."))
        self.assertIsNone(self.detect("There's no need to always rebase first."))
        # The guard is per clause.
        self.assertEqual(self.stored("No, always run clippy first."),
                         "User preference: always run clippy first")
        self.assertEqual(self.stored("That is not it. Always run clippy first."),
                         "User preference: Always run clippy first")

    def test_mid_clause_dont_is_about_the_speaker(self):
        self.assertIsNone(self.detect("I don't know why this test fails."))
        self.assertIsNone(self.detect("We don't have a staging server."))

    def test_returns_null_when_nothing_matches(self):
        self.assertIsNone(self.detect("Please add a test for the parser."))

    def test_length_guards(self):
        self.assertIsNone(self.detect("never x"))  # 7 chars
        self.assertIsNone(self.detect("Always " + "x" * 500))

    def test_dedups_repeated_preferences(self):
        buffer = SessionDedupBuffer()
        self.assertIsNotNone(detect_preference("Always run clippy.", buffer))
        self.assertIsNone(detect_preference("always  RUN clippy", buffer))

    def test_use_x_instead_of_y_stored_once_as_preference(self):
        buffer = SessionDedupBuffer()
        prompt = "Use ripgrep instead of grep."
        self.assertIsNone(detect_correction(prompt, buffer))
        self.assertEqual(detect_preference(prompt, buffer)["content"],
                         "User preference: Use ripgrep instead of grep")


class TestToolTracker(unittest.TestCase):
    def call(self, tool_name, content="fact", is_error=False):
        buffer = SessionDedupBuffer()
        recorded = track_tool_store(tool_name, content, is_error, buffer)  # type: ignore[arg-type]  # 42 is the point: non-string content
        return {"recorded": recorded, "stored": buffer.get_all_stored_contents()}

    def test_records_store_tools_in_client_namings(self):
        # pi 0.99+, adapter 2.x, bare, Hermes native MCP, and Hermes provider-tool naming.
        for name in ("mcp__alexandria__store_memory", "alexandria_store_memory",
                     "mcp_alexandria_store_memory", "store_memory",
                     "mcp__alexandria__update_memory", "update_memory"):
            with self.subTest(name=name):
                self.assertEqual(self.call(name), {"recorded": True, "stored": ["fact"]})

    def test_ignores_other_servers_suffix_tools(self):
        self.assertEqual(self.call("mcp__agentmemory__auto_store_memory"), {"recorded": False, "stored": []})
        self.assertEqual(self.call("mcp__agentmemory__update_memory_backup"), {"recorded": False, "stored": []})

    def test_ignores_other_tools_errors_nonstring(self):
        self.assertEqual(self.call("mcp__alexandria__retrieve_memories"), {"recorded": False, "stored": []})
        self.assertEqual(self.call("mcp__alexandria__store_memory", "fact", True), {"recorded": False, "stored": []})
        self.assertEqual(self.call("mcp__alexandria__store_memory", 42), {"recorded": False, "stored": []})
        self.assertEqual(self.call("mcp__alexandria__store_memory", ""), {"recorded": False, "stored": []})

    def test_known_bound_bare_name_other_server_accepted(self):
        # Pinned with pi: exact-name matching cannot identify the server (see tool-tracker.ts).
        self.assertEqual(self.call("mcp__agentmemory__store_memory"), {"recorded": True, "stored": ["fact"]})
        self.assertEqual(self.call("bulk_store_memory"), {"recorded": True, "stored": ["fact"]})

    def test_bare_tool_name_reduction(self):
        self.assertEqual(bare_tool_name("mcp__alexandria__store_memory"), "store_memory")
        self.assertEqual(bare_tool_name("mcp_alexandria_store_memory"), "store_memory")
        self.assertEqual(bare_tool_name("alexandria_store_memory"), "store_memory")
        self.assertEqual(bare_tool_name("terminal"), "terminal")


class TestErrorTracker(unittest.TestCase):
    # Representative cases from pi's detectors-error-tracker.test.ts semantics.
    ERROR = "Error: connection refused while fetching the index from the registry mirror host"
    SUCCESS = "ok, fetched 128 entries successfully"

    def test_pairs_error_with_next_success(self):
        tracker = ErrorTracker()
        tracker.record_error("terminal", self.ERROR)
        tracker.record_success("terminal", self.SUCCESS)
        memories = tracker.flush()
        self.assertEqual(len(memories), 1)
        self.assertIn("Error with terminal:", memories[0]["content"])
        self.assertIn("Resolution: " + self.SUCCESS, memories[0]["content"])
        self.assertIn("error-resolution", memories[0]["tags"])

    def test_unpaired_error_flushes_nothing_and_releases_claim(self):
        tracker = ErrorTracker()
        tracker.record_error("terminal", self.ERROR)
        self.assertEqual(tracker.flush(), [])
        # Same class again is recordable (claim released).
        tracker.record_error("terminal", self.ERROR)
        tracker.record_success("terminal", self.SUCCESS)
        self.assertEqual(len(tracker.flush()), 1)

    def test_error_class_stored_once_per_session(self):
        tracker = ErrorTracker()
        tracker.record_error("terminal", self.ERROR)
        tracker.record_success("terminal", self.SUCCESS)
        self.assertEqual(len(tracker.flush()), 1)  # first pair stored, claim kept
        tracker.record_error("terminal", self.ERROR)  # class already produced a memory
        tracker.record_success("terminal", self.SUCCESS)
        self.assertEqual(tracker.flush(), [])

    def test_short_and_signalless_errors_dropped(self):
        tracker = ErrorTracker()
        tracker.record_error("terminal", "Error: refused")  # under 30 chars
        tracker.record_error("terminal", "the widget was painted twice on resize by design")  # no signal word
        tracker.record_success("terminal", self.SUCCESS)
        self.assertEqual(tracker.flush(), [])

    def test_transient_call_shape_dropped_unless_alexandria(self):
        tracker = ErrorTracker()
        noise = "Validation failed for tool terminal: missing required parameter 'command' in call"
        tracker.record_error("terminal", noise)
        tracker.record_success("terminal", self.SUCCESS)
        self.assertEqual(tracker.flush(), [])
        # Alexandria's own API contract is a lesson, not noise.
        ours = "Validation failed for tool alexandria_store_memory: unexpected argument 'contnet'"
        tracker.record_error("alexandria_store_memory", ours)
        tracker.record_success("alexandria_store_memory", "stored ok")
        self.assertEqual(len(tracker.flush()), 1)

    def test_path_lookup_head_is_transient(self):
        tracker = ErrorTracker()
        tracker.record_error("read_file", "Path not found: /home/jorb/nope/does-not-exist.txt")
        tracker.record_success("read_file", self.SUCCESS)
        self.assertEqual(tracker.flush(), [])
        # Same words mid-message are durable detail.
        tracker.record_error("terminal", "alexandria serve failed: Path not found: ~/.config/alexandria/x")
        tracker.record_success("terminal", self.SUCCESS)
        self.assertEqual(len(tracker.flush()), 1)

    def test_ring_eviction_keeps_newest_five_pairable(self):
        tracker = ErrorTracker()
        for i in range(7):
            tracker.record_error(f"tool{i}", f"Error: failure number {i} in the pipeline stage")
        # The two oldest were evicted: their successes pair with nothing.
        tracker.record_success("tool0", self.SUCCESS)
        self.assertEqual(tracker.flush(), [])
        # The newest is still in the ring.
        tracker.record_error("tool6", "Error: failure number six in the pipeline stage")
        tracker.record_success("tool6", self.SUCCESS)
        self.assertEqual(len(tracker.flush()), 1)


class TestDedupBuffer(unittest.TestCase):
    def test_normalized_dedup_and_raw_list(self):
        buffer = SessionDedupBuffer()
        self.assertTrue(buffer.add_heuristic_store("Fact  One"))
        self.assertFalse(buffer.add_heuristic_store("fact   one"))
        buffer.add_tool_store("Tool stored")
        self.assertEqual(buffer.get_all_stored_contents(), ["Fact  One", "Tool stored"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
