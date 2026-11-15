use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub const SCHEMA: u32 = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    En,
    Ru,
    Fr,
    De,
    Es,
    It,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strength {
    #[default]
    Natural,
    Strong,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema: u32,
    pub input_id: Option<String>,
    pub output_id: Option<String>,
    pub language: Language,
    pub theme: Theme,
    pub reduced_motion: bool,
    pub startup_update_check: bool,
    pub strength: Strength,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            input_id: None,
            output_id: None,
            language: Language::En,
            theme: Theme::Light,
            reduced_motion: false,
            startup_update_check: true,
            strength: Strength::Natural,
        }
    }
}

impl Settings {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > 64 * 1024 {
            return Err("Settings file exceeds 64 KiB".into());
        }
        let mut value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        let schema = value.get("schema").and_then(|x| x.as_u64()).unwrap_or(1);
        if schema > SCHEMA as u64 {
            return Err("Settings belong to a newer application version".into());
        }
        if schema == 1 {
            if let Some(old) = value.get("check_updates").cloned() {
                value["startup_update_check"] = old;
            }
            value["schema"] = SCHEMA.into();
        }
        let settings: Self = serde_json::from_value(value).map_err(|e| e.to_string())?;
        for id in [&settings.input_id, &settings.output_id]
            .into_iter()
            .flatten()
        {
            if id.len() > 4096 || id.contains('\0') {
                return Err("Invalid saved device identifier".into());
            }
        }
        Ok(settings)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read(path) {
            Ok(bytes) => Self::parse(&bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let parent = path.parent().ok_or("Settings directory is missing")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let pending = path.with_extension("pending");
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        use std::io::Write;
        let mut file = fs::File::create(&pending).map_err(|e| e.to_string())?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string())?;
        drop(file);
        replace_file(&pending, path).map_err(|e| e.to_string())
    }
}

pub fn settings_path() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("MicrophoneCleaner/settings.json"))
        .ok_or_else(|| "Windows local application data directory is unavailable".into())
}

pub(crate) fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // SAFETY: Both pointers refer to live, NUL-terminated paths for this call.
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    fs::rename(source, destination)
}
