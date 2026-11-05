//! `[summaries.llm]` and configuration overlays. `#233 llm-summarizer`.
//!
//! In a file of its own so the section's tests sit beside nothing else's.

use super::*;

const BASE: &str = r#"
[discord]
bot_token = "file-discord-token"

[auth]
read_token = "read-token-that-is-long-enough"
write_token = "write-token-that-is-long-enough"

[[channels]]
id = "111"
label = "lead"
writable = true

[summaries]
threshold_chars = 500
"#;

fn load(extra: &str) -> Result<Config, ConfigError> {
    Config::from_toml_and_env(&format!("{BASE}\n{extra}"), &BTreeMap::new())
}

fn invalid_field(result: Result<Config, ConfigError>) -> String {
    match result {
        Err(ConfigError::Invalid { field, .. }) => field,
        Err(other) => panic!("expected an invalid field, got {other}"),
        Ok(_) => panic!("expected a refusal, got a configuration"),
    }
}

#[test]
fn an_anthropic_section_needs_only_a_key_file_and_takes_conservative_defaults() {
    let cfg = load("[summaries.llm]\napi_key_file = \"/run/secrets/model-key\"\n").expect("valid");
    let llm = cfg.summaries.llm.expect("the section is read");
    assert_eq!(llm.dialect, LlmDialect::Anthropic);
    assert_eq!(llm.base_url, DEFAULT_ANTHROPIC_BASE_URL);
    assert_eq!(llm.model, DEFAULT_LLM_MODEL);
    assert_eq!(llm.max_tokens, DEFAULT_LLM_MAX_TOKENS);
    assert_eq!(llm.timeout_secs, DEFAULT_LLM_TIMEOUT_SECS);
    assert_eq!(llm.requests_per_minute, DEFAULT_LLM_REQUESTS_PER_MINUTE);
    assert_eq!(llm.burst, DEFAULT_LLM_BURST);
    assert_eq!(
        llm.api_key_file.as_deref(),
        Some(Path::new("/run/secrets/model-key"))
    );
    // The rest of [summaries] is untouched by the subsection.
    assert_eq!(cfg.summaries.threshold_chars, 500);
    // ...and with no subsection there is no LLM summariser at all.
    assert_eq!(load("").expect("valid").summaries.llm, None);
}

#[test]
fn an_anthropic_section_without_a_key_file_is_refused_by_name() {
    let err = load("[summaries.llm]\ndialect = \"anthropic\"\n").expect_err("no key");
    assert!(
        matches!(&err, ConfigError::Missing(what) if what.contains("summaries.llm.api_key_file")),
        "{err}"
    );
}

#[test]
fn a_secret_written_inline_is_refused_and_never_repeated() {
    for (line, field) in [
        (
            "api_key = \"sk-inline-secret-value\"",
            "summaries.llm.api_key",
        ),
        (
            "bearer_token = \"sk-inline-secret-value\"",
            "summaries.llm.bearer_token",
        ),
    ] {
        let err = load(&format!(
            "[summaries.llm]\napi_key_file = \"/run/secrets/k\"\n{line}\n"
        ))
        .expect_err("an inline secret must be refused");
        assert!(
            matches!(&err, ConfigError::Invalid { field: f, .. } if f == field),
            "{err}"
        );
        assert!(
            err.to_string().contains("_file"),
            "say what to do instead: {err}"
        );
        assert!(!err.to_string().contains("sk-inline-secret-value"), "{err}");
    }
}

