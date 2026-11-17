use oxa_config::Settings;
use serde_json::json;

#[test]
fn native_local_launch_arguments_remain_literal_in_user_settings() {
    let local = json!({"binary":"/opt/example/oxe-server","models":{"synthetic":{"model":"/models/synthetic.gguf","arguments":["--engine","stub","--ctx-size","4096","--flag=literal","+literal","--engine","stub"]}}});
    let settings = Settings::from_values(json!({"oxa":{"local":local}}), json!({}));
    assert_eq!(settings.get("oxa.local"), Some(&local));
}
