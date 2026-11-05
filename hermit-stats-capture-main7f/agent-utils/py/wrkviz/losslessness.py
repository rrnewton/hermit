"""The losslessness gate: prove, row by row, what the archive would lose if the vendor logs went.

``teams/*/source_snapshots/`` is 5.83 GB of the 8.79 GB archive, 3.59 GB of it vendor Codex JSONL,
and it is gitignored -- one ``rm -rf`` from gone. The reason nobody can run that command is not
that the vendor format is preferred; it is that nobody can currently *state* what it still holds
that the normalized model does not. This module is the thing that states it.

**It is a differential, not an assertion.** It does not ask the reader whether it dropped
something. It re-enumerates the vendor rows independently, one file at a time, and requires every
single one to fall under a rule that says in words what became of it, then checks the rule's claim
against the archive. Three outcomes matter and they are deliberately different:

* **unaccounted** -- no rule matched. This is the failure the module exists for. A new Codex record
  type, a renamed payload type, a vendor field that starts carrying content it did not carry
  before: all of them land here, on the first audit after they appear, instead of being silently
  absent from every archive built afterwards. There is no default rule and no catch-all, because a
  catch-all is precisely how the drop this module was written to find became invisible in the
  first place.
* **unverified** -- a rule matched and its claim was false. The row says an event exists at this
  line and there is none; a tool call says its stdout is in the payload store and the digest does
  not resolve, or resolves to different bytes. A rule table that is never checked is documentation,
  and documentation drifts.
* **declared lossy** -- a rule matched and its claim is that the content is *not* retained. That is
  not a failure; it is the inventory. The bytes under these rules are exactly the bytes that still
  make ``source_snapshots/`` load-bearing, and their total is the number to watch go to zero.

**Why it is not shaped like a two-implementation differential.** The usual form of that test runs
two builds of one program and byte-compares their observable output. There is no second
implementation here, and the two things being compared are not two programs but a corpus and a
projection of it. What this borrows from that form is the part that makes it worth having: an
explicit enumeration of what is claimed, a refusal to compare only the easy fields, and a nonzero
exit on any divergence. Within this package the closest existing shape is
:mod:`wrkviz.glossary_audit` -- a read-only pass over archive state, no lock, no model
call, a typed report and a CLI subcommand -- and this follows that.

**Coverage is Codex only, and that is a choice, not an oversight.** Codex is where the 3.59 GB is,
it is the provider the retired ``_archive_team`` docstring named as "the authority for command
stdout and patch bodies", and its rollout JSONL is a flat row-per-line corpus that can be
enumerated independently of the reader. Claude's transcripts and Orc's SQLite would each need
their own enumeration and their own rule table; a half-written rule table for three providers would
report "everything is accounted for" for the two whose rows it never learned to see, which is worse
than reporting nothing. :func:`audit_archive_losslessness` names the uncovered teams explicitly in
its report rather than omitting them.

**What running it costs.** It reads the whole vendor corpus, one file at a time, and holds one
file's parsed records at once -- the same shape the reader itself uses, for the same reason: the
canonical-record boundary for a subagent session cannot be found without the records before it.
This is an audit invoked deliberately before an irreversible deletion, not something on the ingest
path.
"""

from __future__ import annotations

import hashlib
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Literal

from wrkviz.build_store import (
    ingested_team_slugs,
    team_build_root as _build_root,
)
from wrkviz.archive import (
    JsonValue,
    as_array,
    as_int,
    as_object,
    as_string,
    canonical_json,
    read_json,
)
from wrkviz.codex import (
    _canonical_records,
    _complete_prefix,
    _complete_records,
    _content,
    _mapping,
    _record_payload,
    _string,
    _textual,
)
from wrkviz.model import PayloadRef, TeamData
from wrkviz.payloads import resolve_payloads, verify_payload_store

AUDIT_SCHEMA_VERSION = 1

#: What became of a vendor row. The split that matters is the last two against the rest: a
#: ``redacted`` row's content is deliberately unwanted, while ``partial`` and ``dropped`` rows are
#: content the archive wanted and does not have.
Disposition = Literal[
    "record",  # became one or more model records
    "payload",  # became a model record plus content-addressed text in the payload store
    "state",  # carries no content of its own; only advances the reader's position
    "duplicate",  # restates content the model already holds from another row
    "redacted",  # deliberately not persisted, by a named policy
    "partial",  # a record exists, but named content of the row is not on it
    "dropped",  # nothing in the archive holds this row's content
]

#: How the rule's claim is checked.
#:
#: The one rule about these: a rule may not claim more than its check proves. ``none`` is therefore
#: reserved for rules that claim nothing about the archive -- a redaction, a state carrier, a
#: declared drop -- and every rule that says content survives names a check that would notice if it
#: did not. Where a claim could only be checked approximately, the rule was split until each piece
#: could be checked exactly; ``item_completed`` became two rules that way, and
#: ``response_item/message`` became two more.
Verification = Literal[
    "none",
    # An event, tool call or edge in raw/team.json cites this exact (thread, line). It proves the
    # row *reached* the model and nothing whatever about what arrived: it is the right check for a
    # rule whose claim is that a marker exists, and the wrong one for any rule whose reason
    # contains the word "verbatim". It used to be the check on all six message rules, and it
    # passed cleanly on an archive with `text` blanked on every single event -- which is the exact
    # regression this module was written to catch, applied to messages instead of tool text.
    "line",
    "prompt-text",  # the event at this line carries this row's `message` verbatim
    "content-text",  # ... this row's content parts, plain and encrypted, verbatim
    "objective-text",  # ... this row's goal objective verbatim, inside its rendered line
    "input-payload",  # the tool call's stored arguments are byte-identical to this row's
    "output-payload",  # the tool call's stored output is byte-identical to this row's
    "model-id",  # the identifier this row names is one the model holds
    "corpus-id",  # ... or one a canonical row elsewhere in the vendor corpus carries
    "parent-thread",  # the session whose history this row replays is an agent in the model
]

_RETAINED: frozenset[str] = frozenset({"record", "payload", "state", "duplicate", "redacted"})

_SAMPLE_LIMIT = 8


@dataclass(frozen=True)
class VendorRule:
    """One declared account of what happens to a class of vendor rows.

    ``reason`` is not decoration and is not a restatement of ``disposition``. It is the sentence a
    reader gets when the audit tells them 260 MB of their archive is unrecoverable, and it has to
    be specific enough to act on -- which field, why it went, and what would have to change.
    """

    name: str
    top_type: str
    payload_type: str | None
    disposition: Disposition
    verification: Verification
    reason: str
    id_path: tuple[str, ...] = ()
    #: An optional second discriminator within one row shape: a path into the payload and the
    #: values that select this rule. Two vendor rows can share ``(type, payload.type)`` and have
    #: entirely different fates -- ``response_item/message`` is kept when its role is
    #: ``assistant`` and discarded otherwise -- and collapsing those into one rule would force its
    #: claim to be true of neither. Rules are consulted in table order and the first whose selector
    #: matches wins, so an unselected rule for the same shape reads as the default it is.
    selector: tuple[tuple[str, ...], frozenset[str]] | None = None

    @property
    def blocks_deletion(self) -> bool:
        """Whether rows under this rule keep the vendor file load-bearing."""

        return self.disposition not in _RETAINED