#[test]
fn a_vertex_section_needs_a_project_a_region_and_some_credential() {
    let cfg = load(
        "[summaries.llm]\ndialect = \"vertex\"\nproject = \"example-project\"\nregion = \
         \"us-east5\"\nbearer_token_command = [\"gcloud\", \"auth\", \"print-access-token\"]\n",
    )
    .expect("valid");
    let llm = cfg.summaries.llm.expect("read");
    assert_eq!(llm.dialect, LlmDialect::Vertex);
    assert_eq!(
        llm.base_url,
        "https://us-east5-aiplatform.googleapis.com/v1"
    );
    assert_eq!(llm.api_key_file, None, "no key unless one is configured");

    let global = load(
        "[summaries.llm]\ndialect = \"vertex\"\nproject = \"p\"\nregion = \"global\"\n\
         bearer_token_file = \"/run/secrets/token\"\n",
    )
    .expect("valid");
    assert_eq!(
        global.summaries.llm.expect("read").base_url,
        "https://aiplatform.googleapis.com/v1"
    );

    for (missing, text) in [
        (
            "project",
            "[summaries.llm]\ndialect = \"vertex\"\nregion = \"global\"\nbearer_token_file = \"/t\"\n",
        ),
        (
            "region",
            "[summaries.llm]\ndialect = \"vertex\"\nproject = \"p\"\nbearer_token_file = \"/t\"\n",
        ),
        (
            "credential",
            "[summaries.llm]\ndialect = \"vertex\"\nproject = \"p\"\nregion = \"global\"\n",
        ),
    ] {
        let err = load(text).expect_err(missing);
        assert!(matches!(err, ConfigError::Missing(_)), "{missing}: {err}");
    }

    // A gateway that authenticates by client certificate alone needs nothing else.
    let mtls = load(
        "[summaries.llm]\ndialect = \"vertex\"\nproject = \"p\"\nregion = \"global\"\n\
         base_url = \"https://gateway.example/v1\"\nclient_identity_file = \"/etc/identity.pem\"\n\
         ca_bundle_file = \"/etc/private-root.pem\"\n\
         [summaries.llm.extra_headers]\nX-Client-Name = \"vibe-talk\"\n",
    )
    .expect("valid");
    let llm = mtls.summaries.llm.expect("read");
    assert_eq!(llm.base_url, "https://gateway.example/v1");
    assert_eq!(
        llm.extra_headers.get("x-client-name").map(String::as_str),
        Some("vibe-talk"),
        "header names are lowercased"
    );
}

#[test]
fn settings_that_cannot_work_together_or_at_all_are_refused() {
    let key = "api_key_file = \"/run/secrets/k\"";
    let vertex = "dialect = \"vertex\"\nproject = \"p\"\nregion = \"global\"";
    for (field, text) in [
        ("summaries.llm.project", format!("{key}\nproject = \"p\"")),
        (
            "summaries.llm.bearer_token_command",
            format!(
                "{vertex}\nbearer_token_file = \"/t\"\nbearer_token_command = [\"gcloud\"]"
            ),
        ),
        (
            "summaries.llm.bearer_token_command",
            format!("{vertex}\nbearer_token_command = []"),
        ),
        ("summaries.llm.api_key_file", "api_key_file = \"secrets/k\"".to_owned()),
        (
            "summaries.llm.base_url",
            format!("{key}\nbase_url = \"http://models.example\""),
        ),
        (
            "summaries.llm.base_url",
            format!("{key}\nbase_url = \"https://user:pass@models.example\""),
        ),
        (
            "summaries.llm.base_url",
            format!("{key}\nbase_url = \"not a url\""),
        ),
        ("summaries.llm.model", format!("{key}\nmodel = \"../../admin\"")),
        (
            "summaries.llm.region",
            "dialect = \"vertex\"\nproject = \"p\"\nbearer_token_file = \"/t\"\nregion = \"us/../x\""
                .to_owned(),
        ),
        (
            "summaries.llm.extra_headers",
            format!("{key}\n[summaries.llm.extra_headers]\nx-api-key = \"smuggled\""),
        ),
        (
            "summaries.llm.extra_headers",
            format!("{key}\n[summaries.llm.extra_headers]\nAuthorization = \"Bearer x\""),
        ),
        ("summaries.llm.max_tokens", format!("{key}\nmax_tokens = 0")),
        ("summaries.llm.timeout_secs", format!("{key}\ntimeout_secs = 0")),
        (
            "summaries.llm.requests_per_minute",
            format!("{key}\nrequests_per_minute = 0"),
        ),
        ("summaries.llm.burst", format!("{key}\nburst = 0")),
    ] {
        // `extra_headers` is a table, so it must come after every plain key of the section.
        let text = match text.split_once("\n[") {
            Some((keys, table)) => format!("[summaries.llm]\n{keys}\n[{table}\n"),
            None => format!("[summaries.llm]\n{text}\n"),
        };
        assert_eq!(invalid_field(load(&text)), field, "{text}");
    }
    // Plain HTTP IS accepted to this machine: a local mock or a sidecar.
    let local = load(&format!(
        "[summaries.llm]\n{key}\nbase_url = \"http://127.0.0.1:18080/\""
    ))
    .expect("loopback http is fine");
    assert_eq!(
        local.summaries.llm.expect("read").base_url,
        "http://127.0.0.1:18080"
    );
}

