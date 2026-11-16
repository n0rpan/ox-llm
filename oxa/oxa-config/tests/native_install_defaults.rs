//! Retirement activation defaults; the retirement PR stays draft until native E2E acceptance.
use oxa_config::Settings;
use serde_json::json;

#[test]
fn fresh_user_selects_both_native_paths() {
    let settings = Settings::from_values(json!({}), json!({}));
    assert!(settings.bool("oxa.engine.inProcessCloud", false));
    assert!(settings.bool("oxa.engine.inProcessLocal", false));
}

#[test]
fn explicit_user_choices_remain_effective() {
    let settings = Settings::from_values(
        json!({"oxa":{"engine":{
            "inProcessCloud":false,"inProcessLocal":false
        }}}),
        json!({}),
    );
    assert!(!settings.bool("oxa.engine.inProcessCloud", true));
    assert!(!settings.bool("oxa.engine.inProcessLocal", true));
}