#: Every Codex row shape observed across the 3.59 GB corpus, and what becomes of each. Counts in
#: the reasons are from that corpus and are there to say which entries are worth anyone's time.
_CODEX_RULES: tuple[VendorRule, ...] = (
    VendorRule(
        name="session-metadata",
        top_type="session_meta",
        payload_type=None,
        disposition="redacted",
        verification="none",
        reason=(
            "the agent record keeps this row's thread id, parent, agent path, depth and start "
            "instant; its `cwd` and any credential embedded in its `git.repository_url` are "
            "deliberately removed by `pipeline._archive_team` and are pinned as removed by "
            "test_ingest_never_persists_cwd_or_repository_credentials. This is a policy loss on "
            "content the archive does not want, not a gap in what it can hold"
        ),
    ),
    VendorRule(
        name="turn-context",
        top_type="turn_context",
        payload_type=None,
        disposition="state",
        verification="none",
        reason=(
            "sets the reader's current turn id and carries no content; every turn it names is "
            "recorded by the task_started row that follows it"
        ),
    ),
    VendorRule(
        name="turn-start",
        top_type="event_msg",
        payload_type="task_started",
        disposition="record",
        verification="model-id",
        reason="becomes a Turn record",
        id_path=("turn_id",),
    ),
    VendorRule(
        name="turn-complete",
        top_type="event_msg",
        payload_type="task_complete",
        disposition="record",
        verification="model-id",
        reason="closes the Turn record with its status, end instant and last agent message",
        id_path=("turn_id",),
    ),
    VendorRule(
        name="turn-aborted",
        top_type="event_msg",
        payload_type="turn_aborted",
        disposition="record",
        verification="model-id",
        reason="closes the Turn record as aborted",
        id_path=("turn_id",),
    ),
    VendorRule(
        name="user-prompt",
        top_type="event_msg",
        payload_type="user_message",
        disposition="record",
        verification="prompt-text",
        reason="becomes a user_prompt Event carrying the message verbatim",
    ),
    VendorRule(
        name="assistant-message",
        top_type="response_item",
        payload_type="message",
        disposition="record",
        verification="content-text",
        reason=(
            "becomes an assistant_message Event carrying the text, or the encrypted content when "
            "the provider withheld it"
        ),
        selector=(("role",), frozenset({"assistant"})),
    ),
    VendorRule(
        name="non-assistant-message",
        top_type="response_item",
        payload_type="message",
        disposition="dropped",
        verification="none",
        reason=(
            "a `developer` or `user` role message in the response stream, which the reader skips: "
            "its guard is `payload.role == 'assistant'` and there is no else branch. 714 of the "
            "1,222 message rows in one measured team. The `user` ones are largely restated by the "
            "event_msg/user_message row for the same turn, which *is* recorded; the `developer` "
            "ones are the system and instruction prompt the agent actually ran under, and they "
            "are nowhere else in the archive"
        ),
    ),
    VendorRule(
        name="inter-agent-message",
        top_type="response_item",
        payload_type="agent_message",
        disposition="record",
        verification="content-text",
        reason="becomes an inter_agent_message Event carrying the text, author and recipient",
    ),
    VendorRule(
        name="agent-message-event",
        top_type="event_msg",
        payload_type="agent_message",
        disposition="dropped",
        verification="none",
        reason=(
            "94,074 rows -- the second most numerous shape in the corpus -- and not one of them "
            "reaches the model. The reader has an `agent_message` branch, but a `top_type != "
            "'response_item': continue` guard sits above it, so this branch is unreachable for an "
            "event_msg row; and even if it were reached it reads text from `payload.content` "
            "while this row carries it in `payload.message`. The bodies are the agents' own "
            "structured adjudications -- risk level, authorization, outcome, rationale -- and "
            "they exist only here"
        ),
    ),
    VendorRule(
        name="subagent-activity",
        top_type="event_msg",
        payload_type="sub_agent_activity",
        disposition="record",
        verification="line",
        reason=(
            "becomes a spawn or interaction Edge between coordinator and subagent. `line` is the "
            "whole claim and it is enough, because the row carries no free text: its fields are "
            "`agent_path`, `agent_thread_id`, `event_id`, `kind` and `occurred_at_ms`, all of "
            "which are participants and instants rather than content"
        ),
    ),
    VendorRule(
        name="context-compaction",
        top_type="event_msg",
        payload_type="context_compacted",
        disposition="record",
        verification="line",
        reason=(
            "becomes a context_compacted Event marking where history was summarized away. The "
            "vendor row is `{'type': 'context_compacted'}` and nothing else -- measured across "
            "the corpus -- so a citation is the entire content of the row and `line` proves it. "
            "What the compaction *replaced* the history with is a separate `compacted` row, "
            "inventoried under `compaction-replacement` as the loss it is"
        ),
    ),
    VendorRule(
        name="thread-goal",
        top_type="event_msg",
        payload_type="thread_goal_updated",
        disposition="record",
        verification="objective-text",
        reason=(
            "becomes a goal_updated Event whose text carries the goal's objective verbatim. The "
            "claim is a substring and not an equality because the reader renders status and "
            "objective into one line; equality would be a claim about the rendering, and this rule "
            "is only entitled to claim that the objective itself survived"
        ),
    ),
    VendorRule(
        name="tool-call",
        top_type="response_item",
        payload_type="custom_tool_call",
        disposition="payload",
        verification="input-payload",
        reason=(
            "becomes a ToolCall; its arguments are stored verbatim in the content-addressed "
            "payload tree and referenced by digest from raw/team.json"
        ),
    ),
    VendorRule(
        name="tool-call",
        top_type="response_item",
        payload_type="function_call",
        disposition="payload",
        verification="input-payload",
        reason=(
            "becomes a ToolCall; its arguments are stored verbatim in the content-addressed "
            "payload tree and referenced by digest from raw/team.json"
        ),
    ),
    VendorRule(
        name="tool-output",
        top_type="response_item",
        payload_type="custom_tool_call_output",
        disposition="payload",
        verification="output-payload",
        reason=(
            "completes its ToolCall; the output is stored verbatim in the content-addressed "
            "payload tree. This is the single largest thing the archive used to throw away"
        ),
    ),
    VendorRule(
        name="tool-output",
        top_type="response_item",
        payload_type="function_call_output",
        disposition="payload",
        verification="output-payload",
        reason=(
            "completes its ToolCall; the output is stored verbatim in the content-addressed "
            "payload tree. This is the single largest thing the archive used to throw away"
        ),
    ),
    VendorRule(
        name="item-completed-duplicate",
        top_type="event_msg",
        payload_type="item_completed",
        disposition="duplicate",
        verification="corpus-id",
        reason=(
            "the streaming UI's restatement of an item the canonical response_item stream already "
            "carried, joined on the item id it names -- which resolves for exactly these four "
            "item types and is checked here"
        ),
        id_path=("item", "id"),
        selector=(
            ("item", "type"),
            frozenset({"AgentMessage", "SubAgentActivity", "CollabAgentToolCall", "Reasoning"}),
        ),
    ),
    VendorRule(
        name="item-completed-command",
        top_type="event_msg",
        payload_type="item_completed",
        disposition="partial",
        verification="model-id",
        reason=(
            "the executed-command item, and the largest single thing in the corpus: 1,828 rows "
            "and 82.2 MiB in one 125 MiB team, because it carries the same output three times "
            "over as `stdout`, `aggregated_output` and `formatted_output`. The command and its "
            "output text are now in the tool-call and tool-output payloads, so most of this row "
            "is genuinely duplicated -- but `exit_code`, `duration`, `cwd`, `process_id`, "
            "`parsed_cmd` and the stdout/stderr split exist only here, and its `exec-<uuid>` id "
            "is in a namespace nothing else in the model uses, so the join is made at turn "
            "granularity and that is all this rule claims. Its byte total is the whole row and is "
            "therefore an upper bound on what is actually at risk"
        ),
        id_path=("turn_id",),
        selector=(("item", "type"), frozenset({"CommandExecution"})),
    ),
    VendorRule(
        name="item-completed-file-change",
        top_type="event_msg",
        payload_type="item_completed",
        disposition="dropped",
        verification="model-id",
        reason=(
            "the applied patch again, as the UI rendered it: per-file add/update/delete with "
            "complete new file contents. Where the session also wrote a patch_apply_end row the "
            "two carry the same bodies and this inventory counts them twice -- but the measured "
            "team has 116 of these and no patch_apply_end rows at all, so for that team this is "
            "the only copy of the patches the agents wrote"
        ),
        id_path=("turn_id",),
        selector=(("item", "type"), frozenset({"FileChange"})),
    ),
    VendorRule(
        name="item-completed-restatement",
        top_type="event_msg",
        payload_type="item_completed",
        disposition="duplicate",
        verification="model-id",
        reason=(
            "the same streaming restatement for the remaining item types -- UserMessage and "
            "ContextCompaction -- whose UI-side id is not the canonical one, so the join is made "
            "at turn granularity. Both restate a row that is recorded: user-prompt and "
            "context-compaction respectively"
        ),
        id_path=("turn_id",),
    ),
    VendorRule(
        name="imported-turn-prefix",
        top_type="",
        payload_type=None,
        disposition="duplicate",
        verification="parent-thread",
        reason=(
            "precedes the first turn this subagent session owns. Codex writes the parent's "
            "history into the child's rollout before the child's own work begins, and "
            "`codex._canonical_records` cuts exactly there; these rows are canonical in the "
            "parent's own file. The check is that the parent session really is an agent in this "
            "archive, because that -- not the individual row -- is what makes 'canonical "
            "elsewhere' true"
        ),
    ),
    VendorRule(
        name="superseded-tool-output",
        top_type="",
        payload_type=None,
        disposition="dropped",
        verification="none",
        reason=(
            "an interim output for a tool call that emitted more than one. A long-running command "
            "that streams progress writes a `*_call_output` row per update, and the reader assigns "
            "`tool_builder.output_text` unconditionally, so only the last one survives and the "
            "payload store -- which inherits the model's one-output-per-call shape -- stores only "
            "that. Rare and real: 4 of the 38,134 output rows in one measured 1.1 GiB team, all on "
            "a single call reporting '5/20' through '20/20 exact-lease actions verified'. Fixing "
            "it means the ToolCall carrying a sequence of outputs, which is why it is inventoried "
            "here instead. Without this rule these rows read as UNVERIFIED, and a gate that is "
            "permanently red for a known cause stops being read at all"
        ),
    ),
    VendorRule(
        name="unbounded-session",
        top_type="",
        payload_type=None,
        disposition="dropped",
        verification="none",
        reason=(
            "every row of a subagent session in which `codex._canonical_records` found no "
            "boundary at all: no incoming agent_message naming this agent path, and no "
            "task_started at or after the session's own timestamp. The reader takes the agent "
            "seed from line 1 and nothing else, so the whole session body is lost -- 2 of the 48 "
            "sessions in one measured team. This is a rule rather than an unaccounted row "
            "because the cause is known and named; it is `dropped` because the consequence is "
            "the same as if it were not"
        ),
    ),
    VendorRule(
        name="token-count",
        top_type="event_msg",
        payload_type="token_count",
        disposition="dropped",
        verification="none",
        reason=(
            "per-turn context and token usage, plus rate-limit state. 817,440 rows -- more than "
            "any other single shape -- and nothing in the model holds any of it. This is the "
            "archive's whole record of what each agent's context cost"
        ),
    ),
    VendorRule(
        name="patch-apply",
        top_type="event_msg",
        payload_type="patch_apply_end",
        disposition="dropped",
        verification="none",
        reason=(
            "the applied patch: per-file add/update/delete with the complete new file contents, "
            "plus the apply's stdout and stderr. 38,571 rows. Its `call_id` is an `exec-<uuid>` "
            "in a different namespace from the `call_...` ids on tool calls, so it cannot simply "
            "be attached to one as a third payload slot -- which is why it is inventoried here "
            "rather than fixed alongside tool stdout"
        ),
    ),
    VendorRule(
        name="reasoning",
        top_type="response_item",
        payload_type="reasoning",
        disposition="dropped",
        verification="none",
        reason=(
            "the model's reasoning item: a summary array and, usually, an encrypted_content blob. "
            "132,817 rows. The Event family already carries encrypted content for messages, so "
            "there is somewhere for this to go; nothing puts it there today"
        ),
    ),
    VendorRule(
        name="thread-settings",
        top_type="event_msg",
        payload_type="thread_settings_applied",
        disposition="dropped",
        verification="none",
        reason=(
            "the model, provider, service tier, approval policy and full sandbox permission "
            "profile in force from this point. 6,046 rows, and the only record of what each "
            "agent was actually allowed to do"
        ),
    ),
    VendorRule(
        name="mcp-tool-call",
        top_type="event_msg",
        payload_type="mcp_tool_call_end",
        disposition="dropped",
        verification="none",
        reason="an MCP tool invocation's completion; 2 rows in the corpus and no reader for them",
    ),
    VendorRule(
        name="world-state",
        top_type="world_state",
        payload_type=None,
        disposition="dropped",
        verification="none",
        reason=(
            "the agent instruction context in force -- AGENTS.md text and the directory it came "
            "from. 17,955 rows, mostly repeated verbatim, and none of it reaches the model"
        ),
    ),
    VendorRule(
        name="compaction-replacement",
        top_type="compacted",
        payload_type=None,
        disposition="dropped",
        verification="none",
        reason=(
            "the replacement history a compaction installed, which is the only copy of what the "
            "conversation looked like after it. 4,431 rows. The context_compacted Event records "
            "that a compaction happened, not what survived it"
        ),
    ),
    VendorRule(
        name="inter-agent-metadata",
        top_type="inter_agent_communication_metadata",
        payload_type=None,
        disposition="dropped",
        verification="none",
        reason="a one-field trigger flag accompanying an inter-agent message; 9,824 rows",
    ),
)