#[test]
fn the_section_names_its_own_model_and_never_inherits_the_summaries_model() {
    // `summaries.model` was long documented as read by nothing, so it may hold a value from an
    // older summariser that no Messages API provider knows. Inheriting it would fail every call.
    let text = format!(
        "{BASE}\n[summaries.llm]\napi_key_file = \"/k\"\n",
        BASE = BASE.replace(
            "threshold_chars = 500",
            "threshold_chars = 500\nmodel = \"from-file\""
        )
    );
    let cfg = Config::from_toml_and_env(&text, &BTreeMap::new()).expect("valid");
    assert_eq!(cfg.summaries.llm.expect("read").model, DEFAULT_LLM_MODEL);
    let env = BTreeMap::from([(ENV_SUMMARY_MODEL.to_owned(), "from-env".to_owned())]);
    let cfg = Config::from_toml_and_env(&text, &env).expect("valid");
    assert_eq!(cfg.summaries.llm.expect("read").model, DEFAULT_LLM_MODEL);
    // The section's own model is the one used.
    let own = text.replace(
        "api_key_file = \"/k\"",
        "api_key_file = \"/k\"\nmodel = \"own\"",
    );
    let cfg = Config::from_toml_and_env(&own, &env).expect("valid");
    assert_eq!(cfg.summaries.llm.expect("read").model, "own");
}

#[test]
fn an_overlay_merges_table_by_table_and_replaces_what_it_names() {
    let overlay = r#"
[summaries]
target_chars = 120

[summaries.llm]
api_key_file = "/run/secrets/this-host-only"

[[channels]]
id = "222"
label = "this host's own"
"#;
    let merged =
        merge_overlays(BASE, &[("host.toml".to_owned(), overlay.to_owned())]).expect("merges");
    let cfg = Config::from_toml_and_env(&merged, &BTreeMap::new()).expect("valid");
    // A key the overlay sets in a table the base also has: merged, not replaced wholesale.
    assert_eq!(
        cfg.summaries.threshold_chars, 500,
        "the base's own key survives"
    );
    assert_eq!(cfg.summaries.target_chars, 120, "the overlay's key wins");
    assert!(
        cfg.summaries.llm.is_some(),
        "a subsection only the overlay has is added"
    );
    // An array is a value like any other: replaced, not appended to.
    assert_eq!(cfg.channels.len(), 1);
    assert_eq!(cfg.channels[0].id.as_str(), "222");
    // What the overlay does not mention is exactly as the base has it.
    assert_eq!(
        cfg.auth.read_token.expose(),
        "read-token-that-is-long-enough"
    );

    // Overlays apply in order, each over the last.
    let later = "[summaries]\ntarget_chars = 90\n".to_owned();
    let merged = merge_overlays(
        BASE,
        &[
            ("first.toml".to_owned(), overlay.to_owned()),
            ("second.toml".to_owned(), later),
        ],
    )
    .expect("merges");
    let cfg = Config::from_toml_and_env(&merged, &BTreeMap::new()).expect("valid");
    assert_eq!(cfg.summaries.target_chars, 90);
    assert!(cfg.summaries.llm.is_some());
}

