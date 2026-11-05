//! `vibe-talk` server entry point.

use std::future::IntoFuture;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use vibe_talk::agent_backend::NoAgentBackend;
use vibe_talk::chat::ChatClient;
use vibe_talk::config::{Config, ReadAloudBackend, ENV_CONFIG_OVERLAY, ENV_CONFIG_PATH};
use vibe_talk::discord::fake::FakeDiscord;
use vibe_talk::elevenlabs::http::HttpElevenLabsClient;
use vibe_talk::elevenlabs::SignedUrlProvider;
use vibe_talk::probe::{self, ENV_SKIP_STARTUP_PROBE};
use vibe_talk::providers::ChatRouter;
use vibe_talk::retrieval::LexicalRanker;
use vibe_talk::speech::SpeechProvider;
use vibe_talk::state::AppState;
use vibe_talk::store::disabled::DisabledStore;
use vibe_talk::store::sqlite::SqliteStore;
use vibe_talk::store::StateStore;

/// Long enough for ordinary requests to finish, but shorter than an operator's stop timeout.
/// Streaming responses can remain open forever, so graceful shutdown itself must have a bound.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

const USAGE: &str = "\
vibe-talk — a chat bridge for a voice agent

USAGE:
    vibe-talk [--config PATH] [--config-overlay PATH]... [--fake-discord] [--skip-startup-probe]

OPTIONS:
    --config PATH         configuration file (default: $VIBE_TALK_CONFIG, else ./vibe-talk.toml)
    --config-overlay PATH a TOML file laid over the configuration file, for settings one host
                          needs and a shared file must not carry (for example its own
                          [summaries.llm]). Repeatable; applied in order. Its tables MERGE into the
                          configuration key by key; a key or an array it sets replaces the base's,
                          so an overlay that writes [[channels]] replaces the whole channel list.
                          [summaries.llm] is replaced WHOLE: an overlay naming it inherits none of
                          the base's keys, a key file included. Also settable as
                          VIBE_TALK_CONFIG_OVERLAY=PATH[:PATH...], applied after the flags. A named
                          overlay must exist.
    --fake-discord        run against an in-memory Discord with seeded messages, for local
                          development
    --skip-startup-probe  do not check at startup that each configured channel is readable.
                          Also settable as VIBE_TALK_SKIP_STARTUP_PROBE=1. The skip is logged
                          loudly, because an unchecked channel surfaces later as an empty result
                          that looks like a bug in this server.
    --version             print the version and exit
    --help                print this message and exit

Every secret is read from the configuration file or from the environment. See the project README
for the full list and for the threat model.
";