#: Positional rules, selected by where a row sits rather than by what shape it is.
_PREFIX_RULE = next(rule for rule in _CODEX_RULES if rule.name == "imported-turn-prefix")
_UNBOUNDED_RULE = next(rule for rule in _CODEX_RULES if rule.name == "unbounded-session")
_SUPERSEDED_RULE = next(
    rule for rule in _CODEX_RULES if rule.name == "superseded-tool-output"
)
_POSITIONAL = (_PREFIX_RULE, _UNBOUNDED_RULE, _SUPERSEDED_RULE)

_OUTPUT_TYPES = frozenset({"custom_tool_call_output", "function_call_output"})

_RULES_BY_SHAPE: dict[tuple[str, str | None], tuple[VendorRule, ...]] = {}
for _rule in _CODEX_RULES:
    if _rule in _POSITIONAL:
        continue
    _shape = (_rule.top_type, _rule.payload_type)
    _RULES_BY_SHAPE[_shape] = (*_RULES_BY_SHAPE.get(_shape, ()), _rule)


def _select(rules: Sequence[VendorRule], payload: Mapping[str, object]) -> VendorRule | None:
    for rule in rules:
        if rule.selector is None:
            return rule
        path, accepted = rule.selector
        if _identifier(payload, path) in accepted:
            return rule
    return None