#[test]
fn an_overlay_that_does_not_parse_is_reported_against_its_own_file() {
    let err = merge_overlays(BASE, &[("host.toml".to_owned(), "[summaries\n".to_owned())])
        .expect_err("not TOML");
    assert!(
        matches!(&err, ConfigError::Invalid { field, .. } if field.contains("host.toml")),
        "{err}"
    );
}

#[test]
fn layered_loading_reads_the_overlay_files_from_disk() {
    let dir = crate::testing::TempDir::new("config-overlay");
    let base = dir.path().join("shared.toml");
    let overlay = dir.path().join("this-host.toml");
    std::fs::write(&base, BASE).expect("write");
    std::fs::write(
        &overlay,
        "[summaries.llm]\ndialect = \"anthropic\"\napi_key_file = \"/run/secrets/k\"\n",
    )
    .expect("write");
    // The environment of the test process is read too, as `load` reads it; nothing in it can
    // name a summaries setting this test depends on, because none has an environment form.
    let cfg = Config::load_layered(Some(&base), std::slice::from_ref(&overlay)).expect("valid");
    assert_eq!(
        cfg.summaries.llm.expect("the overlay applied").dialect,
        LlmDialect::Anthropic
    );
    let cfg = Config::load_layered(Some(&base), &[]).expect("valid");
    assert_eq!(
        cfg.summaries.llm, None,
        "without the overlay the shared file has none"
    );
    let missing = dir.path().join("absent.toml");
    assert!(matches!(
        Config::load_layered(Some(&base), &[missing]),
        Err(ConfigError::Read { .. })
    ));
}

#[test]
fn an_overlay_that_names_the_llm_section_replaces_it_whole_and_inherits_no_key() {
    // The two-host case the overlay exists for: the shared file says anthropic with a key file,
    // and this host's overlay moves to a Vertex gateway. A key-by-key merge would have kept the
    // shared `api_key_file`, and the Anthropic key would have gone out as `x-api-key` to the
    // gateway on every call.
    let shared = format!(
        "{BASE}\n[summaries.llm]\ndialect = \"anthropic\"\n\
         api_key_file = \"/run/secrets/anthropic-api-key\"\nmax_tokens = 200\n"
    );
    let overlay = "[summaries.llm]\ndialect = \"vertex\"\n\
                   base_url = \"https://models.gateway.example/v1\"\nproject = \"my-project\"\n\
                   region = \"global\"\nclient_identity_file = \"/etc/vibe-talk/identity.pem\"\n";
    let merged = merge_overlays(
        &shared,
        &[("this-host.toml".to_owned(), overlay.to_owned())],
    )
    .expect("merges");
    let cfg = Config::from_toml_and_env(&merged, &BTreeMap::new()).expect("valid");
    let llm = cfg.summaries.llm.expect("the overlay's section");
    assert_eq!(llm.dialect, LlmDialect::Vertex);
    assert_eq!(
        llm.api_key_file, None,
        "the shared file's key was inherited by a section that never named one"
    );
    assert_eq!(
        llm.max_tokens, DEFAULT_LLM_MAX_TOKENS,
        "nothing of the shared section survives, not even what would have been harmless"
    );
    assert_eq!(llm.base_url, "https://models.gateway.example/v1");
    // Only that one table is taken whole: the rest of [summaries] still merges key by key.
    assert_eq!(cfg.summaries.threshold_chars, 500);

    // An overlay that writes PART of the section gets that part alone, and a part that cannot
    // stand alone is refused by name rather than quietly completed from the shared file.
    let merged = merge_overlays(
        &shared,
        &[(
            "this-host.toml".to_owned(),
            "[summaries.llm]\nmodel = \"claude-sonnet-4-5\"\n".to_owned(),
        )],
    )
    .expect("merges");
    let err = Config::from_toml_and_env(&merged, &BTreeMap::new()).expect_err("no key file");
    assert!(
        matches!(&err, ConfigError::Missing(what) if what.contains("api_key_file")),
        "{err}"
    );

    // ...and an overlay that does not mention the section leaves the shared one exactly as it is.
    let merged = merge_overlays(
        &shared,
        &[(
            "this-host.toml".to_owned(),
            "[summaries]\ntarget_chars = 120\n".to_owned(),
        )],
    )
    .expect("merges");
    let llm = Config::from_toml_and_env(&merged, &BTreeMap::new())
        .expect("valid")
        .summaries
        .llm
        .expect("the shared section");
    assert_eq!(llm.dialect, LlmDialect::Anthropic);
    assert_eq!(llm.max_tokens, 200);
}

