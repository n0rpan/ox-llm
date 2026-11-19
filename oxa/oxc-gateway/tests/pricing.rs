#![cfg(feature = "pricing")]
use oxc_gateway::pricing::{Cache, Price};
use serde_json::{json, Value};

fn price(rates: Value) -> Price {
    Price::from_model(&json!({"pricing": rates})).unwrap()
}
fn flat() -> Price {
    price(json!({"input_per_million": 1, "output_per_million": 2}))
}
fn cached() -> Price {
    price(json!({"input_per_million": 1, "output_per_million": 2,
        "cache_read_per_million": 0.1, "cache_write_per_million": 1.25}))
}
fn close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("known finite price");
    assert!(
        (actual - expected).abs() <= expected.abs() * 1e-12 + f64::MIN_POSITIVE,
        "actual {actual}, expected {expected}"
    );
}

#[test]
fn provider_defaults_price_reads_and_writes_separately() {
    for (provider, upstream, expected, reservation) in [
        ("openrouter", "synthetic/model", 0.001008, 0.00133),
        ("openai", "synthetic/model", 0.001008, 0.001064),
        ("anthropic", "synthetic/model", 0.000193, 0.002064),
        ("openrouter", "anthropic/synthetic", 0.001048, 0.00258),
    ] {
        let price = flat();
        close(
            price.cost(
                provider,
                upstream,
                1000,
                4,
                Cache::Tokens { read: 950, write: 40 },
            ),
            expected,
        );
        close(price.reservation(provider, upstream, 1000, 32), reservation);
    }
    close(
        flat().cost(
            "anthropic",
            "synthetic/model",
            1000,
            4,
            Cache::Tokens { read: 950, write: 0 },
        ),
        0.000153,
    );
}

#[test]
fn explicit_cache_prices_do_not_charge_cached_tokens_as_uncached_input() {
    for provider in ["openrouter", "openai", "anthropic"] {
        // 10 uncached + 950 * 0.1 read + 40 * 1.25 write + 4 * 2 output.
        close(
            cached().cost(
                provider,
                "synthetic/model",
                1000,
                4,
                Cache::Tokens { read: 950, write: 40 },
            ),
            0.000163,
        );
        close(
            cached().reservation(provider, "synthetic/model", 1000, 32),
            if provider == "openrouter" {
                0.0016425
            } else {
                0.001314
            },
        );
    }
    let hourly = price(json!({"input": 1, "output": 2,
        "cache_read_per_million": 0.1, "cache_write_per_million": 2}));
    close(
        hourly.cost(
            "openrouter",
            "synthetic/model",
            1000,
            4,
            Cache::Tokens { read: 950, write: 40 },
        ),
        0.000193,
    );
    close(
        hourly.reservation("openrouter", "synthetic/model", 1000, 32),
        0.00258,
    );
}

#[test]
fn reservation_keeps_openrouter_headroom() {
    close(
        flat().reservation("openrouter", "synthetic/model", 1000, 32),
        0.00133,
    );
    close(
        flat().reservation("openai", "synthetic/model", 1000, 32),
        0.001064,
    );
}

#[test]
fn tier_boundary_is_inclusive_and_changes_price_above_the_bound() {
    let price = price(json!({"input": 1, "output": 2,
        "cache_read_per_million": 0.1, "cache_write_per_million": 1.25,
        "tiers": [{"max_prompt_tokens": 100, "input_per_million": 1},
                  {"max_prompt_tokens": null, "input_per_million": 2}]}));
    for (prompt, expected) in [(99, 0.000026), (100, 0.000027), (101, 0.000039)] {
        close(
            price.cost(
                "openrouter",
                "synthetic/model",
                prompt,
                4,
                Cache::Tokens { read: 90, write: 0 },
            ),
            expected,
        );
    }
    close(
        price.reservation("openrouter", "synthetic/model", 1000, 32),
        0.00258,
    );
}

