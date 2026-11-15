mod support;
use microphone_cleaner::{
    config::{Language, Settings, Strength, Theme},
    i18n::{self, tr},
    provider::{self, Device},
    view::{RenderPolicy, Screen},
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[test]
fn settings_migrate_without_losing_source_devices_or_opt_out() {
    let settings = Settings::parse(br#"{"schema":1,"input_id":"physical-mic","output_id":"cable-render","check_updates":false,"language":"ru","theme":"dark"}"#).unwrap();
    assert_eq!(settings.input_id.as_deref(), Some("physical-mic"));
    assert_eq!(settings.output_id.as_deref(), Some("cable-render"));
    assert_eq!(settings.language, Language::Ru);
    assert_eq!(settings.theme, Theme::Dark);
    assert!(!settings.startup_update_check);
    assert_eq!(settings.strength, Strength::Natural);
    assert_eq!(settings.schema, 2);
}

#[test]
fn unknown_schema_and_corrupt_settings_are_not_silently_overwritten() {
    let directory = support::Scratch::new();
    let path = directory.0.join("settings.json");
    std::fs::write(&path, b"broken").unwrap();
    assert!(Settings::load(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"broken");
    assert!(Settings::parse(br#"{"schema":500}"#).is_err());
    assert!(Settings::parse(br#"{"input_id":"\u0000"}"#).is_err());
}

#[test]
fn atomic_preferences_save_and_reload_all_user_choices() {
    let directory = support::Scratch::new();
    let path = directory.0.join("settings.json");
    let settings = Settings {
        input_id: Some("endpoint:id".into()),
        output_id: Some("cable:id".into()),
        language: Language::Fr,
        theme: Theme::Dark,
        reduced_motion: true,
        startup_update_check: false,
        strength: Strength::Strong,
        ..Settings::default()
    };
    settings.save(&path).unwrap();
    settings.save(&path).unwrap();
    let restored = Settings::load(&path).unwrap();
    assert_eq!(
        serde_json::to_value(settings).unwrap(),
        serde_json::to_value(restored).unwrap()
    );
    assert!(!path.with_extension("pending").exists());
}

fn device(id: &str, name: &str) -> Device {
    Device {
        id: id.into(),
        name: name.into(),
    }
}

#[test]
fn a_real_cable_pair_is_required_and_speakers_are_never_routes() {
    let outputs = [
        device("speakers", "Speakers (Realtek Audio)"),
        device("out", "CABLE Input (VB-Audio Virtual Cable)"),
    ];
    assert!(provider::cable_routes(&outputs, &[]).is_empty());
    let inputs = [
        device("mic", "Microphone (USB Audio)"),
        device("in", "CABLE Output (VB-Audio Virtual Cable)"),
    ];
    let routes = provider::cable_routes(&outputs, &inputs);
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].render.id, "out");
    assert_eq!(routes[0].microphone.id, "in");
    assert!(provider::is_cable_capture(&inputs[1].name));
    assert!(!provider::is_cable_capture(&inputs[0].name));
}

#[test]
fn provider_matching_rejects_lookalikes_ambiguity_and_crossed_cables() {
    let output = [device("out", "CABLE-A Input (VB-Audio Cable A)")];
    let wrong = [device("in", "CABLE-B Output (VB-Audio Cable B)")];
    assert!(provider::cable_routes(&output, &wrong).is_empty());
    assert!(provider::cable_routes(&[device("out", "CABLE Input (Speakers)")], &wrong).is_empty());
    let ambiguous = [
        device("a", "CABLE-A Output (VB-Audio Cable A)"),
        device("b", "CABLE-A Output (VB-Audio Cable A)"),
    ];
    assert!(provider::cable_routes(&output, &ambiguous).is_empty());
}

fn placeholders(text: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut remaining = text;
    while let Some((_, rest)) = remaining.split_once('{') {
        let (name, next) = rest.split_once('}').unwrap();
        result.push(name);
        remaining = next;
    }
    result.sort_unstable();
    result
}

#[test]
fn every_translation_is_complete_and_preserves_format_tokens() {
    let dictionaries =
        i18n::RESOURCES.map(|json| serde_json::from_str::<BTreeMap<String, String>>(json).unwrap());
    let base = &dictionaries[0];
    assert!(base.len() >= 65);
    for dictionary in &dictionaries[1..] {
        assert_eq!(
            base.keys().collect::<Vec<_>>(),
            dictionary.keys().collect::<Vec<_>>()
        );
        for (key, text) in dictionary {
            assert!(!text.is_empty());
            assert_eq!(placeholders(&base[key]), placeholders(text));
        }
    }
    assert_eq!(tr(Language::Ru, "settings"), "Настройки");
}

#[test]
fn settled_and_hidden_views_do_not_schedule_animation_or_renders() {
    let now = Instant::now();
    let mut view = RenderPolicy::default();
    view.transition(now);
    assert_eq!(view.timer_interval(now), None);
    view.visible = true;
    assert_eq!(view.timer_interval(now), None);
    view.transition(now);
    assert_eq!(view.timer_interval(now), Some(16));
    assert_eq!(view.timer_interval(now + Duration::from_millis(150)), None);
    view.running = true;
    assert_eq!(view.timer_interval(now), Some(100));
    view.visible = false;
    assert_eq!(view.timer_interval(now), None);
    assert_eq!(view.raster_count, 0);
}

#[test]
fn reduced_motion_and_non_meter_screens_stay_static() {
    let now = Instant::now();
    let mut view = RenderPolicy::default();
    view.visible = true;
    view.running = true;
    view.screen = Screen::Settings;
    view.reduced_motion = true;
    view.transition(now);
    assert_eq!(view.animation(now), 0.0);
    assert_eq!(view.timer_interval(now), None);
    view.downloading = true;
    assert_eq!(view.timer_interval(now), Some(100));
    view.screen = Screen::Licenses;
    assert_eq!(view.timer_interval(now), None);
}
