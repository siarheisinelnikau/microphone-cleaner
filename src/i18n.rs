use crate::config::Language;
use std::{collections::BTreeMap, sync::OnceLock};

pub const LANGUAGES: [Language; 6] = [
    Language::En,
    Language::Ru,
    Language::Fr,
    Language::De,
    Language::Es,
    Language::It,
];
pub const LANGUAGE_NAMES: [&str; 6] = [
    "English",
    "Русский",
    "Français",
    "Deutsch",
    "Español",
    "Italiano",
];
pub const RESOURCES: [&str; 6] = [
    include_str!("../locales/en.json"),
    include_str!("../locales/ru.json"),
    include_str!("../locales/fr.json"),
    include_str!("../locales/de.json"),
    include_str!("../locales/es.json"),
    include_str!("../locales/it.json"),
];
static DICTIONARIES: OnceLock<[BTreeMap<String, String>; 6]> = OnceLock::new();
pub fn index(language: Language) -> usize {
    LANGUAGES.iter().position(|l| *l == language).unwrap_or(0)
}
pub fn tr(language: Language, key: &str) -> &'static str {
    let dictionaries = DICTIONARIES.get_or_init(|| {
        RESOURCES.map(|json| serde_json::from_str(json).expect("embedded translations"))
    });
    dictionaries[index(language)]
        .get(key)
        .or_else(|| dictionaries[0].get(key))
        .map(String::as_str)
        .unwrap_or("…")
}
pub fn format(language: Language, key: &str, token: &str, value: &str) -> String {
    tr(language, key).replace(&format!("{{{token}}}"), value)
}