/// The backlog `--fake-discord` seeds into every configured channel.
///
/// Deliberately long-winded and repetitive, because that is the problem this project exists for:
/// a driver returns to a dozen verbose agent messages and cannot skim them. Two cheerful one-liners
/// would make the digest and the scrollback look like they work when neither had anything to do.
/// Several entries also share vocabulary ("runner", "cache", "token") so that
/// `POST /resolve` is exercised against real competition rather than a single obvious hit.
const SEEDED_BACKLOG: &[(&str, &str)] = &[
    (
        "codex-eng",
        "seeded: starting on the retry-budget work. Branch is codex/retry-budget off integration \
         at 4f21ab0. I'll push as soon as the first commit is coherent.",
    ),
    (
        "codex-eng",
        "seeded: first cut pushed. The budget is per-host rather than per-request, which is the \
         only shape that survives a fan-out; a per-request budget lets N parallel calls each spend \
         the whole allowance. Focused tests pass locally (14 of 14 in the retry module).",
    ),
    (
        "claude-integ",
        "seeded: heads up, the mac runner went offline mid-deploy so the arm64 job never reported. \
         The run shows green at the run level but the job list has one queued job that never \
         started, so that green is not evidence of anything. Re-firing.",
    ),
    (
        "claude-integ",
        "seeded: re-fired. Same runner, same stall. I think the runner pool is down to one mac and \
         it is wedged rather than busy - queue wait is 41 minutes and climbing with nothing \
         executing.",
    ),
    (
        "codex-review",
        "seeded: review of the retry-budget branch. One real finding: the budget is consulted \
         before the jitter is applied, so under contention every caller wakes at the same instant \
         and the budget is spent in one burst. Suggest consulting it after the sleep.",
    ),
    (
        "codex-eng",
        "seeded: good catch, fixed and pushed. Also removed the cache-key rewrite I snuck in - it \
         was unrelated to this branch and it belongs in its own change.",
    ),
    (
        "build-bot",
        "seeded: nightly cache rebuild finished in 22m14s, 3.1 GB written, 0 evictions. Previous \
         night was 24m02s so this is noise, not an improvement.",
    ),
    (
        "build-bot",
        "seeded: WARNING - the token used by the release job expires in 6 days. Rotating it \
         requires the maintainer account; nothing automated can do it.",
    ),
    (
        "claude-integ",
        "seeded: the arm64 job finally reported after the runner was recycled by hand. Green, and \
         this one has a real job list: 11 completed, 0 skipped. Landing the retry-budget branch on \
         that evidence.",
    ),
    (
        "claude-integ",
        "seeded: landed. integration is at 9c07d3e and the tip is green. Branch deleted; the PR is \
         closed as merged rather than closed unmerged, which matters for the changelog script.",
    ),
    (
        "codex-eng",
        "seeded: next up is the cache-key rewrite I pulled out earlier. It is small but it changes \
         a durable format, so it is a sequential change - nobody else should touch the cache \
         version namespace while it is in flight.",
    ),
    (
        "claude-qa",
        "seeded: filed two issues from the overnight sweep. The first is a genuine flake - a test \
         that depends on wall-clock ordering between two spawned processes. The second is not a \
         flake, it is a real ordering bug that only shows up when the machine is loaded, and I \
         would rather we not label it flaky because that is how it gets ignored.",
    ),
    // `#217 markdown-blocks`: the shape of message the page used to draw as literal dashes with
    // the closing lines run together — a paragraph, a list whose items run on to an indented line,
    // a blank line, and two closing paragraphs. The server renders it, so the screenshot walk
    // photographs the server's rendering (`41-message-blocks`). Under the summary threshold below,
    // so the long entry after it is still the only one a summary is written for, and over the
    // page's fold, as such an answer is.
    (
        "claude-integ",
        "seeded: where the release stands this morning:\n\n\
         - the build is green on both runners\n\
         - the flaky ordering test is quarantined\n  and has an issue filed against it\n\
         - the docs are updated\n\
         - the release notes are drafted\n\
         - the tag is not pushed yet\n  because it waits on your review\n\
         - nothing else is open\n\n\n\
         I will push the tag when you say so.\n\nThanks!",
    ),
    // `#237 nested-lists`: numbered steps with their sub-steps indented two spaces, as chat writes
    // them, three levels deep in one place and with two-digit numbers. CommonMark nests only at an
    // item's text — three columns in under `9. `, four under `10. ` — so this was drawn with every
    // sub-step level with the steps, and the second step's own `1.` and `2.` numbered eleven and
    // twelve. Photographed in `46-nested-lists`. Under the summary threshold, and older than the
    // long entry below, which must stay the newest.
    (
        "claude-integ",
        "seeded: steps 9 to 11 of the release checklist, and where each stands:\n\n\
         9. finish the key rewrite\n  - fold in the feature set\n  - version the key\n    \
         - old entries age out within the week\n\
         10. land it behind the flag\n  1. one machine first\n  2. then all four\n\
         11. write the release notes\n  - needs the maintainer's sign-off",
    ),
    // OVER `summaries.threshold_chars`, which is 400 by default, and it is the only entry here
    // that is. Everything above is between 140 and 350 characters, so against this backlog every
    // summary the page asked for came back `below_threshold` and `#49 cached-summaries` could not
    // be exercised locally AT ALL — the seam existed, the endpoint answered, and nothing a
    // developer could see ever produced a summary. It is also the honest case: the message this
    // whole project exists for is the one you cannot skim at the roadside.
    //
    // NOTE that `--fake-discord` fakes DISCORD and not ElevenLabs, so on a machine with no
    // credentials this row now comes back FAILED and red rather than truncated — the extractive
    // summariser that used to answer it offline is gone. To see the summarised state locally,
    // run the `vibe-talk-mock-elevenlabs` binary and point `elevenlabs.api_base` at it; that is
    // what `scripts/run.sh` does for the screenshot scenes.
    (
        "codex-eng",
        "seeded: long one, sorry. The cache-key rewrite is done and I want to write down what \
         changed before I forget it. The key used to be the source path plus the compiler \
         version, which looked complete and was not: two builds of the same file with different \
         feature flags hashed identically, so the second one silently reused the first one's \
         object and the flags did nothing. That is the bug behind the three 'it works on my \
         machine' reports from last month, and it explains why clearing the cache always fixed \
         them. The new key folds in the feature set, the target triple and the optimisation \
         level, and it is versioned, so the moment any of that changes every old entry becomes \
         unreachable at once rather than being served under rules that no longer exist. I did \
         NOT make it a cryptographic hash - it is a change detector, not a defence, and calling \
         it a defence would invite somebody to rely on it as one. The migration is a no-op: old \
         entries are simply never hit again and the retention sweep collects them within the \
         week. One thing I want a second opinion on before this lands: the optimisation level is \
         in the key, which means a debug build and a release build no longer share anything, and \
         on this repository that roughly doubles the cache on disk.",
    ),
];

struct Args {
    config: Option<PathBuf>,
    overlays: Vec<PathBuf>,
    fake_discord: bool,
    skip_startup_probe: bool,
}

