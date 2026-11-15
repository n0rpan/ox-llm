#![cfg(feature = "cloud-only")]
use oxc_gateway::catalog::{Access, Catalog, Error, Model, ModelInfo, Provider};

fn model(id: &str, provider: Provider, context: u32, output: u32) -> Model {
    Model::new(
        id,
        provider,
        "private-upstream",
        "https://example.invalid/private-route",
        context,
        output,
    )
    .unwrap()
}

#[test]
fn keyless_discovery_is_sorted_and_uses_each_models_limits() {
    let catalog = Catalog::new(
        Access::Enabled,
        [
            model("large", Provider::Anthropic, 1_000_000, 32_768),
            model("fast", Provider::OpenRouter, 64_000, 4096),
        ],
    )
    .unwrap();
    assert_eq!(
        catalog.models().map(|m| m.id).collect::<Vec<_>>(),
        ["fast", "large"]
    );
    assert_eq!(
        catalog.resolve(Some("large")).unwrap().info(),
        ModelInfo {
            id: "large",
            provider: Provider::Anthropic,
            context_window: 1_000_000,
            max_output: 32_768,
        }
    );
    assert_eq!(
        catalog.resolve(Some("fast")).unwrap().info().max_output,
        4096
    );
    assert_eq!(catalog.resolve(None).unwrap().info().id, "fast");
    assert_eq!(
        catalog.resolve(Some("typo")).unwrap_err(),
        Error::UnknownModel
    );
}

#[test]
fn cloud_is_off_by_default_and_offline_hides_and_refuses_models() {
    assert_eq!(Access::default(), Access::Disabled);
    for (access, refusal) in [
        (Access::default(), Error::Disabled),
        (Access::Offline, Error::Offline),
    ] {
        let catalog =
            Catalog::new(access, [model("cloud", Provider::Anthropic, 4096, 512)]).unwrap();
        assert_eq!(catalog.models().count(), 0);
        assert_eq!(catalog.resolve(None).unwrap_err(), refusal);
        assert_eq!(catalog.resolve(Some("cloud")).unwrap_err(), refusal);
    }
}

#[test]
fn empty_and_duplicate_catalogs_do_not_silently_choose_a_model() {
    let empty = Catalog::new(Access::Enabled, []).unwrap();
    assert_eq!(empty.models().count(), 0);
    assert_eq!(empty.resolve(None).unwrap_err(), Error::UnknownModel);
    assert_eq!(
        Catalog::new(
            Access::Enabled,
            [
                model("same", Provider::Anthropic, 4096, 512),
                model("same", Provider::OpenAi, 4096, 512),
            ]
        )
        .unwrap_err(),
        Error::DuplicateModel
    );
}

#[test]
fn token_limits_are_positive_integers_and_output_fits_context() {
    for (context, output) in [(0, 1), (1, 0), (100, 101)] {
        assert_eq!(
            Model::new(
                "cloud",
                Provider::Anthropic,
                "upstream",
                "https://example.invalid",
                context,
                output
            )
            .unwrap_err(),
            Error::InvalidLimits
        );
    }
    assert_eq!(
        model("cloud", Provider::Anthropic, u32::MAX, 1)
            .info()
            .context_window,
        u32::MAX
    );
}

#[test]
fn invalid_endpoints_cannot_embed_credentials_or_url_secrets() {
    for endpoint in [
        "bad-url",
        "file:///tmp/upstream",
        "https://user:synthetic@example.invalid",
        "https://user@example.invalid",
        "https://example.invalid/?private=synthetic",
        "https://example.invalid/#private",
    ] {
        let error = Model::new(
            "cloud",
            Provider::Anthropic,
            "upstream",
            endpoint,
            4096,
            512,
        )
        .unwrap_err();
        assert_eq!(error, Error::InvalidEndpoint);
        assert!(!format!("{error:?}").contains(endpoint));
    }
    assert!(Model::new(
        "cloud",
        Provider::Anthropic,
        "upstream",
        "http://127.0.0.1:1234/v1",
        4096,
        512
    )
    .is_ok());
}

#[test]
fn public_metadata_and_debug_hide_private_routing_configuration() {
    let catalog = Catalog::new(
        Access::Enabled,
        [model("cloud", Provider::Anthropic, 4096, 512)],
    )
    .unwrap();
    let selected = catalog.resolve(None).unwrap();
    assert_eq!(selected.upstream(), "private-upstream");
    assert_eq!(selected.endpoint().path(), "/private-route");
    for debug in [format!("{catalog:?}"), format!("{:?}", selected.info())] {
        assert!(debug.contains("cloud"));
        assert!(!debug.contains("private-upstream"));
        assert!(!debug.contains("private-route"));
        assert!(!debug.contains("example.invalid"));
    }
}

#[test]
fn invalid_names_and_upstream_controls_are_refused_without_echoing_them() {
    for (id, upstream) in [
        ("", "upstream"),
        ("bad id", "upstream"),
        ("cloud", ""),
        ("cloud", "upstream\r\nheader"),
    ] {
        assert_eq!(
            Model::new(
                id,
                Provider::Anthropic,
                upstream,
                "https://example.invalid",
                4096,
                512
            )
            .unwrap_err(),
            Error::InvalidModel
        );
    }
}