@dataclass(frozen=True)
class RowFinding:
    """One vendor row the audit could not account for, or could not confirm."""

    source_path: str
    line: int
    top_type: str | None
    payload_type: str | None
    vendor_bytes: int
    detail: str

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the finding as a JSON-serializable object."""

        return {
            "source_path": self.source_path,
            "line": self.line,
            "top_type": self.top_type,
            "payload_type": self.payload_type,
            "vendor_bytes": self.vendor_bytes,
            "detail": self.detail,
        }


@dataclass(frozen=True)
class RuleTally:
    """How much of the corpus one rule accounted for."""

    name: str
    disposition: Disposition
    rows: int
    vendor_bytes: int
    unverified: int
    blocks_deletion: bool
    reason: str

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the tally as a JSON-serializable object."""

        return {
            "rule": self.name,
            "disposition": self.disposition,
            "rows": self.rows,
            "vendor_bytes": self.vendor_bytes,
            "unverified": self.unverified,
            "blocks_deletion": self.blocks_deletion,
            "reason": self.reason,
        }


@dataclass(frozen=True)
class LosslessnessReport:
    """What one team's vendor corpus contains, and what the archive would keep without it."""

    team_slug: str
    provider: str
    covered: bool
    vendor_files: int
    vendor_rows: int
    vendor_bytes: int
    # Rows whose content is entirely absent from the archive, and rows only part of which is.
    # They are separate because they call for different work and because their byte totals mean
    # different things: bytes are attributed by whole row, which is exact for `absent` and an
    # upper bound for `partial` -- a CommandExecution row is 45 KB of which the archive already
    # holds nearly all, but there is no honest way to attribute a fraction of a row, so the tool
    # reports the row and says so rather than inventing a number.
    absent_rows: int
    absent_bytes: int
    partial_rows: int
    partial_bytes: int
    tallies: tuple[RuleTally, ...]
    unaccounted: tuple[RowFinding, ...]
    unaccounted_rows: int
    unverified: tuple[RowFinding, ...]
    unverified_rows: int
    payload_problems: tuple[str, ...]
    #: Ways the vendor tree on disk is not the tree ``raw/source-manifest.json`` describes: a
    #: recorded rollout that is gone, one that is shorter than its record, one whose bytes no
    #: longer hash to it, or a file present that nothing recorded. These are what stop the corpus
    #: from being defined by whatever survives.
    source_problems: tuple[str, ...] = ()
    skipped: str | None = None

    @property
    def sound(self) -> bool:
        """Whether every row was accounted for and every rule's claim held.

        This is the gate. It says nothing about whether the archive is *complete* -- see
        :attr:`lossless` -- only that its account of itself is true. A false value means the rule
        table has fallen behind the corpus or the archive does not hold what it says it holds, and
        in either case the inventory below it cannot be trusted either.

        ``source_problems`` is in here rather than beside the inventory because a corpus that is
        not the recorded corpus makes every other number on this report a statement about a
        different archive. It is the one failure that has to be checked before the enumeration
        rather than derived from it.
        """

        return (
            self.covered
            and not self.unaccounted_rows
            and not self.unverified_rows
            and not self.payload_problems
            and not self.source_problems
        )

    @property
    def lossless(self) -> bool:
        """Whether deleting this team's vendor snapshots would lose nothing the archive wants."""

        return self.sound and not self.absent_rows and not self.partial_rows

    @property
    def blocking_bytes(self) -> int:
        """Whole-row bytes under every rule that keeps the vendor file load-bearing."""

        return self.absent_bytes + self.partial_bytes

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the report as a JSON-serializable object."""

        return {
            "team_slug": self.team_slug,
            "provider": self.provider,
            "covered": self.covered,
            "skipped": self.skipped,
            "sound": self.sound,
            "lossless": self.lossless,
            "vendor_files": self.vendor_files,
            "vendor_rows": self.vendor_rows,
            "vendor_bytes": self.vendor_bytes,
            "absent_rows": self.absent_rows,
            "absent_bytes": self.absent_bytes,
            "partial_rows": self.partial_rows,
            "partial_bytes": self.partial_bytes,
            "unaccounted_rows": self.unaccounted_rows,
            "unverified_rows": self.unverified_rows,
            "rules": [tally.to_json_obj() for tally in self.tallies],
            "unaccounted_samples": [item.to_json_obj() for item in self.unaccounted],
            "unverified_samples": [item.to_json_obj() for item in self.unverified],
            "payload_problems": list(self.payload_problems),
            "source_problems": list(self.source_problems),
        }


@dataclass(frozen=True)
class ArchiveLosslessnessReport:
    """Every selected team's verdict, including the ones no provider rule table covers."""

    reports: tuple[LosslessnessReport, ...]

    @property
    def sound(self) -> bool:
        """Whether every *covered* team's account of itself held.

        An uncovered team cannot make the gate fail, because it has made no claim to be wrong
        about. It also cannot make it pass: :attr:`lossless` is false while any team is uncovered,
        which is what keeps "we never taught the audit to read Orc" from reading as "Orc is fine".
        """

        return all(report.sound for report in self.reports if report.covered)

    @property
    def lossless(self) -> bool:
        """Whether the whole selection could give up its vendor snapshots today."""

        return bool(self.reports) and all(report.lossless for report in self.reports)

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the archive-wide report as a JSON-serializable object."""

        return {
            "schema_version": AUDIT_SCHEMA_VERSION,
            "tool": "wrkviz",
            "sound": self.sound,
            "lossless": self.lossless,
            "teams": [report.to_json_obj() for report in self.reports],
        }


def _identifier(payload: Mapping[str, object], path: Sequence[str]) -> str | None:
    current: object = payload
    for key in path:
        if not isinstance(current, dict):
            return None
        current = _mapping(current).get(key)
    return _string(current)


def unscope_codex_id(value: str) -> str:
    """Undo ``codex._scoped_id`` so a vendor row's raw id can be looked up in the model.

    A continuation lineage rewrites every id it owns to
    ``codex-continuation-<len>-<lineage>-<len>-<raw>``, precisely so two sessions that both call a
    turn ``root-turn`` cannot collapse into one. The audit reads the *vendor* file, which still
    says ``root-turn``, so it has to undo the rewrite to compare. Both components are
    length-prefixed, which is what makes this exact rather than a guess: the parse reads a length,
    skips that many characters, and reads the next -- a string that merely looks like the prefix
    fails one of those checks and is returned unchanged, which is the correct answer for an id that
    was never scoped.
    """

    prefix = "codex-continuation-"
    if not value.startswith(prefix):
        return value
    rest = value[len(prefix) :]
    lineage_length, separator, remainder = rest.partition("-")
    if not separator or not lineage_length.isdigit():
        return value
    skip = int(lineage_length)
    if len(remainder) <= skip or remainder[skip] != "-":
        return value
    tail = remainder[skip + 1 :]
    raw_length, separator, raw = tail.partition("-")
    if not separator or not raw_length.isdigit() or len(raw) != int(raw_length):
        return value
    return raw


@dataclass(frozen=True)
class _ArchiveIndex:
    """Everything the rule checks need from ``raw/team.json``, joined the way vendor rows join."""

    lines: frozenset[tuple[str, int]]
    #: Every ``(text, encrypted_content)`` pair an Event cites at a given ``(thread, line)``. A
    #: list rather than a single value because a continuation lineage reads one physical row under
    #: more than one scoped identity, so two Events can legitimately share a line; the check is
    #: membership, which is exact for that case and no weaker for the ordinary one.
    event_content: Mapping[tuple[str, int], tuple[tuple[str | None, str | None], ...]]
    identifiers: frozenset[str]
    input_payload_by_call: Mapping[str, str | None]
    output_payload_by_call: Mapping[str, str | None]
    resolved: Mapping[str, str]


def _build_index(archive: Path, team: TeamData) -> _ArchiveIndex:
    lines = {(event.thread_id, event.source_line) for event in team.events}
    content: dict[tuple[str, int], tuple[tuple[str | None, str | None], ...]] = {}
    for event in team.events:
        at = (event.thread_id, event.source_line)
        content[at] = (*content.get(at, ()), (event.text, event.encrypted_content))
    lines.update((tool.thread_id, tool.source_line) for tool in team.tool_calls)
    lines.update((edge.from_thread_id, edge.source_line) for edge in team.edges)
    identifiers = {unscope_codex_id(event.event_id) for event in team.events}
    identifiers.update(unscope_codex_id(turn.turn_id) for turn in team.turns)
    identifiers.update(unscope_codex_id(tool.call_id) for tool in team.tool_calls)
    identifiers.update(
        unscope_codex_id(tool.item_id) for tool in team.tool_calls if tool.item_id is not None
    )
    identifiers.update(unscope_codex_id(edge.edge_id) for edge in team.edges)
    identifiers.update(agent.thread_id for agent in team.agents)
    inputs: dict[str, str | None] = {}
    outputs: dict[str, str | None] = {}
    refs: list[PayloadRef] = []
    for tool in team.tool_calls:
        raw_call_id = unscope_codex_id(tool.call_id)
        inputs[raw_call_id] = tool.input_payload.sha256 if tool.input_payload else None
        outputs[raw_call_id] = tool.output_payload.sha256 if tool.output_payload else None
        refs.extend(ref for ref in (tool.input_payload, tool.output_payload) if ref is not None)
    resolved = resolve_payloads(_build_root(archive, team.team_slug) / "payloads", refs)
    return _ArchiveIndex(
        lines=frozenset(lines),
        event_content=content,
        identifiers=frozenset(identifiers),
        input_payload_by_call=inputs,
        output_payload_by_call=outputs,
        resolved=resolved,
    )


def _verify_payload_row(
    payload: Mapping[str, object],
    index: _ArchiveIndex,
    *,
    is_output: bool,
) -> str | None:
    """Return why a tool row's text is not recoverable from the archive, or ``None`` if it is."""

    call_id = _string(payload.get("call_id"))
    if call_id is None:
        return "the row names no call_id, so the reader could not have built a tool call from it"
    slot = index.output_payload_by_call if is_output else index.input_payload_by_call
    if call_id not in slot:
        return f"no tool call in raw/team.json corresponds to call_id {call_id!r}"
    if is_output:
        expected = _textual(payload.get("output"))
    else:
        expected = _string(payload.get("input"))
        if expected is None:
            expected = _string(payload.get("arguments"))
    digest = slot[call_id]
    if expected is None:
        if digest is None:
            return None
        return f"call_id {call_id!r} references stored text for a row that carries none"
    if digest is None:
        return f"call_id {call_id!r} has no stored payload for text this row carries"
    stored = index.resolved.get(digest)
    if stored is None:
        return f"payload {digest} is referenced by call_id {call_id!r} but absent from the store"
    if stored != expected:
        return f"payload {digest} does not match the text this row carries"
    return None