#[test]
fn reservation_bounds_every_tier_including_cache_write_and_output_premiums() {
    let price = price(json!({"input": 1, "output": 2,
    "tiers": [
        {"max_prompt_tokens": 100, "cache_write_per_million": 3},
        {"max_prompt_tokens": null, "input_per_million": 0.5, "output_per_million": 9}
    ]}));
    let reserved = price
        .reservation("openrouter", "synthetic/model", 1000, 32)
        .unwrap();
    close(Some(reserved), 0.00411);
    for prompt in [1, 100, 101, 1000] {
        for cache in [
            Cache::Tokens { read: 0, write: 0 },
            Cache::Tokens {
                read: 0,
                write: prompt,
            },
            Cache::Invalid,
        ] {
            assert!(
                price
                    .cost("openrouter", "synthetic/model", prompt, 32, cache)
                    .unwrap()
                    <= reserved
            );
        }
    }
}

#[test]
fn malformed_or_unordered_tiers_are_unknown_even_with_a_free_marker() {
    let mut invalid = vec![
        json!([]),
        json!(null),
        json!(true),
        json!({}),
        json!([false]),
        json!([{"max_prompt_tokens": 200}]),
        json!([{}, {"max_prompt_tokens": null}]),
        json!([{"max_prompt_tokens": 100}, {"max_prompt_tokens": 50}, {"max_prompt_tokens": null}]),
        json!([{"max_prompt_tokens": 100}, {"max_prompt_tokens": 100}, {"max_prompt_tokens": null}]),
    ];
    for bound in [
        json!(true),
        json!(false),
        json!(0),
        json!(-1),
        json!(1.0),
        json!(100_000_001_u64),
        json!("100"),
    ] {
        invalid.push(json!([{"max_prompt_tokens": bound}, {"max_prompt_tokens": null}]));
    }
    for tiers in invalid {
        for free in [false, true] {
            assert!(
                Price::from_model(&json!({"pricing": {"input": 1, "output": 2, "tiers": tiers},
                "free": free}))
                .is_none(),
                "tiers {tiers}, free {free}"
            );
        }
    }
}

#[test]
fn tier_count_is_bounded_and_each_tier_requires_valid_rates() {
    let mut tiers: Vec<_> = (1..64).map(|n| json!({"max_prompt_tokens": n})).collect();
    tiers.push(json!({"max_prompt_tokens": null}));
    let mut model = json!({"pricing": {"input": 1, "output": 2, "tiers": tiers}});
    assert!(Price::from_model(&model).is_some());
    model["pricing"]["tiers"]
        .as_array_mut()
        .unwrap()
        .insert(63, json!({"max_prompt_tokens": 64}));
    assert!(Price::from_model(&model).is_none());
    model["pricing"]["tiers"] = json!([{"max_prompt_tokens": null, "input": -1}]);
    assert!(Price::from_model(&model).is_none());
}

