# Vibe-talk UI responsiveness audit

This audit covers the `/voice` client. “Immediate” means the visible state changes in the same
event turn from data already in memory or browser storage. A background request may follow, but it
must not delay the feedback.

| Interaction | Current contract | Network work | Failure behavior |
|---|---|---|---|
| Voice / channel switch, Settings, Help, Back | Immediate | None | None |
| Main / Threads / All / open thread | Immediate after that context's first fetch | First visit fetches one timeline page; later switches use the in-memory context cache | Loading text on first visit; cached context remains if refresh fails |
| Search, fold/unfold, expand/collapse all, message size, reading width, bar placement | Immediate | None | Storage refusal is reported for persisted preferences |
| Hide read / Showing | Immediate local projection of the loaded timeline | None on the deployed threaded backend | The cached list remains usable |
| Mark my own messages read, message combining, audio source, pace popover | Immediate | None; agent pace change may prepare new speech tickets in background | Preference remains active for the page if browser storage refuses it |
| Done / Unarchive / swipe / Undo | Immediate optimistic archive state | Ordered local-store POST in background | Roll back the affected ids and show a dismissible error if persistence fails |
| Clear backlog | First tap arms locally; second hides locally | One ordered dismiss POST; the legacy boundary form may read the provider | Roll back on persistence failure |
| Device read-aloud | Immediate pending/playing state | No application request; the browser/device speech engine may have its own implementation | Row and banner name device-voice failures |
| Agent read-aloud | Immediate pending state | Speech preparation plus agent synthesis/audio | Row leaves pending state; actionable provider error is dismissible |
| Channel refresh, pull refresh, Older messages, Earlier turns | Loading state is immediate | Required provider/store read | Existing content remains; error banner can be dismissed and expires |
| Summaries | Mode and row pending state are immediate | One model request per uncached visible message | Per-row failed state; original text remains |
| Send channel message / reply | Optimistic outgoing row and cleared composer | Required provider write | Durable retryable or unconfirmed receipt; never automatic replay |
| Retry outgoing message | Immediate sending state | Required provider write | Returns to retryable/unconfirmed state |
| Rename, add, remove channel | Control has local open/close state; saved result requires server | Required configuration/provider mutation | Existing channel configuration remains visible |
| Talk / Start call / typed agent turn | Working/connecting or queued state is immediate | Voice-session request and WebSocket/provider work | Provider-neutral, dismissible error plus connection detail |
| Mute, speaker audio, canned prompt tray, text-entry mode | Immediate | Mute/prompt may send a frame on an already-open socket | Local state remains legible if socket later closes |
| Upstream “mark read through here” | Immediate working feedback only | Required source-provider mutation | Local Done/archive state is unchanged |
| Save/forget token, dismiss status/error/banner | Immediate | Save verifies the token with one config request; dismiss/forget are local | Save button and inline state report the result |

The 2026-09-24 live failure was a mismatch with this contract. Each Done request called the chat
provider again to validate ids. About two dozen quick dismissals became concurrent five-second
message reads and then 20-second 502s. Explicit dismissals now validate the configured channel and
write only the local store, with a one-page batch ceiling. The browser also serializes these writes
while applying each visible change immediately.

The main remaining unavoidable waits are first-load/provider refresh, outgoing messages, model
summaries, new voice sessions, and agent-generated audio. Each of those must expose a working state
without replacing already cached content. Browser-only presentation and preference actions must
issue no request.
