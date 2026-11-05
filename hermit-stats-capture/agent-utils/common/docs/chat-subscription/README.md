# Ordered chat subscriptions

This Rust library defines a provider-neutral contract for ordered inbound chat
events. It keeps an opaque provider replay cursor distinct from a live delivery
identifier, groups events by their upstream acknowledgement boundary, and
allows only one uncommitted delivery by default.

The host persists every child event and the accompanying provider cursor before
calling `commit_durable`. A successful commit is the backend's permission to
acknowledge the upstream delivery or advance its local replay position. A
disconnect before commit leaves the batch unacknowledged. Typed checkpoint and
gap events make initial-head and retention-loss boundaries explicit.

The object-safe traits support statically linked provider implementations.
Provider event loops supply bounded in-memory values and do not persist host
state through this API. Normalized routing fields can carry a bounded opaque
provider payload so a provider package can retain its typed full resource.
The 262,144-byte provider-payload bound covers the complete compact JSON
`{"schema":...,"data":...}` envelope, and `ProviderPayload::encoded_bytes`
reports that same complete size.
Batch limits count compact JSON encoding and string escaping rather than raw
UTF-8 alone, so bounded process adapters accept the same domain values.
The host may also attach a bounded, schema-identified JSON configuration object
for non-secret resource names. Credentials remain outside the subscription
contract. A process host may inherit credential values through a separate,
operator-controlled allowlist of environment-variable names; neither this
request object nor a provider manifest can select or persist those values.