#[test]
fn rate_types_sign_and_upper_limit_are_validated_for_every_rate() {
    for name in [
        "input_per_million",
        "output_per_million",
        "cache_read_per_million",
        "cache_write_per_million",
    ] {
        for invalid in [
            json!(true),
            json!(false),
            json!(-0.1),
            json!(1e12 + 1.0),
            json!("1"),
            json!({}),
            json!([]),
        ] {
            let mut raw = json!({"pricing": {"input_per_million": 1, "output_per_million": 2}});
            raw["pricing"][name] = invalid;
            assert!(Price::from_model(&raw).is_none(), "rate {name}: {raw}");
        }
        for valid in [json!(0), json!(1e-20), json!(1e12)] {
            let mut raw = json!({"pricing": {"input_per_million": 1, "output_per_million": 2}});
            raw["pricing"][name] = valid;
            assert!(Price::from_model(&raw).is_some());
        }
    }
    for rates in [
        json!({}),
        json!({"input": null, "output": 2}),
        json!({"input": 1}),
        serde_json::from_str::<Value>(r#"{"input": 1e10000, "output": 2}"#).unwrap(),
    ] {
        assert!(Price::from_model(&json!({"pricing": rates})).is_none());
    }
}

#[test]
fn aliases_and_optional_cache_rates_are_normalized_without_inventing_prices() {
    let price = price(json!({"input": 1, "output": 2,
        "cache_read_per_million": null, "cache_write_per_million": null}));
    assert_eq!(
        price.normalized(),
        &json!({"input_per_million": 1.0, "output_per_million": 2.0})
    );
    close(
        price.cost(
            "openai",
            "synthetic/model",
            1000,
            4,
            Cache::Tokens { read: 950, write: 40 },
        ),
        0.001008,
    );
    assert!(
        Price::from_model(&json!({"pricing": {"input_per_million": false, "input": 1, "output": 2}}))
            .is_none()
    );
}

#[test]
fn unknown_prices_stay_unknown_and_explicit_free_models_cost_zero() {
    for raw in [
        json!({}),
        json!({"free": false}),
        json!({"free": "true"}),
        json!({"pricing": {"input": -1, "output": 2}}),
    ] {
        assert!(Price::from_model(&raw).is_none());
    }
    for raw in [
        json!({"free": true}),
        json!({"free": true, "pricing": {"input": "invalid", "output": 2}}),
    ] {
        let price = Price::from_model(&raw).unwrap();
        assert_eq!(
            price.cost("openrouter", "synthetic/model", 1000, 4, Cache::Invalid),
            Some(0.0)
        );
        assert_eq!(
            price.reservation("openrouter", "synthetic/model", 1000, 32),
            Some(0.0)
        );
    }

    let unpriced = Price::unpriced();
    assert_eq!(
        unpriced.reservation("openrouter", "synthetic/model", 1000, 32),
        None
    );
    assert_eq!(
        unpriced.cost("openrouter", "synthetic/model", 1000, 4, Cache::Invalid),
        None
    );
}

#[test]
fn invalid_cache_composition_uses_the_highest_input_rate_without_overflow() {
    for cache in [
        Cache::Invalid,
        Cache::Tokens { read: 999, write: 9 },
        Cache::Tokens {
            read: u64::MAX,
            write: u64::MAX,
        },
    ] {
        close(
            cached().cost("openrouter", "synthetic/model", 1000, 4, cache),
            0.001258,
        );
    }
}

#[test]
fn token_accounting_bounds_are_inclusive_and_excess_is_refused() {
    let price = flat();
    let uncached = Cache::Tokens { read: 0, write: 0 };
    close(
        price.cost("openai", "synthetic/model", 100_000_000, 4, uncached),
        100.000008,
    );
    close(
        price.cost("openai", "synthetic/model", 1000, 10_000_000, uncached),
        20.001,
    );
    close(
        price.reservation("openai", "synthetic/model", 100_000_000, 10_000_000),
        120.0,
    );
    for (prompt, output) in [(100_000_001, 4), (1000, 10_000_001), (u64::MAX, u64::MAX)] {
        assert_eq!(
            price.cost("openai", "synthetic/model", prompt, output, uncached),
            None
        );
        assert_eq!(
            price.reservation("openai", "synthetic/model", prompt, output),
            None
        );
    }
}

#[test]
fn fractional_and_tiny_decimal_prices_retain_cache_discounts() {
    let decimal = price(json!({"input": 0.111, "output": 4.44,
        "cache_read_per_million": 0.0111, "cache_write_per_million": 0.13875}));
    close(
        decimal.cost(
            "openrouter",
            "synthetic/model",
            979,
            83,
            Cache::Tokens { read: 950, write: 20 },
        ),
        0.000382839,
    );
    close(
        decimal.reservation("openrouter", "synthetic/model", 1000, 32),
        0.0003510375,
    );
    let tiny = price(json!({"input": 1e-20, "output": 3e-20}));
    close(
        tiny.cost(
            "openrouter",
            "synthetic/model",
            1000,
            4,
            Cache::Tokens { read: 950, write: 40 },
        ),
        1.012e-23,
    );
    close(
        tiny.reservation("openrouter", "synthetic/model", 1000, 32),
        1.37e-23,
    );
}