_EXCERPT = 60


def _excerpt(value: str | None) -> str:
    """Render a text field for a finding without pasting a 45 KB command output into it."""

    if value is None:
        return "nothing"
    if len(value) <= _EXCERPT:
        return repr(value)
    return f"{value[:_EXCERPT]!r}... ({len(value)} characters)"


def _verify_event_content(
    context: _FileContext,
    line: int,
    index: _ArchiveIndex,
    expected: tuple[str | None, str | None],
) -> str | None:
    """Check that some Event at this line carries exactly the text this vendor row does.

    The comparison is against ``codex._content`` and ``payload["message"]``, the very extractors
    the reader used, and that is deliberate rather than a shortcut. What is under test here is not
    whether two parsers agree about the vendor format -- there is one parser -- but whether the
    bytes the reader extracted are still in the archive. Blanking every event's ``text``, which is
    exactly the shape of the drop this module exists to catch, changes the archive side and not
    the vendor side, so this catches it; re-implementing the extractor would catch the same thing
    and additionally produce false findings every time the two implementations drifted.
    """

    found = index.event_content.get((context.thread_id, line), ())
    if not found:
        return "no event in raw/team.json cites this line"
    if expected in found:
        return None
    text, encrypted = expected
    held_text, held_encrypted = found[0]
    if text is not None and held_text != text:
        return (
            f"the event at this line holds text {_excerpt(held_text)}, the row carries "
            f"{_excerpt(text)}"
        )
    return (
        f"the event at this line holds encrypted content {_excerpt(held_encrypted)}, the row "
        f"carries {_excerpt(encrypted)}"
    )