fn parse_args() -> anyhow::Result<Option<Args>> {
    let mut args = Args {
        config: None,
        overlays: Vec::new(),
        fake_discord: false,
        skip_startup_probe: false,
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "--version" => {
                println!("vibe-talk {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--config" => {
                let value = argv.next().context("--config needs a path")?;
                args.config = Some(PathBuf::from(value));
            }
            "--config-overlay" => {
                let value = argv.next().context("--config-overlay needs a path")?;
                args.overlays.push(PathBuf::from(value));
            }
            "--fake-discord" => args.fake_discord = true,
            "--skip-startup-probe" => args.skip_startup_probe = true,
            other => anyhow::bail!("unrecognized argument {other:?}\n\n{USAGE}"),
        }
    }
    Ok(Some(args))
}

/// Re-key the summaries the previous policy version filed, then sweep every other old version.
/// `#208 persistent-cache`, milestone `summary-keep`.
///
/// Before it, the version hashed the threshold, the reply deadline and the cap as well as the
/// wording, so the rows under `policy_version_v1` of THIS configuration were produced under
/// exactly the wording `policy_version_v2` names: they are moved, not deleted, and the ones that
/// end "…" are marked as cut at that version's cap. Every other old version is then swept as
/// before. On every later start the re-key finds nothing and the sweep finds nothing, so a restart
/// never makes a summary be asked for again. A deploy that also changes the wording names a v1 no
/// row is under, and its rows are swept, which is right.
///
/// `current` is the version this server files under, computed once by the caller.
///
/// One line at INFO with the two counts and nothing else: no key, no id, no text.
async fn keep_summaries_across_the_policy_change(
    store: &dyn StateStore,
    config: &vibe_talk::config::Config,
    summarizer: &dyn vibe_talk::summarize::Summarizer,
    current: &str,
) {
    let previous = vibe_talk::summarize::policy_version_v1_for(&config.summaries, summarizer);
    let cap_then = u32::try_from(vibe_talk::summarize::hard_cap_chars(
        config.summaries.target_chars,
    ))
    .unwrap_or(u32::MAX);
    let rekeyed = match store.rekey_summaries(&previous, current, cap_then).await {
        Ok(moved) => moved,
        // Not configured is not a failure: there is no cache, so there is nothing to keep or
        // sweep.
        Err(vibe_talk::store::StoreError::Unavailable(_)) => return,
        Err(error) => {
            // The sweep below would then delete what could not be moved; skipping it leaves the
            // old rows unreachable until the next start retries, which costs nothing.
            tracing::warn!(
                %error,
                "the cached summaries of the previous policy version could NOT be re-keyed; they \
                 are left in place and the re-key is retried at the next start"
            );
            return;
        }
    };
    // The sweep. Without it a changed policy leaves the old entries on disk forever: unreachable,
    // invisible, and still a copy of other people's text at rest.
    match store.forget_summaries_except(current).await {
        // Every start, zeros included: "re-keyed=0 swept=0" is the evidence that a restart kept
        // every summary it had.
        Ok(swept) => tracing::info!(
            "re-keyed" = rekeyed,
            swept,
            "kept the cached summaries whose wording is unchanged and deleted those produced \
             under an older wording"
        ),
        // A store that IS configured and could not be swept is a real fault and has to be
        // visible at the default RUST_LOG=info, because what it leaves behind is other people's
        // text at rest under a policy that no longer applies.
        Err(vibe_talk::store::StoreError::Unavailable(_)) => {}
        Err(error) => tracing::warn!(
            %error,
            "re-keyed" = rekeyed,
            "the summary cache could NOT be swept; entries produced under an older policy are \
             still on disk"
        ),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let Some(args) = parse_args()? else {
        return Ok(());
    };

    // An explicitly named file must exist. A defaulted one need not: a container is often given
    // everything through the environment, and that is a supported deployment rather than an error.
    // It is announced loudly either way, because silently running on a different configuration
    // than the operator believes is the failure mode worth preventing.
    let explicit = args.config.is_some();
    let config_path = args
        .config
        .or_else(|| std::env::var_os(ENV_CONFIG_PATH).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("vibe-talk.toml"));
    // Overlays: the flags in order, then the environment's. Every one is named in the log, because
    // a setting that came from a file the operator forgot about is the hardest kind to find.
    let mut overlays = args.overlays;
    if let Some(listed) = std::env::var_os(ENV_CONFIG_OVERLAY) {
        overlays.extend(std::env::split_paths(&listed).filter(|path| !path.as_os_str().is_empty()));
    }
    for overlay in &overlays {
        if !overlay.exists() {
            anyhow::bail!("configuration overlay {} does not exist", overlay.display());
        }
        tracing::info!(path = %overlay.display(), "laying a configuration overlay over the base");
    }
    let config = if config_path.exists() {
        tracing::info!(path = %config_path.display(), "reading configuration file");
        Config::load_layered(Some(&config_path), &overlays)
            .with_context(|| format!("loading configuration from {}", config_path.display()))?
    } else if explicit {
        anyhow::bail!(
            "configuration file {} does not exist",
            config_path.display()
        );
    } else {
        tracing::warn!(
            path = %config_path.display(),
            "no configuration file found; taking the ENTIRE configuration from the environment"
        );
        Config::load_layered(None, &overlays)
            .context("assembling configuration from the environment alone")?
    };

    let providers: Arc<ChatRouter> = if args.fake_discord {
        tracing::warn!(
            "--fake-discord: serving an IN-MEMORY Discord. Nothing is read from or posted to a \
             real channel."
        );
        let fake = Arc::new(FakeDiscord::new());
        for channel in &config.channels {
            let mut seeded = Vec::new();
            for (author, content) in SEEDED_BACKLOG {
                seeded.push(fake.seed(&channel.id, author, content));
            }
            // A couple of the seeded messages ANSWER earlier ones, because a channel in which
            // nothing has been replied to cannot exercise the inbox view at all: every row would
            // be open, and "replied dims the row" would look identical to "the feature is not
            // wired up". Indices are into SEEDED_BACKLOG and chosen where the text really does
            // read as a reply to the earlier message.
            for (answer, question) in [(5_usize, 4_usize), (9, 3)] {
                if let (Some(a), Some(q)) = (seeded.get(answer), seeded.get(question)) {
                    fake.set_reply_to(a, q);
                }
            }
        }
        Arc::new(ChatRouter::single(fake, "fake", config.live_poll_seconds()))
    } else {
        Arc::new(ChatRouter::from_config(&config).context("building the chat providers")?)
    };
    let chat: Arc<dyn ChatClient> = providers.clone();
    // Learn each provider's own account in the background, so the page can draw this server's
    // posts as the owner's from the first render. A provider that cannot answer yet is reported
    // by diagnostics and by the startup probe; it does not hold up the server.
    for entry in providers.entries() {
        let client = Arc::clone(&entry.client);
        let key = entry.key.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(std::time::Duration::from_secs(30), client.identity()).await
            {
                Ok(Ok(identity)) => tracing::info!(
                    provider = key,
                    account = identity.id,
                    "chat provider identified its account"
                ),
                Ok(Err(error)) => {
                    tracing::warn!(provider = key, %error, "chat provider could not identify its account");
                }
                Err(_) => tracing::warn!(provider = key, "chat provider identity check timed out"),
            }
        });
    }

    // Open the store BEFORE the probe and before the listener: a storage path that cannot be
    // created is a configuration error, and a server that came up and only discovered it on the
    // first attempt to record a turn would lose that turn and say nothing useful about why.
    let store: Arc<dyn StateStore> = match &config.storage.path {
        Some(path) => {
            let opened = SqliteStore::open(path, config.storage.retention).with_context(|| {
                format!(
                    "opening the durable state store at {}. It must be an absolute path this \
                     process can write to — in a container, a mounted VOLUME rather than a \
                     directory in the image.",
                    path.display()
                )
            })?;
            let retention = config.storage.retention;
            tracing::info!(
                path = %path.display(),
                provider = chat.provider_name(),
                max_conversations = retention.max_conversations,
                max_turns_per_conversation = retention.max_turns_per_conversation,
                max_summaries = retention.max_summaries,
                retain_days = retention.retain_days,
                summary_retain_days = retention.summary_retain_days,
                "durable state is ON. Conversation transcripts and read marks are written to \
                 this file at 0600 and survive a restart. Read marks are THIS SERVER'S OWN: \
                 nothing is read from or written back to the chat provider."
            );
            Arc::new(opened)
        }
        None => {
            tracing::warn!(
                setting = vibe_talk::store::disabled::SETTING,
                "durable state is OFF. This server keeps NOTHING between restarts, and the \
                 routes that need it refuse with 503 storage_not_configured rather than \
                 pretending. Set an absolute path on a mounted volume to turn it on."
            );
            Arc::new(DisabledStore)
        }
    };

    run_startup_probe(chat.as_ref(), &config, args.skip_startup_probe).await?;

    for channel in &config.channels {
        tracing::info!(
            channel = %channel.id,
            label = %channel.label,
            writable = channel.writable,
            "configured channel"
        );
    }
    // Say the zone out loud. Leaving `server.timezone` unset is not an error and must not be, but
    // it means every time the agent speaks is UTC — and a wrong-but-plausible clock is precisely
    // the failure this setting exists to prevent, so it should not have to be inferred from a
    // digest read out in the car.
    tracing::info!(
        timezone = config.timezone.name(),
        "message times will be spoken in this zone"
    );
    // ElevenLabs credentials are relevant to live voice or read-aloud only when one of those
    // selected providers uses them. The summary backend reports its own configuration below.
    if config.conversation.backend == vibe_talk::config::ConversationBackend::ElevenLabs
        || config.read_aloud.backend == ReadAloudBackend::ElevenLabs
    {
        match vibe_talk::elevenlabs::credentials(&config.elevenlabs) {
            Ok((agent_id, _key)) => tracing::info!(
                agent_id,
                "ElevenLabs is configured for a selected voice capability"
            ),
            Err(error) => tracing::warn!(
                %error,
                "a selected ElevenLabs voice capability is not configured"
            ),
        }
    }
    // Say the MCP endpoint out loud at startup: an agent platform has to be told a URL, and the
    // most common deployment mistake is pointing it at the wrong path and getting a bare 404.
    tracing::info!(
        path = vibe_talk::mcp::transport::MCP_PATH,
        protocol = vibe_talk::mcp::protocol::PROTOCOL_VERSION,
        public_base_url = config
            .public_base_url
            .as_deref()
            .unwrap_or("(not configured)"),
        "MCP Streamable HTTP endpoint mounted; it requires the same bearer tokens as the API"
    );

    // Say the access log exists, at startup, in the log itself. It is what makes an EMPTY log a
    // finding rather than an ambiguity: a reader who sees no request lines below this banner
    // knows that no client called, rather than wondering whether this server logs requests at all.
    tracing::info!(
        target: "vibe_talk::access",
        "per-request access logging is ON at INFO. Every HTTP request, every MCP JSON-RPC \
         message, and every tool call leaves a line below. If you see NO lines below this one, \
         nothing called this server -- that is a finding, not a gap in the log. Tokens are never \
         logged; message content is DEBUG only."
    );

    // ONE client, three capabilities. Building a second would mean two connection pools and two
    // user agents against the same vendor for no reason. Built here, before the summariser,
    // because the agent-backed summariser mints its conversations through this same client.
    let elevenlabs_client =
        Arc::new(HttpElevenLabsClient::new().context("building the ElevenLabs client")?);

    // ONE summariser, built in ONE place — `summarize::summarizer_for`, which the tests call too,
    // so nothing here can drift from what a test believes the binary built: the direct Messages
    // API call when `[summaries.llm]` is configured, else the ElevenLabs agent. Say which it is
    // and under which policy version: the version is what a person with a shell greps the cache
    // by when they suspect a stale entry.
    let summarizer = vibe_talk::summarize::summarizer_for(
        &config.elevenlabs,
        &config.summaries,
        Arc::new(
            vibe_talk::elevenlabs::socket::WebSocketTextChatProvider::new(Arc::clone(
                &elevenlabs_client,
            )
                as Arc<dyn SignedUrlProvider>),
        ),
    );
    // Unconfigured is a warning here and a per-request refusal there, never a refusal to start:
    // the chat half of this server is useful on its own, and a deployment that mistyped a key
    // should still come up far enough to say so. There is no summariser that needs no vendor —
    // this IS the path that produces the failed rows.
    let summaries_unavailable = summarizer.unavailable_reason();
    if let Some(error) = &summaries_unavailable {
        if config.summaries.llm.is_some() {
            tracing::warn!(
                %error,
                "the [summaries.llm] summariser cannot work as configured, so EVERY summary will \
                 fail and /voice will draw those rows as failed, and no thread will be named. Fix \
                 the setting named here; nothing else needs to change."
            );
        } else {
            tracing::warn!(
                %error,
                "this deployment has NO summariser it can use: no [summaries.llm] section and no \
                 ElevenLabs credentials. EVERY summary will fail and /voice will draw those rows \
                 as failed. Configure [summaries.llm] (see the README's Summaries section), or set \
                 elevenlabs.api_key and elevenlabs.agent_id."
            );
        }
    }
    let summaries_possible = summaries_unavailable.is_none();
    // `#233 llm-summarizer`. Which provider, which dialect and which model — never a credential,
    // and never a credential's path, which says more about a host than this line needs to.
    if let Some(llm) = &config.summaries.llm {
        tracing::info!(
            dialect = llm.dialect.as_str(),
            model = %llm.model,
            base_url = %llm.base_url,
            max_tokens = llm.max_tokens,
            requests_per_minute = llm.requests_per_minute,
            burst = llm.burst,
            client_identity = llm.client_identity_file.is_some(),
            bearer_token = llm.bearer_token_file.is_some() || llm.bearer_token_command.is_some(),
            "summaries and thread names come from one Messages API call apiece, with NO tools; the \
             text of the messages being summarised is sent to this model provider"
        );
    }
    let summary_version =
        vibe_talk::summarize::policy_version_for(&config.summaries, summarizer.as_ref());
    tracing::info!(
        backend = summarizer.describe(),
        version = summary_version,
        threshold_chars = config.summaries.threshold_chars,
        target_chars = config.summaries.target_chars,
        // Said here as well as three lines above, because a reader who greps for the backend line
        // should not also have to find the warning above it to learn that it cannot work.
        summaries_available = summaries_possible,
        // The prose deliberately does NOT repeat the field's rendered form. It used to say
        // `summaries_available=false` in words, which meant a test asserting on that string
        // passed while the field itself had been deleted — the assertion was reading the
        // sentence, not the value.
        "summaries are produced by this backend under this policy version; every cached summary \
         is filed under the version, so changing a setting that changes what a summary says makes \
         the old ones unreachable at once. When the field above is false, NOTHING is summarised \
         and every long row shows as failed."
    );
    keep_summaries_across_the_policy_change(
        store.as_ref(),
        &config,
        summarizer.as_ref(),
        &summary_version,
    )
    .await;
    // `#208 persistent-cache`. The table is read and checked, so a bad overlay is refused here
    // rather than by the build that first opens the cache; this build opens none.
    if let Some(cache) = &config.message_cache {
        tracing::info!(
            enabled = cache.enabled,
            "[message_cache] is read and valid; this build does not open a message cache"
        );
    }

    // `#44 live-push`. Say the ingestion posture out loud in both directions, for the same reason
    // the MCP path and the access log are announced: a page waiting on a stream that nobody
    // publishes to is indistinguishable from a quiet channel, and the setting that causes it is
    // one line in a file the operator is not looking at.
    let live = Arc::new(vibe_talk::live::LiveHub::new());
    if config.ingest.enabled() {
        tracing::info!(
            events_route = "/api/v1/live/events",
            hints_route = "/api/v1/live/hints",
            "live adapter input is ON: an authenticated provider adapter may publish normalized \
             events for unpolled channels or wake a polled channel's authoritative cursor with \
             a content-free change hint"
        );
    }
    for entry in providers.entries() {
        let provider = entry.client.provider_name();
        if entry.live_poll_seconds > 0 {
            tracing::info!(
                provider,
                key = entry.key,
                interval_seconds = entry.live_poll_seconds,
                "live ingestion is ON for this provider: each of its channels is polled on this \
                 interval and what is new is pushed to GET /api/v1/channels/{{id}}/stream. Polling \
                 spends one request to the chat provider per channel per tick."
            );
        } else if !config.ingest.enabled() {
            tracing::info!(
                provider,
                key = entry.key,
                "live ingestion is OFF for this provider. GET /api/v1/channels/{{id}}/stream \
                 accepts subscribers and nothing publishes to them, so the page falls back to its \
                 own timed re-read. Set a live poll interval to turn it on; every tick is one \
                 chat-provider request per channel."
            );
        }
    }

    let bind = config.bind;
    let speech: Arc<dyn SpeechProvider> = match config.read_aloud.backend {
        ReadAloudBackend::Browser => Arc::new(vibe_talk::speech::BrowserSpeech),
        ReadAloudBackend::Conversation => Arc::new(vibe_talk::speech::ConversationSpeech::new(
            &config.conversation,
            config.read_aloud.websocket_url.as_deref(),
        )),
        ReadAloudBackend::ElevenLabs => {
            Arc::new(vibe_talk::elevenlabs::speech::ElevenLabsSpeech::new(
                elevenlabs_client.clone(),
                config.elevenlabs.clone(),
            ))
        }
    };
    let read_aloud = speech.describe();
    tracing::info!(
        backend = read_aloud.backend,
        label = read_aloud.label,
        playback = ?read_aloud.playback,
        "message read-aloud configured independently of conversational voice mode"
    );
    let elevenlabs: Arc<dyn SignedUrlProvider> = elevenlabs_client;
    // `#209 voice-socket-open`. A browser socket on another host's front door was what kept every
    // call on this host from connecting for two days; it is said here, once, where it is cheap.
    if let Some(warning) = vibe_talk::conversation::foreign_socket_host(
        &config.conversation,
        config.public_base_url.as_deref(),
    ) {
        tracing::warn!("{warning}");
    }
    // A replay the voice backend cannot carry is reported off to the page; say why, once.
    if let Some(warning) = vibe_talk::replay::unusable_warning(&config.replay, &config.conversation)
    {
        tracing::warn!("{warning}");
    }
    let conversation = vibe_talk::conversation::provider(
        &config.conversation,
        Arc::clone(&elevenlabs),
        config.elevenlabs.clone(),
    );
    let live_limit = config.chat.default_fetch_limit;
    let state = AppState {
        config: Arc::new(config),
        chat: Arc::clone(&chat),
        providers: Arc::clone(&providers),
        ranker: Arc::new(LexicalRanker),
        agent: Arc::new(NoAgentBackend),
        elevenlabs,
        conversation,
        speech,
        store,
        spoken_names: Arc::new(vibe_talk::speakable::SharedNames::new()),
        added_channels: Arc::new(std::sync::RwLock::new(Vec::new())),
        hidden_channels: Arc::new(std::sync::RwLock::new(std::collections::BTreeSet::new())),
        channel_registration_lock: Arc::new(tokio::sync::Mutex::new(())),
        voice_health_budget: Arc::new(vibe_talk::voice_health::LogBudget::new()),
        voice_end_budget: Arc::new(vibe_talk::voice_health::LogBudget::new()),
        headings: Arc::new(vibe_talk::heading::HeadingTags::new()),
        speech_tickets: Arc::new(vibe_talk::speech_tickets::SpeechTickets::new()),
        read_ahead: Arc::new(vibe_talk::read_ahead::ReadAhead::new()),
        recent_view_shadow: Arc::new(vibe_talk::recent_view::Shadow::new()),
        agent_override: Arc::new(vibe_talk::voice_prompt::OverrideAnswers::new()),
        post_gate: Arc::new(vibe_talk::post_gate::PostGate::new()),
        post_lanes: Arc::new(vibe_talk::post_lanes::PostLanes::new()),
        live: Arc::clone(&live),
        summarizer,
        summary_version: summary_version.into(),
        summary_counters: Arc::new(vibe_talk::summarize::counters::SummaryCounters::new()),
        thread_names: Arc::new(vibe_talk::summarize::threads::ThreadNames::new()),
    };
    // CHANNELS THE OWNER ADDED FROM INSIDE THE APP, brought back into the allowlist before the
    // server answers anything. They were probed when they were added; they are not probed again
    // here, because a channel the bot has since been removed from should fail the request that
    // touches it rather than stop the whole bridge from starting over somebody else's change.
    match state.restore_added_channels().await {
        Ok(restored) => {
            if restored > 0 {
                tracing::info!(added = restored, "restored channels added in the app");
            }
        }
        // A store that is absent is the ordinary state of a deployment that never set
        // `storage.path`, and it means there is nothing to restore rather than that something is
        // wrong. A real backend failure degrades the same way and says so, for the same reason an
        // unreadable alias does not stop the channel being read.
        Err(error) => tracing::warn!(%error, "could not restore channels added in the app"),
    }
    // `#199 removable-config-channels`. Configured channels the owner took off his list stay off
    // it. A store that cannot be read degrades to showing them, which is the state the file
    // describes and the one that loses nothing.
    match state.restore_hidden_channels().await {
        Ok(restored) => {
            if restored > 0 {
                tracing::info!(
                    hidden = restored,
                    "configured channels the owner removed in the app stay off the list"
                );
            }
        }
        Err(error) => tracing::warn!(%error, "could not restore the hidden configured channels"),
    }
    state.refresh_routes();
    for entry in providers.entries() {
        if entry.live_poll_seconds > 0 {
            tokio::spawn(vibe_talk::live::poll_forever(
                state.clone(),
                providers.is_multi().then(|| entry.key.clone()),
                Arc::clone(&entry.client),
                live_limit,
                std::time::Duration::from_secs(entry.live_poll_seconds),
            ));
        }
    }
    // `#232 upstream-mark-read`. Only a provider that can clear its own unread state gets the
    // loop; it is off for every channel until the owner sets one in Settings.
    if state.chat.supports_upstream_clear() {
        tokio::spawn(vibe_talk::upstream_clear::Clearer::for_state(&state).run());
    }
    // `#208 persistent-cache`. One line per provider per hour: how long its operations and Web API
    // calls took that hour, and how big the messages read were. Names and numbers only.
    tokio::spawn(vibe_talk::upstream_profile::log_every_hour(Arc::clone(
        providers.profile(),
    )));
    let app = vibe_talk::http::router(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!(%bind, "vibe-talk listening");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });
    let mut server_shutdown = shutdown_rx.clone();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            wait_for_shutdown(&mut server_shutdown).await;
        })
        .into_future();
    match run_with_shutdown_deadline(server, shutdown_rx, GRACEFUL_SHUTDOWN_TIMEOUT).await {
        Some(result) => result.context("serving")?,
        None => tracing::warn!(
            timeout_seconds = GRACEFUL_SHUTDOWN_TIMEOUT.as_secs(),
            "graceful shutdown deadline reached; closing remaining streaming connections"
        ),
    }
    Ok(())
}

