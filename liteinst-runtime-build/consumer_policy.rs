//! Application build policy. RV independently checks this compiled definition,
//! actual source/package graph, Cargo events and its closed machine recipes.
use std::collections::BTreeMap;

use proof::ConsumerBuildVariant;
use proof::ConsumerCargoForm;
use proof::ConsumerPackageRole as Role;
use proof::ConsumerPackageSpec;
use proof::ConsumerPolicySpec;
use proof::ConsumerRecipe;
use proof::ConsumerSourceRoot;
use proof::ConsumerWorkspaceFileRole as FileRole;
use proof::ConsumerWrapperSpec;
use reverie_liteinst_artifact as proof;

/// The caller chooses a closed RV machine recipe; every factory is reachable
/// through this single typed entry point in each importing crate.
pub fn specification(recipe: ConsumerRecipe) -> ConsumerPolicySpec {
    match recipe {
        ConsumerRecipe::Allocation101LayoutA => native(),
        ConsumerRecipe::Allocation101LayoutB => ordinary(),
        ConsumerRecipe::Allocation62 => standalone(),
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}
fn runtime(source_root: ConsumerSourceRoot, name: &str, target: &str) -> ConsumerPackageSpec {
    ConsumerPackageSpec {
        source_root,
        name: name.into(),
        version: "0.5.4".into(),
        manifest: format!("{name}/Cargo.toml"),
        target: target.into(),
        target_source: format!("{name}/src/lib.rs"),
        kind: strings(&["cdylib"]),
        crate_types: strings(&["cdylib"]),
    }
}
fn spec(
    runtime: ConsumerPackageSpec,
    wrapper: Option<ConsumerWrapperSpec>,
    variant: ConsumerBuildVariant,
) -> ConsumerPolicySpec {
    ConsumerPolicySpec {
        definition_relative: "liteinst-runtime-build/consumer_policy.rs".into(),
        definition_compiled_bytes: include_bytes!("consumer_policy.rs"),
        runtime,
        wrapper,
        variants: vec![variant],
    }
}

fn ordinary() -> ConsumerPolicySpec {
    let resolved_features = BTreeMap::from([
        (
            Role::InGuest,
            strings(&["coordinator-rpc", "launcher", "rcb-clock"]),
        ),
        (Role::Instrumentation, strings(&[])),
        (Role::Runtime, strings(&[])),
    ]);
    let evidence_labels = BTreeMap::from([
        (
            "allocation101_layout_b_legacy_v1".into(),
            "detcore_ordinary_off_alloc101_reviewed_source_current_build_rx_loader_census_v1"
                .into(),
        ),
        (
            "allocation101_layout_b_semantic_v2".into(),
            "ordinary_off_alloc101_semantic_v0_reviewed_source_current_build_rx_loader_census_v2"
                .into(),
        ),
    ]);
    let variant = ConsumerBuildVariant {
        recipe: ConsumerRecipe::Allocation101LayoutB,
        command_form: ConsumerCargoForm::SingleLibrary,
        requested_features: strings(&[]),
        resolved_features,
        profile_name: "liteinst-runtime".into(),
        profile: serde_json::json!({"debug_assertions":true,"debuginfo":0,"opt_level":"3","overflow_checks":true,"test":false}),
        evidence_labels,
    };
    spec(
        runtime(
            ConsumerSourceRoot::Consumer,
            "detcore-liteinst",
            "detcore_liteinst",
        ),
        None,
        variant,
    )
}

fn native() -> ConsumerPolicySpec {
    let resolved_features = BTreeMap::from([
        (
            Role::InGuest,
            strings(&[
                "coordinator-rpc",
                "exec-native-injection",
                "exec-private-init",
                "exec-private-successor",
                "fixed-syscall-gates",
                "launcher",
                "native-exec-state",
                "rcb-clock",
            ]),
        ),
        (
            Role::Instrumentation,
            strings(&[
                "exec-native-injection",
                "exec-private-init",
                "exec-private-successor",
                "fixed-syscall-gates",
                "native-exec-state",
            ]),
        ),
        (
            Role::Runtime,
            strings(&["exec-native-injection", "exec-private-init"]),
        ),
    ]);
    let evidence_labels = BTreeMap::from([
        ("allocation101_layout_a_legacy_v1".into(), "detcore_native_off_alloc101_reviewed_source_current_build_rx_loader_census_v1".into()),
        ("allocation101_layout_a_semantic_v2".into(), "native_off_alloc101_tls140_semantic_v0_reviewed_source_current_build_rx_loader_census_v2".into()),
        ("allocation101_layout_a_tls100_legacy_v2".into(), "detcore_native_off_alloc101_tls100_reviewed_source_current_build_rx_loader_census_v2".into()),
        ("allocation101_layout_a_tls100_semantic_v3".into(), "native_off_alloc101_tls100_semantic_v0_reviewed_source_current_build_rx_loader_census_v3".into()),
        ("allocation101_layout_a_tls130_owner138_legacy_v4".into(), "detcore_native_off_alloc101_tls130_owner138_reviewed_source_current_build_rx_loader_census_v4".into()),
    ]);
    let variant = ConsumerBuildVariant {
        recipe: ConsumerRecipe::Allocation101LayoutA,
        command_form: ConsumerCargoForm::SingleLibrary,
        requested_features: strings(&["exec-native-injection"]),
        resolved_features,
        profile_name: "liteinst-runtime".into(),
        profile: serde_json::json!({"debug_assertions":true,"debuginfo":0,"opt_level":"3","overflow_checks":true,"test":false}),
        evidence_labels,
    };
    spec(
        runtime(
            ConsumerSourceRoot::Consumer,
            "detcore-liteinst",
            "detcore_liteinst",
        ),
        None,
        variant,
    )
}

fn standalone() -> ConsumerPolicySpec {
    let resolved_features = BTreeMap::from([
        (
            Role::InGuest,
            strings(&["coordinator-rpc", "launcher", "rcb-clock"]),
        ),
        (Role::Instrumentation, strings(&[])),
        (Role::Runtime, strings(&[])),
        (Role::Wrapper, strings(&[])),
    ]);
    let evidence_labels = BTreeMap::from([
        ("allocation62_legacy_v1".into(), "standalone_release_off_alloc62_reviewed_source_current_build_rx_loader_census_v1".into()),
        ("allocation62_semantic_v2".into(), "standalone_release_off_alloc62_semantic_v0_reviewed_source_current_build_rx_loader_census_v2".into()),
    ]);
    let variant = ConsumerBuildVariant {
        recipe: ConsumerRecipe::Allocation62,
        command_form: ConsumerCargoForm::NestedLibraryPair,
        requested_features: strings(&[]),
        resolved_features,
        profile_name: "release".into(),
        profile: serde_json::json!({"debug_assertions":false,"debuginfo":0,"opt_level":"3","overflow_checks":false,"test":false}),
        evidence_labels,
    };
    let wrapper = ConsumerWrapperSpec {
        workspace: "liteinst-runtime-build".into(),
        files: BTreeMap::from([
            (
                FileRole::BuildScript,
                "liteinst-runtime-build/build.rs".into(),
            ),
            (
                FileRole::ProofModule,
                "liteinst-runtime-build/artifact.rs".into(),
            ),
            (
                FileRole::RootLock,
                "liteinst-runtime-build/Cargo.lock".into(),
            ),
            (
                FileRole::RootManifest,
                "liteinst-runtime-build/Cargo.toml".into(),
            ),
            (
                FileRole::WrapperManifest,
                "liteinst-runtime-build/runtime/Cargo.toml".into(),
            ),
            (
                FileRole::WrapperSource,
                "liteinst-runtime-build/runtime/src/lib.rs".into(),
            ),
        ]),
        package: ConsumerPackageSpec {
            source_root: ConsumerSourceRoot::Consumer,
            name: "hermit-liteinst-runtime-artifact".into(),
            version: "0.5.4".into(),
            manifest: "liteinst-runtime-build/runtime/Cargo.toml".into(),
            target: "hermit_liteinst_runtime_artifact".into(),
            target_source: "liteinst-runtime-build/runtime/src/lib.rs".into(),
            kind: strings(&["lib"]),
            crate_types: strings(&["lib"]),
        },
    };
    spec(
        runtime(
            ConsumerSourceRoot::Reverie,
            "reverie-liteinst-preload",
            "reverie_liteinst_preload",
        ),
        Some(wrapper),
        variant,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn consumer_runtime_package_and_policy_definition_golden() {
        for policy in [ordinary(), native()] {
            assert_eq!(
                policy.definition_relative,
                "liteinst-runtime-build/consumer_policy.rs"
            );
            assert_eq!(policy.runtime.name, "detcore-liteinst");
            assert_eq!(policy.runtime.version, "0.5.4");
            assert_eq!(policy.runtime.manifest, "detcore-liteinst/Cargo.toml");
            assert_eq!(policy.runtime.target, "detcore_liteinst");
            assert_eq!(policy.runtime.target_source, "detcore-liteinst/src/lib.rs");
            assert_eq!(policy.runtime.source_root, ConsumerSourceRoot::Consumer);
            assert_eq!(policy.runtime.kind, ["cdylib"]);
            assert_eq!(policy.runtime.crate_types, ["cdylib"]);
            assert!(policy.wrapper.is_none());
        }
    }
    #[test]
    fn consumer_features_and_profiles_golden() {
        let ordinary = ordinary();
        let native = native();
        let standalone = standalone();
        assert_eq!(ordinary.variants.len(), 1);
        assert_eq!(native.variants.len(), 1);
        assert_eq!(standalone.variants.len(), 1);
        let o = &ordinary.variants[0];
        let n = &native.variants[0];
        let s = &standalone.variants[0];
        assert_eq!(o.requested_features, Vec::<String>::new());
        assert_eq!(n.requested_features, ["exec-native-injection"]);
        assert_eq!(s.requested_features, Vec::<String>::new());
        assert_eq!(o.resolved_features[&Role::Runtime], Vec::<String>::new());
        assert_eq!(
            o.resolved_features[&Role::Instrumentation],
            Vec::<String>::new()
        );
        assert_eq!(
            o.resolved_features[&Role::InGuest],
            ["coordinator-rpc", "launcher", "rcb-clock"]
        );
        assert_eq!(
            n.resolved_features[&Role::Runtime],
            ["exec-native-injection", "exec-private-init"]
        );
        assert_eq!(
            n.resolved_features[&Role::Instrumentation],
            [
                "exec-native-injection",
                "exec-private-init",
                "exec-private-successor",
                "fixed-syscall-gates",
                "native-exec-state"
            ]
        );
        assert_eq!(
            n.resolved_features[&Role::InGuest],
            [
                "coordinator-rpc",
                "exec-native-injection",
                "exec-private-init",
                "exec-private-successor",
                "fixed-syscall-gates",
                "launcher",
                "native-exec-state",
                "rcb-clock"
            ]
        );
        assert_eq!(s.resolved_features[&Role::Runtime], Vec::<String>::new());
        assert_eq!(s.resolved_features[&Role::Wrapper], Vec::<String>::new());
        assert_eq!(
            s.resolved_features[&Role::Instrumentation],
            Vec::<String>::new()
        );
        assert_eq!(
            s.resolved_features[&Role::InGuest],
            ["coordinator-rpc", "launcher", "rcb-clock"]
        );
        assert_eq!(o.profile_name, "liteinst-runtime");
        assert_eq!(n.profile_name, "liteinst-runtime");
        assert_eq!(s.profile_name, "release");
        assert_eq!(
            o.profile,
            serde_json::json!({"opt_level":"3","debuginfo":0,
            "debug_assertions":true,"overflow_checks":true,"test":false})
        );
        assert_eq!(n.profile, o.profile);
        assert_eq!(
            s.profile,
            serde_json::json!({"opt_level":"3","debuginfo":0,
            "debug_assertions":false,"overflow_checks":false,"test":false})
        );
        assert_eq!(o.recipe, ConsumerRecipe::Allocation101LayoutB);
        assert_eq!(n.recipe, ConsumerRecipe::Allocation101LayoutA);
        assert_eq!(s.recipe, ConsumerRecipe::Allocation62);
    }
    #[test]
    fn consumer_nested_source_and_wrapper_golden() {
        let policy = standalone();
        let w = policy.wrapper.unwrap();
        assert_eq!(w.workspace, "liteinst-runtime-build");
        assert_eq!(w.package.name, "hermit-liteinst-runtime-artifact");
        assert_eq!(w.package.version, "0.5.4");
        assert_eq!(w.package.target, "hermit_liteinst_runtime_artifact");
        assert_eq!(w.package.kind, ["lib"]);
        assert_eq!(w.package.crate_types, ["lib"]);
        assert_eq!(w.files.len(), 6);
        assert_eq!(
            w.files,
            BTreeMap::from([
                (
                    FileRole::RootManifest,
                    "liteinst-runtime-build/Cargo.toml".into()
                ),
                (
                    FileRole::RootLock,
                    "liteinst-runtime-build/Cargo.lock".into()
                ),
                (
                    FileRole::BuildScript,
                    "liteinst-runtime-build/build.rs".into()
                ),
                (
                    FileRole::ProofModule,
                    "liteinst-runtime-build/artifact.rs".into()
                ),
                (
                    FileRole::WrapperManifest,
                    "liteinst-runtime-build/runtime/Cargo.toml".into()
                ),
                (
                    FileRole::WrapperSource,
                    "liteinst-runtime-build/runtime/src/lib.rs".into()
                ),
            ])
        );
        assert_eq!(policy.runtime.source_root, ConsumerSourceRoot::Reverie);
        assert_eq!(policy.runtime.name, "reverie-liteinst-preload");
    }
    #[test]
    fn consumer_evidence_labels_golden() {
        assert_eq!(native().variants[0].evidence_labels, BTreeMap::from([
            ("allocation101_layout_a_legacy_v1".into(), "detcore_native_off_alloc101_reviewed_source_current_build_rx_loader_census_v1".into()),
            ("allocation101_layout_a_semantic_v2".into(), "native_off_alloc101_tls140_semantic_v0_reviewed_source_current_build_rx_loader_census_v2".into()),
            ("allocation101_layout_a_tls100_legacy_v2".into(), "detcore_native_off_alloc101_tls100_reviewed_source_current_build_rx_loader_census_v2".into()),
            ("allocation101_layout_a_tls100_semantic_v3".into(), "native_off_alloc101_tls100_semantic_v0_reviewed_source_current_build_rx_loader_census_v3".into()),
            ("allocation101_layout_a_tls130_owner138_legacy_v4".into(), "detcore_native_off_alloc101_tls130_owner138_reviewed_source_current_build_rx_loader_census_v4".into()),
        ]));
        assert_eq!(ordinary().variants[0].evidence_labels, BTreeMap::from([
            ("allocation101_layout_b_legacy_v1".into(), "detcore_ordinary_off_alloc101_reviewed_source_current_build_rx_loader_census_v1".into()),
            ("allocation101_layout_b_semantic_v2".into(), "ordinary_off_alloc101_semantic_v0_reviewed_source_current_build_rx_loader_census_v2".into()),
        ]));
        assert_eq!(standalone().variants[0].evidence_labels, BTreeMap::from([
            ("allocation62_legacy_v1".into(), "standalone_release_off_alloc62_reviewed_source_current_build_rx_loader_census_v1".into()),
            ("allocation62_semantic_v2".into(), "standalone_release_off_alloc62_semantic_v0_reviewed_source_current_build_rx_loader_census_v2".into()),
        ]));
    }
}