@dataclass(frozen=True)
class _FileContext:
    """The per-session facts a rule check can need beyond the row itself."""

    thread_id: str
    parent_thread_id: str | None


#: Returned by :func:`_verify` for a ``corpus-id`` rule, whose check cannot be answered until the
#: whole corpus has been read: the canonical row it joins to may live in a file the scan has not
#: reached. Deferring it keeps the scan to one pass over several gigabytes rather than two.
_DEFERRED = "\0deferred"


def _verify(
    rule: VendorRule,
    context: _FileContext,
    line: int,
    payload: Mapping[str, object],
    index: _ArchiveIndex,
) -> str | None:
    if rule.verification == "none":
        return None
    if rule.verification == "line":
        if (context.thread_id, line) in index.lines:
            return None
        return "no event, tool call or edge in raw/team.json cites this line"
    if rule.verification == "prompt-text":
        return _verify_event_content(
            context, line, index, (_string(payload.get("message")), None)
        )
    if rule.verification == "content-text":
        text, encrypted, _availability = _content(payload)
        return _verify_event_content(context, line, index, (text, encrypted))
    if rule.verification == "objective-text":
        objective = _string(_mapping(payload.get("goal")).get("objective"))
        found = index.event_content.get((context.thread_id, line), ())
        if not found:
            return "no event in raw/team.json cites this line"
        if objective is None:
            # The row names no objective, so there is no text for the event to have lost. The
            # citation is all this rule can claim, and it has it.
            return None
        if any(held is not None and objective in held for held, _encrypted in found):
            return None
        return (
            f"the event at this line does not carry the objective {_excerpt(objective)}; it "
            f"holds {_excerpt(found[0][0])}"
        )
    if rule.verification in ("input-payload", "output-payload"):
        return _verify_payload_row(
            payload, index, is_output=rule.verification == "output-payload"
        )
    if rule.verification == "parent-thread":
        if context.parent_thread_id is None:
            return "the session declares no parent, so nothing replayed its history into it"
        if context.parent_thread_id in index.identifiers:
            return None
        return (
            f"parent session {context.parent_thread_id!r} is not an agent in raw/team.json, so "
            "these replayed rows are canonical nowhere in this archive"
        )
    identifier = _identifier(payload, rule.id_path)
    if identifier is None:
        return f"the row carries no {'.'.join(rule.id_path)} to check against the model"
    if unscope_codex_id(identifier) in index.identifiers:
        return None
    if rule.verification == "corpus-id":
        return _DEFERRED
    return f"identifier {identifier!r} names nothing in raw/team.json"


@dataclass(frozen=True)
class _RecordedSource:
    """One rollout the source manifest says this team snapshotted, and how much of it."""

    snapshot_path: str
    copied_bytes: int
    sha256: str


def _recorded_sources(
    manifest: Mapping[str, JsonValue], where: str
) -> tuple[_RecordedSource, ...]:
    """Read the file set the manifest claims, which is what the corpus is measured against.

    Without this the audit's corpus is whatever ``rglob`` happens to find, and deletion becomes
    self-ratifying: remove one rollout and the rows in it are not enumerated, so they are not
    unaccounted, so the gate goes green -- the more you delete, the cleaner the report, until an
    emptied ``source_snapshots/`` directory reports "0 rows in 0 files: vendor snapshots are
    redundant" and exits 0 in front of an ``rm -rf``. An audit whose subject is defined by what
    survives cannot detect loss, only describe it.
    """

    recorded: list[_RecordedSource] = []
    for index, item in enumerate(as_array(manifest.get("sources"), f"{where}.sources")):
        entry = as_object(item, f"{where}.sources[{index}]")
        recorded.append(
            _RecordedSource(
                snapshot_path=as_string(
                    entry.get("snapshot_path"), f"{where}.sources[{index}].snapshot_path"
                ),
                copied_bytes=as_int(
                    entry.get("copied_bytes"), f"{where}.sources[{index}].copied_bytes"
                ),
                sha256=as_string(
                    entry.get("sha256"), f"{where}.sources[{index}].sha256"
                ),
            )
        )
    return tuple(recorded)


def _lineage_roots(manifest: Mapping[str, JsonValue], where: str) -> frozenset[str]:
    roots = {as_string(manifest.get("root_thread_id"), f"{where}.root_thread_id")}
    raw_links = manifest.get("continuation_sessions")
    if raw_links is not None:
        for index, link in enumerate(as_array(raw_links, f"{where}.continuation_sessions")):
            entry = as_object(link, f"{where}.continuation_sessions[{index}]")
            roots.add(as_string(entry.get("thread_id"), f"{where}.continuation_sessions"))
    return frozenset(roots)


@dataclass
class _Accumulator:
    rows: int = 0
    vendor_bytes: int = 0
    unverified: int = 0


@dataclass(frozen=True)
class _DeferredCheck:
    """A ``corpus-id`` check parked until the whole corpus has been enumerated."""

    rule_name: str
    identifier: str
    finding: RowFinding


def _parent_thread_id(metadata: Mapping[str, object]) -> str | None:
    spawn = _mapping(_mapping(_mapping(metadata.get("source")).get("subagent")).get("thread_spawn"))
    return _string(metadata.get("parent_thread_id")) or _string(spawn.get("parent_thread_id"))


