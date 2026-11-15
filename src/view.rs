use crate::config::Theme;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub struct Palette {
    pub background: u32,
    pub surface: u32,
    pub text: u32,
    pub muted: u32,
    pub border: u32,
    pub accent: u32,
    pub accent_text: u32,
    pub warning: u32,
}

impl Palette {
    // Win32 COLORREF is 0x00BBGGRR. Teal provides one stable accent in both themes.
    pub fn for_theme(theme: Theme) -> Self {
        match theme {
            Theme::Light => Self {
                background: 0x00F7F8F7,
                surface: 0x00FFFFFF,
                text: 0x003A3228,
                muted: 0x006B6257,
                border: 0x00DEDCD6,
                accent: 0x00776712,
                accent_text: 0x00FFFFFF,
                warning: 0x003756A1,
            },
            Theme::Dark => Self {
                background: 0x0027231E,
                surface: 0x00332E27,
                text: 0x00F6F4ED,
                muted: 0x00BEB6AB,
                border: 0x00524A3F,
                accent: 0x00CBB85E,
                accent_text: 0x00221C12,
                warning: 0x008DCFF1,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Controls,
    Settings,
    Licenses,
    Setup,
}

pub struct RenderPolicy {
    pub visible: bool,
    pub running: bool,
    pub downloading: bool,
    pub screen: Screen,
    pub reduced_motion: bool,
    animation_end: Option<Instant>,
    pub raster_count: u64,
}

impl Default for RenderPolicy {
    fn default() -> Self {
        Self {
            visible: false,
            running: false,
            downloading: false,
            screen: Screen::Controls,
            reduced_motion: false,
            animation_end: None,
            raster_count: 0,
        }
    }
}

impl RenderPolicy {
    pub fn transition(&mut self, now: Instant) {
        self.animation_end = if self.visible && !self.reduced_motion {
            Some(now + Duration::from_millis(140))
        } else {
            None
        };
    }
    pub fn animation(&mut self, now: Instant) -> f32 {
        match self.animation_end {
            Some(end) if self.visible && now < end && !self.reduced_motion => {
                (end - now).as_secs_f32() / 0.14
            }
            _ => {
                self.animation_end = None;
                0.0
            }
        }
    }
    pub fn timer_interval(&mut self, now: Instant) -> Option<u32> {
        if !self.visible {
            self.animation_end = None;
            return None;
        }
        if self.animation(now) > 0.0 {
            return Some(16);
        }
        if (self.running && self.screen == Screen::Controls)
            || (self.downloading && self.screen == Screen::Settings)
        {
            Some(100)
        } else {
            None
        }
    }
}
