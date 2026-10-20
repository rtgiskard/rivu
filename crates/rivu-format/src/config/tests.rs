use super::*;

#[test]
fn current_toml_contract_rejects_explicit_noncurrent_versions_and_invalid_values() {
    for input in [
        "schema_version = 0\n",
        "schema_version = 2\n",
        "schema_version = '1'\n",
        "schema_version = true\n",
        "schema_version = 1.0\n",
        "schema_version = -1\n",
        "schema_version = {}\n",
        "schema_version = 1\nunknown = true\n",
        "schema_version = 1\n[interface]\nui_theme = 'rivu_dark'\n",
        "schema_version = 1\n[playback]\nvolume_percent = 101\n",
        "schema_version = 1\n[library]\nroots = ['/Music', '/Music/Live']\n",
    ] {
        assert!(validate_toml(input).is_err(), "accepted {input}");
    }
    assert_eq!(
        from_toml("schema_version = 1\n").unwrap(),
        Config::default()
    );
}

#[test]
fn absent_version_uses_current_schema_and_serializes_explicitly() {
    for input in ["", "[interface]\n"] {
        assert_eq!(from_toml(input).unwrap(), Config::default());
    }
    let config = from_toml(
        "[interface]\nlanguage = 'ZH_cn'\npage_size = 42\n[playback]\nvolume_percent = 25\nrepeat = 'one'\n",
    )
    .unwrap();
    assert_eq!(config.ui_language, "zh-CN");
    assert_eq!(config.page_size, 42);
    assert_eq!(config.volume, 0.25);
    assert_eq!(config.repeat, RepeatMode::One);
    let saved = to_toml(&config).unwrap();
    let document: toml::Value = toml::from_str(&saved).unwrap();
    assert_eq!(
        document["schema_version"].as_integer(),
        Some(i64::from(CONFIG_SCHEMA_VERSION))
    );
    assert_eq!(from_toml(&saved).unwrap(), config);
}

#[test]
fn absent_version_keeps_strict_parse_and_validation_causes() {
    for input in [
        "unknown = true\n",
        "[interface]\nui_theme = 'rivu_dark'\n",
        "[playback]\nvolume_percent = 101\n",
        "[library]\nroots = ['/Music', '/Music/Live']\n",
    ] {
        let unversioned = from_toml(input).unwrap_err();
        let versioned = from_toml(&format!(
            "schema_version = {CONFIG_SCHEMA_VERSION}\n{input}"
        ))
        .unwrap_err();
        // Parse locations differ after adding the version line; compare the
        // structured TOML message, or the shared validation root cause.
        if let Some(cause) = unversioned.downcast_ref::<toml::de::Error>() {
            assert_eq!(
                cause.message(),
                versioned
                    .downcast_ref::<toml::de::Error>()
                    .unwrap()
                    .message()
            );
        } else {
            assert_eq!(
                unversioned.root_cause().to_string(),
                versioned.root_cause().to_string()
            );
        }
    }
}

#[test]
fn explicit_versions_gate_current_fields_and_syntax_errors_keep_the_parse_cause() {
    let error =
        from_toml("schema_version = 2\n[playback]\nvolume_percent = 'future'\n").unwrap_err();
    assert!(error.downcast_ref::<toml::de::Error>().is_none());
    for input in ["[interface", "schema_version = 1\n[interface"] {
        let error = from_toml(input).unwrap_err();
        assert!(error.downcast_ref::<toml::de::Error>().is_some());
    }
}

#[test]
fn grouped_round_trip_uses_shared_defaults_validation_and_normalization() {
    let text = "schema_version = 1\n[interface]\nlanguage = 'ZH_cn'\nfont = ''\n[playback]\nrepeat = 'one'\nvolume_percent = 25\n";
    let config = from_toml(text).unwrap();
    assert_eq!(config.ui_language, "zh-CN");
    assert_eq!(config.ui_font, Config::default().ui_font);
    assert_eq!(config.repeat, RepeatMode::One);
    assert_eq!(config.volume, 0.25);
    let saved = to_toml(&config).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert!(
        to_toml(&Config {
            volume: f32::NAN,
            ..config
        })
        .is_err()
    );
}