def audit_codex_losslessness(archive: Path, team_slug: str) -> LosslessnessReport:
    """Account for every row of one Codex team's vendor snapshots against its archive.

    Reads only ``teams/<slug>/``: the source manifest for the file set and the lineage roots, the
    snapshots themselves, ``raw/team.json`` for the model, and the payload tree for the text. It
    takes no lock and writes nothing, so it is safe to run against an archive another process is
    building -- the worst a concurrent ingest can do is make the audit describe a slightly older
    generation, which is exactly what a read-only audit of a moving archive should do.
    """

    from wrkviz.pipeline import load_archived_team, snapshot_root_for

    team_root = _build_root(archive, team_slug)
    manifest_path = team_root / "raw" / "source-manifest.json"
    if not manifest_path.is_file():
        return _skipped(team_slug, "unknown", "the team has no raw/source-manifest.json")
    manifest = as_object(read_json(manifest_path), str(manifest_path))
    provider = as_string(manifest.get("provider"), f"{manifest_path}.provider")
    if provider != "codex":
        return _skipped(
            team_slug,
            provider,
            f"no rule table exists for provider {provider!r}; only codex is covered",
        )
    # Resolved rather than assumed: the vendor snapshots may be inside the archive, where an
    # older tool put them, or in the store beside it, where `migrate-snapshots` puts them. This
    # audit is the gate any future *deletion* of that tree has to pass, so an audit that could
    # only see one of the two locations would report a relocated archive as "already absent,
    # nothing to compare" -- a green result whose meaning is the opposite of green.
    snapshot_root = snapshot_root_for(archive, team_slug)
    if not snapshot_root.is_dir():
        return _skipped(
            team_slug,
            provider,
            "the vendor snapshots are already absent, so there is nothing left to compare "
            "the archive against",
        )

    team = load_archived_team(archive, team_slug)
    index = _build_index(archive, team)
    payload_problems = verify_payload_store(team_root / "payloads")
    roots = _lineage_roots(manifest, str(manifest_path))
    recorded_sources = _recorded_sources(manifest, str(manifest_path))
    recorded_by_path = {source.snapshot_path: source for source in recorded_sources}
    observed_paths: set[str] = set()
    source_problems: list[str] = []

    tallies: dict[str, _Accumulator] = {}
    rules_by_name = {rule.name: rule for rule in _CODEX_RULES}
    unaccounted: list[RowFinding] = []
    unverified: list[RowFinding] = []
    deferred: list[_DeferredCheck] = []
    corpus_ids: set[str] = set()
    unaccounted_rows = 0
    unverified_rows = 0
    vendor_files = 0
    vendor_rows = 0
    vendor_bytes = 0

    for path in sorted(snapshot_root.rglob("*.jsonl")):
        if path.is_symlink() or not path.is_file():
            continue
        vendor_files += 1
        relative = str(path.relative_to(snapshot_root))
        raw = path.read_bytes()
        # The file is checked against its manifest record while its bytes are already in hand.
        # The digest is the one comparison that catches a rollout truncated mid-way, which is
        # otherwise invisible: `_complete_prefix` silently drops an incomplete tail, so a file cut
        # in half enumerates cleanly and simply has fewer rows to account for.
        observed_paths.add(relative)
        recorded = recorded_by_path.get(relative)
        if recorded is None:
            source_problems.append(
                f"{relative}: present in source_snapshots/ but not recorded in "
                "raw/source-manifest.json"
            )
        elif len(raw) < recorded.copied_bytes:
            source_problems.append(
                f"{relative}: {len(raw)} bytes on disk, the manifest records "
                f"{recorded.copied_bytes}"
            )
        elif hashlib.sha256(raw[: recorded.copied_bytes]).hexdigest() != recorded.sha256:
            source_problems.append(
                f"{relative}: bytes do not match the digest recorded in raw/source-manifest.json"
            )
        records = _complete_records(_complete_prefix(raw), relative)
        if not records:
            continue
        metadata = _record_payload(records[0])
        context = _FileContext(
            thread_id=_string(metadata.get("id")) or "",
            parent_thread_id=_parent_thread_id(metadata),
        )
        # `_canonical_records` only ever compares its third argument for equality with this file's
        # thread id, so passing the empty string for a non-root session is exact rather than a
        # stand-in: no Codex thread id is empty, so the comparison is false, which is the answer.
        canonical = _canonical_records(
            records, metadata, context.thread_id if context.thread_id in roots else ""
        )
        canonical_start = canonical[0].line if canonical else len(records) + 1
        unbounded = not canonical and len(records) > 1
        # Which output row actually reaches the model for each call: the reader overwrites
        # `output_text` on every one it sees, so within a session the last wins. Computed up front
        # because the classifier meets the earlier rows first and must already know they were
        # superseded -- discovering it afterwards would mean re-tallying a row it had already
        # called a payload.
        last_output_line: dict[str, int] = {}
        for record in canonical:
            output_payload = _record_payload(record)
            if _string(output_payload.get("type")) in _OUTPUT_TYPES:
                output_call = _string(output_payload.get("call_id"))
                if output_call is not None:
                    last_output_line[output_call] = record.line
        for record in records:
            vendor_rows += 1
            row_bytes = len(record.raw)
            vendor_bytes += row_bytes
            top_type = _string(record.value.get("type"))
            payload = _record_payload(record)
            payload_type = _string(payload.get("type"))
            if record.line == 1 and top_type == "session_meta":
                rule: VendorRule | None = _select(
                    _RULES_BY_SHAPE[("session_meta", None)], payload
                )
            elif unbounded:
                rule = _UNBOUNDED_RULE
            elif record.line < canonical_start:
                rule = _PREFIX_RULE
            elif top_type is None:
                rule = None
            elif (
                payload_type in _OUTPUT_TYPES
                and last_output_line.get(_string(payload.get("call_id")) or "") != record.line
            ):
                rule = _SUPERSEDED_RULE
            else:
                candidates = _RULES_BY_SHAPE.get((top_type, payload_type))
                if candidates is None and payload_type is not None:
                    candidates = _RULES_BY_SHAPE.get((top_type, None))
                rule = _select(candidates, payload) if candidates else None
            # Canonical response items are what an `item_completed` row claims to restate, so the
            # identifiers they carry are collected as the scan goes and the claim is settled once
            # every file has been seen. Collected before the rule is even consulted, because a row
            # the table cannot classify still carries an id, and refusing to notice it would make
            # one unaccounted row cascade into a second false unverified finding.
            if top_type == "response_item" and record.line >= canonical_start and not unbounded:
                for key in ("id", "call_id"):
                    value = _string(payload.get(key))
                    if value is not None:
                        corpus_ids.add(value)
            if rule is None:
                unaccounted_rows += 1
                if len(unaccounted) < _SAMPLE_LIMIT:
                    unaccounted.append(
                        RowFinding(
                            source_path=relative,
                            line=record.line,
                            top_type=top_type,
                            payload_type=payload_type,
                            vendor_bytes=row_bytes,
                            detail=(
                                "no rule in the Codex table accounts for this row shape; add one "
                                "that says what becomes of it before trusting any inventory below"
                            ),
                        )
                    )
                continue
            accumulator = tallies.setdefault(rule.name, _Accumulator())
            accumulator.rows += 1
            accumulator.vendor_bytes += row_bytes
            failure = _verify(rule, context, record.line, payload, index)
            if failure is None:
                continue
            finding = RowFinding(
                source_path=relative,
                line=record.line,
                top_type=top_type,
                payload_type=payload_type,
                vendor_bytes=row_bytes,
                detail="",
            )
            if failure is _DEFERRED:
                identifier = _identifier(payload, rule.id_path)
                assert identifier is not None  # `_verify` returns _DEFERRED only when it resolved
                deferred.append(_DeferredCheck(rule.name, identifier, finding))
                continue
            accumulator.unverified += 1
            unverified_rows += 1
            if len(unverified) < _SAMPLE_LIMIT:
                unverified.append(
                    replace(finding, detail=f"rule {rule.name!r} claims otherwise: {failure}")
                )

    for source in recorded_sources:
        if source.snapshot_path not in observed_paths:
            source_problems.append(
                f"{source.snapshot_path}: recorded in raw/source-manifest.json but absent from "
                "source_snapshots/"
            )

    for check in deferred:
        if check.identifier in corpus_ids:
            continue
        tallies[check.rule_name].unverified += 1
        unverified_rows += 1
        if len(unverified) < _SAMPLE_LIMIT:
            unverified.append(
                replace(
                    check.finding,
                    detail=(
                        f"rule {check.rule_name!r} claims otherwise: identifier "
                        f"{check.identifier!r} appears on no canonical row anywhere in the corpus "
                        "and names nothing in raw/team.json, so this row restates nothing"
                    ),
                )
            )

    ordered = tuple(
        RuleTally(
            name=name,
            disposition=rules_by_name[name].disposition,
            rows=accumulator.rows,
            vendor_bytes=accumulator.vendor_bytes,
            unverified=accumulator.unverified,
            blocks_deletion=rules_by_name[name].blocks_deletion,
            reason=rules_by_name[name].reason,
        )
        for name, accumulator in sorted(
            tallies.items(), key=lambda item: (-item[1].vendor_bytes, item[0])
        )
    )
    return LosslessnessReport(
        team_slug=team_slug,
        provider=provider,
        covered=True,
        vendor_files=vendor_files,
        vendor_rows=vendor_rows,
        vendor_bytes=vendor_bytes,
        absent_rows=sum(tally.rows for tally in ordered if tally.disposition == "dropped"),
        absent_bytes=sum(
            tally.vendor_bytes for tally in ordered if tally.disposition == "dropped"
        ),
        partial_rows=sum(tally.rows for tally in ordered if tally.disposition == "partial"),
        partial_bytes=sum(
            tally.vendor_bytes for tally in ordered if tally.disposition == "partial"
        ),
        tallies=ordered,
        unaccounted=tuple(unaccounted),
        unaccounted_rows=unaccounted_rows,
        unverified=tuple(unverified),
        unverified_rows=unverified_rows,
        payload_problems=payload_problems,
        source_problems=tuple(sorted(source_problems)),
    )