#[test]
fn a_mistake_in_any_layer_is_reported_against_that_file_at_its_own_line() {
    // A value of the wrong type in an overlay: named, with the overlay's own line, not a line of
    // the merged text nobody has seen.
    let err = merge_overlays(
        BASE,
        &[(
            "this-host.toml".to_owned(),
            "# this host\n\n[summaries]\ntarget_chars = \"wide\"\n".to_owned(),
        )],
    )
    .expect_err("a string where a number goes");
    let ConfigError::Invalid { field, detail } = &err else {
        panic!("expected the overlay to be named: {err}");
    };
    assert!(field.contains("this-host.toml"), "{field}");
    assert!(detail.contains("line 4"), "{detail}");

    // An unknown key in an overlay is caught the same way.
    let err = merge_overlays(
        BASE,
        &[(
            "this-host.toml".to_owned(),
            "[summaries.llm]\napi_key_flie = \"/k\"\n".to_owned(),
        )],
    )
    .expect_err("a misspelt key");
    assert!(err.to_string().contains("api_key_flie"), "{err}");

    // And one in the base keeps the base's own line, though an overlay is in use.
    let broken = BASE.replace("threshold_chars = 500", "threshold_chars = \"many\"");
    let line = broken
        .lines()
        .position(|text| text.contains("\"many\""))
        .expect("the broken line")
        + 1;
    let err = merge_overlays(
        &broken,
        &[(
            "this-host.toml".to_owned(),
            "[summaries]\ntarget_chars = 120\n".to_owned(),
        )],
    )
    .expect_err("the base is broken");
    assert!(
        matches!(&err, ConfigError::Parse(_)) && err.to_string().contains(&format!("line {line}")),
        "{err}"
    );
}

#[test]
fn an_extra_header_named_like_a_credential_is_refused_and_its_value_never_repeated() {
    for name in [
        "cookie",
        "proxy-authorization",
        "x-goog-api-key",
        "x-auth-token",
        "x-client-secret",
        "X-Session-Id",
    ] {
        let text = format!(
            "[summaries.llm]\napi_key_file = \"/k\"\n[summaries.llm.extra_headers]\n\"{name}\" = \"hunter2-value\"\n"
        );
        let err = load(&text).expect_err(name);
        let said = err.to_string();
        assert!(
            said.contains("summaries.llm.extra_headers"),
            "{name}: {said}"
        );
        assert!(
            !said.contains("hunter2-value"),
            "{name}: the value was repeated: {said}"
        );
    }
    // A header that only identifies the caller is fine.
    let cfg = load(
        "[summaries.llm]\napi_key_file = \"/k\"\n[summaries.llm.extra_headers]\nx-client-name = \"vibe-talk\"\n",
    )
    .expect("valid");
    assert_eq!(
        cfg.summaries.llm.expect("read").extra_headers["x-client-name"],
        "vibe-talk"
    );
}