async fn wait_for_shutdown(shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
}

async fn run_with_shutdown_deadline<F>(
    server: F,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    timeout: Duration,
) -> Option<F::Output>
where
    F: std::future::Future,
{
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => Some(result),
        () = async {
            wait_for_shutdown(&mut shutdown).await;
            tokio::time::sleep(timeout).await;
        } => None,
    }
}

/// Check at startup that every configured channel can actually be read, and refuse to start if
/// one cannot.
///
/// This is the same posture the configuration already takes toward equal or short tokens: a
/// single-user tool that comes up in a state the operator did not mean is worse than one that
/// refuses and says why. The failure it exists for — a bot that was created, tokened, and
/// configured, but never added to the channel — is invisible at startup today and surfaces later
/// as an empty digest, which reads like a bug in this code.
async fn run_startup_probe(
    chat: &dyn ChatClient,
    config: &Config,
    skip_flag: bool,
) -> anyhow::Result<()> {
    let skip_env = std::env::var(ENV_SKIP_STARTUP_PROBE).ok();
    match probe::startup_check(chat, &config.channels, skip_flag, skip_env.as_deref()).await {
        // Loudly, and naming which switch did it: a skipped check that says nothing is the same
        // silent start this probe was added to remove.
        probe::StartupCheck::Skipped { source } => {
            tracing::warn!(
                source,
                "SKIPPING the startup channel probe. This server has NOT checked that its bot can \
                 read the configured channels; if it cannot, reads will come back empty rather \
                 than failing."
            );
            eprintln!(
                "WARNING: startup channel probe SKIPPED via {source}; channel reachability is \
                 unverified."
            );
        }
        probe::StartupCheck::Passed(report) => {
            if report.warnings().is_empty() {
                tracing::info!("{}", report.render_for(chat.provider_name()));
            } else {
                tracing::warn!("{}", report.render_for(chat.provider_name()));
            }
        }
        probe::StartupCheck::Failed(report) => {
            let failed = report.failures().len().max(usize::from(report.aborted));
            // The detail goes to stderr rather than through `tracing`, because RUST_LOG is an
            // operator-settable filter and a startup refusal must not be suppressible by it. One
            // structured line still goes to the log, so `podman logs` shows why the container
            // exited without repeating the whole report twice.
            tracing::error!(
                failed,
                configured = config.channels.len(),
                "startup channel probe FAILED; the per-channel diagnosis is on stderr"
            );
            eprintln!("{}", report.render_for(chat.provider_name()));
            anyhow::bail!(
                "{failed} of {} configured channel(s) could not be read; refusing to start. Fix \
                 the configuration above, or re-run with {} if you know the check is wrong.",
                config.channels.len(),
                probe::SKIP_FLAG
            );
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    // A container stops with SIGTERM, not SIGINT. Handling only ctrl-c means every `podman stop`
    // waits out its ten-second grace period and then SIGKILLs the process, which is both slow and
    // a lie about whether shutdown was clean.
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "could not install the interrupt handler");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "could not install the SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => tracing::info!("interrupted; shutting down"),
        () = terminate => tracing::info!("terminated; shutting down"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn an_open_stream_cannot_hold_shutdown_past_the_deadline() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let serving = std::future::pending::<()>();
        let stopped = run_with_shutdown_deadline(serving, shutdown_rx, Duration::from_secs(5));
        tokio::pin!(stopped);
        assert!(shutdown_tx.send(true).is_ok());
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(stopped.await.is_none());
    }

    #[tokio::test]
    async fn a_re_key_that_fails_skips_the_sweep_and_the_next_start_retries_it() {
        // `#208 persistent-cache`, milestone `summary-keep`. The sweep after a failed re-key would
        // delete exactly the rows that could not be moved, which is the full sweep this milestone
        // exists to prevent. Skipped, they wait for the next start, which moves them.
        use vibe_talk::model::{ChannelId, MessageId};
        use vibe_talk::store::SummaryKey;
        let config = vibe_talk::testing::config();
        let summarizer = vibe_talk::summarize::fake::FakeSummarizer::new();
        let current = vibe_talk::summarize::policy_version_for(&config.summaries, &summarizer);
        let previous = vibe_talk::summarize::policy_version_v1_for(&config.summaries, &summarizer);
        let unrelated = "v1-messages-api-w3-c160-0000000000000001";
        assert_ne!(previous, unrelated);
        let key = |version: &str| SummaryKey {
            channel: ChannelId("1111111111".to_owned()),
            message: MessageId("1000000000000000100".to_owned()),
            content_hash: 7,
            version: version.to_owned(),
        };
        let store = vibe_talk::store::fake::FakeStore::new();
        for version in [previous.as_str(), unrelated] {
            store
                .cache_summary(&key(version), "somebody else's words, shortened", None)
                .await
                .expect("file");
        }
        let held = |version: String| {
            let store = &store;
            let key = key(&version);
            async move { store.cached_summary(&key).await.expect("read").is_some() }
        };

        store.fail_next("the disk is busy");
        keep_summaries_across_the_policy_change(&store, &config, &summarizer, &current).await;
        assert!(
            held(previous.clone()).await,
            "a row that was not moved went"
        );
        assert!(
            held(unrelated.to_owned()).await,
            "the sweep ran after the re-key failed"
        );
        assert!(!held(current.clone()).await);

        keep_summaries_across_the_policy_change(&store, &config, &summarizer, &current).await;
        assert!(held(current.clone()).await, "the next start did not re-key");
        assert!(!held(previous).await);
        assert!(
            !held(unrelated.to_owned()).await,
            "the next start did not sweep"
        );
    }

    #[tokio::test]
    async fn an_ordinary_server_exit_is_returned_without_waiting_for_shutdown() {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let stopped = run_with_shutdown_deadline(
            std::future::ready("served"),
            shutdown_rx,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(stopped, Some("served"));
    }
}
