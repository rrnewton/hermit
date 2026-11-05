//! `vibe-talk-mock-elevenlabs` — the loopback vendor substitute, as a process.
//!
//! The cargo suite spawns [`vibe_talk::elevenlabs::mock`] in-process; this binary is the same
//! mock for everything that cannot do that — a browser under Playwright, the smoke script, or a
//! person who wants to click through `/voice` without spending vendor minutes.
//!
//! It refuses to bind anything but loopback. It hands out conversations for an agent nobody owns,
//! on a key it invented, so being reachable off this machine is not a configuration choice.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use vibe_talk::elevenlabs::mock::{
    MockElevenLabs, MockOptions, Scenario, MOCK_AGENT_ID, MOCK_API_KEY,
};

const USAGE: &str = "\
vibe-talk-mock-elevenlabs — a deterministic, offline ElevenLabs substitute

USAGE:
    vibe-talk-mock-elevenlabs --bridge-base URL --bridge-token TOKEN [OPTIONS]

It serves two loopback listeners: an HTTP half that answers the real mint endpoint
(GET /v1/convai/conversation/get-signed-url) plus a /_mock/ control plane, and a WebSocket half
that holds a real conversation. Point vibe-talk's elevenlabs.api_base at the HTTP half:

    [elevenlabs]
    agent_id = \"agent_mock00000000000000000000000\"
    api_key  = \"xi-mock-api-key-not-a-real-one\"
    api_base = \"http://127.0.0.1:18092/v1\"

OPTIONS:
    --bridge-base URL     the vibe-talk server this mock's agent calls over MCP, e.g.
                          http://127.0.0.1:18091 (required)
    --bridge-token TOKEN  write-scope bearer token for that server (required)
    --http-port PORT      port for the mint + control plane (default: 18092; 0 asks the kernel)
    --ws-port PORT        port for the conversation socket (default: 18093; 0 asks the kernel)
    --bind ADDR           loopback address to bind (default: 127.0.0.1). Anything else is refused.
    --agent-id ID         the agent id this mock account holds (default: the constant above)
    --api-key KEY         the api key this mock account accepts (default: the constant above)
    --scenario NAME       full | no_tool_call | ignored_tool_result | mint_rejected |
                          socket_drop | unsupported_audio | no_reply (default: full)
    --ping-ms MS          send the JSON ping event every MS milliseconds (default: never)
    --trace-file PATH     write the redacted event trace as JSON Lines on shutdown
    --help                print this message and exit

CONTROL PLANE (on the HTTP half):
    POST /_mock/scenario  {\"scenario\": \"no_tool_call\"}
    POST /_mock/say       {\"who\": \"agent|user|correction|interrupt\", \"text\": \"...\"}
    POST /_mock/reset     forget the trace and go back to the full scenario
    GET  /_mock/trace     the redacted event trace as JSON
";

struct Args {
    bridge_base: String,
    bridge_token: String,
    http_port: u16,
    ws_port: u16,
    bind: IpAddr,
    agent_id: String,
    api_key: String,
    scenario: Scenario,
    ping_every: Option<Duration>,
    trace_file: Option<PathBuf>,
}

fn parse_args() -> anyhow::Result<Option<Args>> {
    let mut args = Args {
        bridge_base: String::new(),
        bridge_token: String::new(),
        http_port: 18092,
        ws_port: 18093,
        bind: IpAddr::from([127, 0, 0, 1]),
        agent_id: MOCK_AGENT_ID.to_owned(),
        api_key: MOCK_API_KEY.to_owned(),
        scenario: Scenario::Full,
        ping_every: None,
        trace_file: None,
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = || argv.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "--bridge-base" => args.bridge_base = value()?,
            "--bridge-token" => args.bridge_token = value()?,
            "--http-port" => args.http_port = value()?.parse().context("--http-port")?,
            "--ws-port" => args.ws_port = value()?.parse().context("--ws-port")?,
            "--bind" => args.bind = value()?.parse().context("--bind")?,
            "--agent-id" => args.agent_id = value()?,
            "--api-key" => args.api_key = value()?,
            "--scenario" => {
                let name = value()?;
                args.scenario = Scenario::from_name(&name).with_context(|| {
                    let known: Vec<&str> = Scenario::all().iter().map(|s| s.name()).collect();
                    format!("unknown scenario {name:?}; known scenarios are {known:?}")
                })?;
            }
            "--ping-ms" => {
                let ms: u64 = value()?.parse().context("--ping-ms")?;
                args.ping_every = Some(Duration::from_millis(ms));
            }
            "--trace-file" => args.trace_file = Some(PathBuf::from(value()?)),
            other => anyhow::bail!("unrecognized argument {other:?}\n\n{USAGE}"),
        }
    }
    // Required rather than defaulted: a mock pointed at nothing would answer every question with
    // an apology, and that reads exactly like the bug it exists to reproduce.
    anyhow::ensure!(
        !args.bridge_base.trim().is_empty(),
        "--bridge-base is required: this mock's agent has to have a vibe-talk server to call"
    );
    anyhow::ensure!(
        !args.bridge_token.trim().is_empty(),
        "--bridge-token is required: the MCP endpoint refuses an unauthenticated caller"
    );
    Ok(Some(args))
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
    let trace_file = args.trace_file.clone();

    let mock = MockElevenLabs::spawn(MockOptions {
        bind: args.bind,
        http_port: args.http_port,
        ws_port: args.ws_port,
        agent_id: args.agent_id.clone(),
        api_key: args.api_key.clone(),
        bridge_base: args.bridge_base.clone(),
        bridge_token: args.bridge_token.clone(),
        scenario: args.scenario,
        ping_every: args.ping_every,
        ..MockOptions::default()
    })
    .await?;

    tracing::warn!(
        "this is a MOCK ElevenLabs. It mints conversations for an agent nobody owns; it is bound \
         to loopback and must stay there."
    );
    tracing::info!(api_base = %mock.api_base(), "point elevenlabs.api_base here");
    tracing::info!(control = %mock.control_base(), "the /_mock/ control plane is here");
    tracing::info!(websocket = %mock.ws_addr(), "conversations are held here");
    tracing::info!(agent_id = %args.agent_id, scenario = args.scenario.name(), bridge = %args.bridge_base, "ready");

    tokio::signal::ctrl_c()
        .await
        .context("waiting for ctrl-c")?;
    tracing::info!("stopping");

    if let Some(path) = trace_file {
        mock.trace()
            .write_jsonl(&path)
            .with_context(|| format!("writing the trace to {}", path.display()))?;
        tracing::info!(path = %path.display(), "wrote the event trace");
    }
    Ok(())
}