def _skipped(team_slug: str, provider: str, reason: str) -> LosslessnessReport:
    return LosslessnessReport(
        team_slug=team_slug,
        provider=provider,
        covered=False,
        vendor_files=0,
        vendor_rows=0,
        vendor_bytes=0,
        absent_rows=0,
        absent_bytes=0,
        partial_rows=0,
        partial_bytes=0,
        tallies=(),
        unaccounted=(),
        unaccounted_rows=0,
        unverified=(),
        unverified_rows=0,
        payload_problems=(),
        skipped=reason,
    )


def audit_archive_losslessness(
    archive: Path, team_slugs: Sequence[str] = ()
) -> ArchiveLosslessnessReport:
    """Audit the selected teams, or every ingested team when the selection is empty.

    An uncovered team is reported, never omitted. A report that listed only the Codex teams would
    let an operator read "everything is accounted for" off a screen that never mentioned the two
    Orc teams holding 2.5 GB of snapshots, and that is the specific way this kind of tool lies.
    """

    selected = tuple(team_slugs)
    if not selected:
        selected = ingested_team_slugs(archive)
    if not selected:
        raise ValueError(f"no ingested teams found in {archive}")
    if len(set(selected)) != len(selected):
        raise ValueError("losslessness audit team selection contains duplicates")
    return ArchiveLosslessnessReport(
        reports=tuple(audit_codex_losslessness(archive, slug) for slug in selected)
    )


def _mib(value: int) -> str:
    return f"{value / (1024 * 1024):.1f} MiB"


def format_losslessness_audit(
    report: ArchiveLosslessnessReport, output_format: Literal["text", "json"]
) -> str:
    """Render the audit for a terminal or for a machine."""

    if output_format == "json":
        return canonical_json(report.to_json_obj())
    lines: list[str] = []
    for team in report.reports:
        lines.append(f"{team.team_slug} [{team.provider}]")
        if not team.covered:
            lines.append(f"  not covered: {team.skipped}")
            lines.append("")
            continue
        verdict = (
            "vendor snapshots are redundant"
            if team.lossless
            else (
                "vendor snapshots still hold content the archive does not"
                if team.sound
                else "AUDIT FAILED -- the archive's account of itself does not hold"
            )
        )
        lines.append(
            f"  {team.vendor_rows:,} rows in {team.vendor_files} files, "
            f"{_mib(team.vendor_bytes)}: {verdict}"
        )
        if team.absent_rows:
            lines.append(
                f"  content absent from the archive: {team.absent_rows:,} rows, "
                f"{_mib(team.absent_bytes)} "
                f"({100 * team.absent_bytes / max(team.vendor_bytes, 1):.1f}% of the corpus)"
            )
        if team.partial_rows:
            lines.append(
                f"  content only partly held: {team.partial_rows:,} rows, up to "
                f"{_mib(team.partial_bytes)} "
                f"({100 * team.partial_bytes / max(team.vendor_bytes, 1):.1f}% of the corpus; "
                "whole-row attribution, so an upper bound)"
            )
        for tally in team.tallies:
            marker = "!" if tally.blocks_deletion else " "
            lines.append(
                f"  {marker} {tally.name:<26} {tally.disposition:<9} "
                f"{tally.rows:>9,} rows {_mib(tally.vendor_bytes):>12}"
                + (f"  {tally.unverified:,} UNVERIFIED" if tally.unverified else "")
            )
            if tally.blocks_deletion:
                lines.append(f"      {tally.reason}")
        for problem in team.source_problems:
            lines.append(f"  vendor snapshots: {problem}")
        for problem in team.payload_problems:
            lines.append(f"  payload store: {problem}")
        for finding in team.unaccounted:
            lines.append(
                f"  UNACCOUNTED {finding.source_path}:{finding.line} "
                f"({finding.top_type}/{finding.payload_type}): {finding.detail}"
            )
        if team.unaccounted_rows > len(team.unaccounted):
            lines.append(
                f"  ... {team.unaccounted_rows - len(team.unaccounted):,} further "
                "unaccounted rows"
            )
        for finding in team.unverified:
            lines.append(
                f"  UNVERIFIED {finding.source_path}:{finding.line} "
                f"({finding.top_type}/{finding.payload_type}): {finding.detail}"
            )
        if team.unverified_rows > len(team.unverified):
            lines.append(
                f"  ... {team.unverified_rows - len(team.unverified):,} further unverified rows"
            )
        lines.append("")
    lines.append(
        "sound: "
        + ("yes" if report.sound else "NO")
        + "; lossless: "
        + ("yes" if report.lossless else "no")
    )
    return "\n".join(lines) + "\n"


__all__ = [
    "AUDIT_SCHEMA_VERSION",
    "ArchiveLosslessnessReport",
    "Disposition",
    "LosslessnessReport",
    "RowFinding",
    "RuleTally",
    "VendorRule",
    "audit_archive_losslessness",
    "audit_codex_losslessness",
    "format_losslessness_audit",
    "unscope_codex_id",
]
