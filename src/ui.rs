#![allow(clippy::too_many_arguments)]

use crate::{
    audio::{self, Catalog, RunningInfo, Session},
    config::{self, Language, Settings, Strength, Theme},
    i18n::{self, tr},
    provider,
    transport::Signals,
    updates::{self, Check, Progress, UpdateError},
    view::{Palette, RenderPolicy, Screen},
};
use std::{
    cell::RefCell,
    mem::{size_of, zeroed},
    path::PathBuf,
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::Ordering,
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    System::{
        LibraryLoader::GetModuleHandleW,
        SystemServices::{SS_LEFT, SS_NOPREFIX},
        Threading::*,
    },
    UI::{Controls::*, HiDpi::*, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
};

const CLASS: &str = "MicrophoneCleaner.Window.v1";
const WAKE: u32 = WM_APP + 1;
const TRAY: u32 = WM_APP + 2;
const RESTORE: u32 = WM_APP + 3;
const PULSE: usize = 1;
const ID_START: usize = 100;
const ID_INPUT: usize = 101;
const ID_OUTPUT: usize = 102;
const ID_CLEANUP: usize = 103;
const ID_STRENGTH: usize = 104;
const ID_REFRESH: usize = 105;
const ID_SETTINGS: usize = 106;
const ID_HIDE: usize = 107;
const ID_BACK: usize = 108;
const ID_THEME: usize = 109;
const ID_LANGUAGE: usize = 110;
const ID_MOTION: usize = 111;
const ID_STARTUP: usize = 112;
const ID_CHECK: usize = 113;
const ID_INSTALL: usize = 114;
const ID_CANCEL: usize = 115;
const ID_SETUP: usize = 116;
const ID_WEBSITE: usize = 117;
const ID_LICENSES: usize = 118;
const ID_QUIT: usize = 119;
const ID_STATUS: usize = 200;
const ID_UPDATE_STATUS: usize = 201;
const ID_INPUT_DB: usize = 202;
const ID_OUTPUT_DB: usize = 203;

thread_local! { static APP: RefCell<Option<App>> = const { RefCell::new(None) }; }
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

enum AudioCommand {
    Refresh,
    Start {
        input: String,
        output: String,
        strength: Strength,
        bypass: bool,
    },
    Stop,
    Quit,
}
enum Event {
    Catalog(Result<Catalog, String>),
    Started(Result<(RunningInfo, Arc<Signals>), String>),
    Stopped,
    Lost,
    Checked(Result<Check, UpdateError>),
    Staged(Result<updates::InstallPlan, UpdateError>),
    Armed(Result<(), UpdateError>),
}

#[derive(Clone)]
struct Events {
    sender: SyncSender<Event>,
    hwnd: isize,
}
impl Events {
    fn send(&self, event: Event) {
        if self.sender.try_send(event).is_ok() {
            // SAFETY: This is a nonblocking wake only. The UI drains the queue on its own thread.
            unsafe {
                PostMessageW(self.hwnd as HWND, WAKE, 0, 0);
            }
        }
    }
}

fn audio_controller(events: Events) -> Result<(SyncSender<AudioCommand>, JoinHandle<()>), String> {
    let (sender, receiver) = mpsc::sync_channel(8);
    let worker = thread::Builder::new()
        .name("audio-control".into())
        .spawn(move || {
            let mut session: Option<Session> = None;
            loop {
                let command = if session.is_some() {
                    match receiver.recv_timeout(Duration::from_millis(50)) {
                        Ok(command) => Some(command),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                } else {
                    match receiver.recv() {
                        Ok(command) => Some(command),
                        Err(_) => break,
                    }
                };
                match command {
                    Some(AudioCommand::Refresh) => events.send(Event::Catalog(audio::enumerate())),
                    Some(AudioCommand::Start {
                        input,
                        output,
                        strength,
                        bypass,
                    }) => {
                        drop(session.take());
                        match Session::start(&input, &output, strength, bypass) {
                            Ok(started) => {
                                events.send(Event::Started(Ok((
                                    started.info.clone(),
                                    started.signals.clone(),
                                ))));
                                session = Some(started);
                            }
                            Err(error) => events.send(Event::Started(Err(error))),
                        }
                    }
                    Some(AudioCommand::Stop) => {
                        drop(session.take());
                        events.send(Event::Stopped);
                    }
                    Some(AudioCommand::Quit) => break,
                    None => {}
                }
                if session
                    .as_ref()
                    .is_some_and(|s| s.signals.errors.load(Ordering::Acquire) != 0)
                {
                    drop(session.take());
                    events.send(Event::Lost);
                }
            }
            drop(session);
        })
        .map_err(|e| e.to_string())?;
    Ok((sender, worker))
}

enum AudioState {
    Stopped,
    Starting,
    Running(RunningInfo),
    Stopping,
    Failed,
    Lost,
}
enum UpdateState {
    Idle,
    Checking,
    Current,
    Available(updates::Available),
    Downloading,
    Installing,
    Failed(UpdateError),
}

struct App {
    hwnd: HWND,
    instance: HINSTANCE,
    settings: Settings,
    settings_path: PathBuf,
    catalog: Catalog,
    audio: AudioState,
    signals: Option<Arc<Signals>>,
    bypass: bool,
    policy: RenderPolicy,
    events: Events,
    receiver: Receiver<Event>,
    commands: SyncSender<AudioCommand>,
    controller: Option<JoinHandle<()>>,
    update_worker: Option<JoinHandle<()>>,
    update: UpdateState,
    progress: Arc<Progress>,
    update_available: Option<updates::Available>,
    children: Vec<HWND>,
    inputs: Vec<Option<String>>,
    outputs: Vec<Option<String>>,
    brush: HBRUSH,
    fonts: [HFONT; 3],
    icon: HICON,
    large_icon: HICON,
    last_tray_tip: RefCell<String>,
    dpi: u32,
    scroll: i32,
    client_height: i32,
    timer: Option<u32>,
    dirty: bool,
    notice: Option<&'static str>,
    detail: String,
    levels: [i32; 2],
    percent: u64,
    transition: f32,
    taskbar_created: u32,
}

impl App {
    fn text(&self, key: &str) -> &'static str {
        tr(self.settings.language, key)
    }
    fn px(&self, value: i32) -> i32 {
        (value as i64 * self.dpi as i64 / 96) as i32
    }
    fn palette(&self) -> Palette {
        Palette::for_theme(self.settings.theme)
    }
    fn is_running(&self) -> bool {
        matches!(self.audio, AudioState::Running(_))
    }
    fn pending(&self) -> bool {
        matches!(self.audio, AudioState::Starting | AudioState::Stopping)
    }
    fn status_key(&self) -> &'static str {
        match self.audio {
            AudioState::Stopped => "status_stopped",
            AudioState::Starting => "status_starting",
            AudioState::Stopping => "busy",
            AudioState::Running(_) => {
                if self.bypass {
                    "status_bypass"
                } else {
                    "status_cleaning"
                }
            }
            AudioState::Failed => "status_error",
            AudioState::Lost => "status_device_lost",
        }
    }

    fn set_text(&self, id: usize, text: &str) {
        // SAFETY: Child IDs are owned by this window. Missing optional controls are ignored.
        unsafe {
            let child = GetDlgItem(self.hwnd, id as i32);
            if !child.is_null() {
                SetWindowTextW(child, wide(text).as_ptr());
            }
        }
    }

    fn save(&mut self) {
        if let Err(error) = self.settings.save(&self.settings_path) {
            self.notice = Some("save_error");
            self.detail = error;
        }
    }

    fn command(&mut self, command: AudioCommand) -> bool {
        if self.commands.try_send(command).is_err() {
            self.notice = Some("busy");
            false
        } else {
            true
        }
    }

    fn changed(&mut self, animate: bool) {
        self.dirty = true;
        if self.policy.visible {
            if animate {
                self.policy.transition(Instant::now());
            }
            self.rebuild();
        }
        self.sync_timer();
        self.update_tray();
    }

    fn sync_timer(&mut self) {
        self.policy.running = self.is_running();
        self.policy.downloading = matches!(self.update, UpdateState::Downloading);
        self.policy.reduced_motion = self.settings.reduced_motion;
        let interval = self.policy.timer_interval(Instant::now());
        if interval == self.timer {
            return;
        }
        // SAFETY: The timer belongs to this UI window, and is removed before it becomes hidden.
        unsafe {
            KillTimer(self.hwnd, PULSE);
            if let Some(interval) = interval {
                SetTimer(self.hwnd, PULSE, interval, None);
            }
        }
        self.timer = interval;
    }

    fn tick(&mut self) {
        if !self.policy.visible {
            self.sync_timer();
            return;
        }
        let next = self.policy.animation(Instant::now());
        let mut repaint = (next - self.transition).abs() > 0.001;
        self.transition = next;
        if self.is_running()
            && self.policy.screen == Screen::Controls
            && let Some(signals) = &self.signals
        {
            let to_db = |bits: u32| {
                let value = f32::from_bits(bits);
                if value > 0.000_031_6 {
                    (20.0 * value.log10()).round().clamp(-90.0, 0.0) as i32
                } else {
                    -90
                }
            };
            let levels = [
                to_db(signals.input_peak.load(Ordering::Relaxed)),
                to_db(signals.output_peak.load(Ordering::Relaxed)),
            ];
            if levels != self.levels {
                self.levels = levels;
                repaint = true;
                self.set_text(
                    ID_INPUT_DB,
                    &format!("{}  {} dBFS", self.text("input_level"), levels[0]),
                );
                self.set_text(
                    ID_OUTPUT_DB,
                    &format!("{}  {} dBFS", self.text("output_level"), levels[1]),
                );
            }
        }
        if matches!(self.update, UpdateState::Downloading) && self.policy.screen == Screen::Settings
        {
            let total = self.progress.total.load(Ordering::Relaxed);
            let percent = (self.progress.bytes.load(Ordering::Relaxed) * 100)
                .checked_div(total)
                .unwrap_or(0)
                .min(100);
            if percent != self.percent {
                self.percent = percent;
                self.set_text(ID_UPDATE_STATUS, &self.update_text());
            }
        }
        if repaint {
            // SAFETY: Invalidation only schedules a useful visible frame, never a hidden render.
            unsafe {
                InvalidateRect(self.hwnd, null(), 0);
            }
        }
        self.sync_timer();
    }

    fn show(&mut self, near_cursor: bool) {
        self.policy.visible = true;
        self.scroll = 0;
        // SAFETY: All window operations happen on its owning UI thread.
        unsafe {
            let mut point: POINT = zeroed();
            GetCursorPos(&mut point);
            let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
            let mut info: MONITORINFO = zeroed();
            info.cbSize = size_of::<MONITORINFO>() as u32;
            GetMonitorInfoW(monitor, &mut info);
            let width = self.px(460).min(info.rcWork.right - info.rcWork.left - 16);
            let height = self.px(704).min(info.rcWork.bottom - info.rcWork.top - 16);
            let (x, y) = if near_cursor {
                (point.x - width, point.y - height)
            } else {
                (
                    (info.rcWork.left + info.rcWork.right - width) / 2,
                    (info.rcWork.top + info.rcWork.bottom - height) / 2,
                )
            };
            let x = x.clamp(info.rcWork.left, info.rcWork.right - width);
            let y = y.clamp(info.rcWork.top, info.rcWork.bottom - height);
            SetWindowPos(
                self.hwnd,
                null_mut(),
                x,
                y,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            ShowWindow(self.hwnd, SW_RESTORE);
            SetForegroundWindow(self.hwnd);
            let mut rect: RECT = zeroed();
            GetClientRect(self.hwnd, &mut rect);
            self.client_height = rect.bottom;
        }
        self.rebuild();
        self.sync_timer();
    }

    fn hide(&mut self) {
        self.policy.visible = false;
        self.sync_timer();
        // SAFETY: Hiding does not destroy the window or the audio/control workers.
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn child(
        &mut self,
        class: &str,
        id: usize,
        text: &str,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        font: usize,
    ) -> HWND {
        // SAFETY: Class and title are NUL-terminated for creation; the parent/module are live.
        unsafe {
            let child = CreateWindowExW(
                0,
                wide(class).as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                self.px(x),
                self.px(y) - self.scroll,
                self.px(w),
                self.px(h),
                self.hwnd,
                id as HMENU,
                self.instance,
                null(),
            );
            if !child.is_null() {
                SendMessageW(child, WM_SETFONT, self.fonts[font] as usize, 0);
                self.children.push(child);
            }
            if self.settings.theme == Theme::Dark {
                SetWindowTheme(child, wide("DarkMode_Explorer").as_ptr(), null());
            }
            child
        }
    }
    fn label(&mut self, id: usize, text: &str, x: i32, y: i32, w: i32, h: i32, font: usize) {
        self.child("STATIC", id, text, SS_LEFT | SS_NOPREFIX, x, y, w, h, font);
    }
    fn button(&mut self, id: usize, key: &str, x: i32, y: i32, w: i32, enabled: bool) {
        let button = self.child(
            "BUTTON",
            id,
            self.text(key),
            BS_OWNERDRAW as u32 | WS_TABSTOP,
            x,
            y,
            w,
            36,
            0,
        );
        // SAFETY: The optional returned child belongs to this window.
        unsafe {
            EnableWindow(button, i32::from(enabled));
        }
    }
    fn checkbox(
        &mut self,
        id: usize,
        key: &str,
        x: i32,
        y: i32,
        w: i32,
        checked: bool,
        enabled: bool,
    ) {
        let control = self.child(
            "BUTTON",
            id,
            self.text(key),
            BS_AUTOCHECKBOX as u32 | BS_MULTILINE as u32 | WS_TABSTOP,
            x,
            y,
            w,
            36,
            0,
        );
        // SAFETY: The control is a standard checkbox; BM_SETCHECK does not call application code.
        unsafe {
            SendMessageW(control, BM_SETCHECK, usize::from(checked), 0);
            EnableWindow(control, i32::from(enabled));
        }
    }
    fn combo(
        &mut self,
        id: usize,
        items: &[String],
        selected: usize,
        x: i32,
        y: i32,
        w: i32,
        enabled: bool,
    ) {
        let control = self.child(
            "COMBOBOX",
            id,
            "",
            CBS_DROPDOWNLIST as u32 | CBS_HASSTRINGS as u32 | WS_TABSTOP | WS_VSCROLL,
            x,
            y,
            w,
            240,
            0,
        );
        // SAFETY: CB_ADDSTRING copies each string synchronously into the standard combo box.
        unsafe {
            for item in items {
                SendMessageW(control, CB_ADDSTRING, 0, wide(item).as_ptr() as isize);
            }
            SendMessageW(control, CB_SETCURSEL, selected, 0);
            SendMessageW(control, CB_SETITEMHEIGHT, usize::MAX, self.px(28) as isize);
            EnableWindow(control, i32::from(enabled));
        }
    }

    fn rebuild(&mut self) {
        if !self.policy.visible {
            self.dirty = true;
            return;
        }
        // SAFETY: Controls/fonts/brushes belong only to this UI thread. Destruction is bounded by
        // the small fixed screen; reentrant notifications cannot borrow APP while rebuilding.
        let focus_id = unsafe { GetDlgCtrlID(GetFocus()) };
        // SAFETY: Each HWND is an owned child; RefCell rejects synchronous reentrant app access.
        unsafe {
            for child in self.children.drain(..) {
                DestroyWindow(child);
            }
        }
        self.dirty = false;
        self.update_scrollbar();
        match self.policy.screen {
            Screen::Controls => self.controls(),
            Screen::Settings => self.preferences(),
            Screen::Setup => self.setup(),
            Screen::Licenses => self.licenses(),
        }
        if focus_id > 0 {
            // SAFETY: Restore keyboard focus only to an enabled control on the new screen.
            unsafe {
                let control = GetDlgItem(self.hwnd, focus_id);
                if !control.is_null() && IsWindowEnabled(control) != 0 {
                    SetFocus(control);
                }
            }
        }
        // SAFETY: Redraw is requested only after a visible screen change.
        unsafe {
            InvalidateRect(self.hwnd, null(), 0);
        }
    }

    fn controls(&mut self) {
        self.label(0, "Microphone Cleaner", 24, 18, 398, 34, 1);
        self.label(0, self.text("tagline"), 24, 62, 398, 30, 0);
        self.label(ID_STATUS, self.text(self.status_key()), 48, 107, 374, 32, 0);
        self.label(0, self.text("input"), 24, 151, 398, 24, 2);
        self.inputs = vec![None];
        let mut names = vec![self.text("select_input").to_owned()];
        for input in &self.catalog.inputs {
            self.inputs.push(Some(input.id.clone()));
            names.push(input.name.clone());
        }
        let selected = self.choice(true, &mut names);
        self.combo(
            ID_INPUT,
            &names,
            selected,
            24,
            177,
            398,
            !self.is_running() && !self.pending(),
        );
        self.label(0, self.text("virtual_microphone"), 24, 221, 398, 24, 2);
        self.outputs = vec![None];
        let mut names = vec![self.text("select_output").to_owned()];
        for route in &self.catalog.routes {
            self.outputs.push(Some(route.render.id.clone()));
            names.push(route.render.name.clone());
        }
        let selected = self.choice(false, &mut names);
        self.combo(
            ID_OUTPUT,
            &names,
            selected,
            24,
            247,
            398,
            !self.is_running() && !self.pending(),
        );
        let route = self
            .catalog
            .routes
            .iter()
            .find(|r| Some(&r.render.id) == self.settings.output_id.as_ref());
        let hint = match route {
            Some(route) => i18n::format(
                self.settings.language,
                "use_in_call",
                "name",
                &route.microphone.name,
            ),
            None => self.text("no_cable").to_owned(),
        };
        self.label(0, &hint, 24, 290, 398, 58, 2);
        self.button(ID_REFRESH, "refresh", 24, 356, 188, !self.pending());
        self.button(ID_SETUP, "setup", 224, 356, 198, true);
        self.checkbox(
            ID_CLEANUP,
            "cleanup",
            24,
            411,
            190,
            !self.bypass,
            !self.pending(),
        );
        self.label(0, self.text("strength"), 234, 398, 188, 22, 2);
        self.combo(
            ID_STRENGTH,
            &[self.text("natural").into(), self.text("strong").into()],
            usize::from(self.settings.strength == Strength::Strong),
            234,
            423,
            188,
            !self.pending(),
        );
        self.label(0, self.text("cleanup_help"), 24, 465, 398, 42, 2);
        if self.is_running() {
            self.label(
                ID_INPUT_DB,
                &format!("{}  {} dBFS", self.text("input_level"), self.levels[0]),
                24,
                517,
                192,
                24,
                2,
            );
            self.label(
                ID_OUTPUT_DB,
                &format!("{}  {} dBFS", self.text("output_level"), self.levels[1]),
                234,
                517,
                188,
                24,
                2,
            );
        } else {
            self.label(0, self.text("level_idle"), 24, 517, 398, 24, 2);
        }
        let can_start = self
            .catalog
            .inputs
            .iter()
            .any(|d| Some(&d.id) == self.settings.input_id.as_ref())
            && self
                .catalog
                .routes
                .iter()
                .any(|r| Some(&r.render.id) == self.settings.output_id.as_ref());
        self.button(
            ID_START,
            if self.is_running() { "stop" } else { "start" },
            24,
            568,
            398,
            !self.pending() && (self.is_running() || can_start),
        );
        self.button(ID_SETTINGS, "settings", 24, 620, 188, true);
        self.button(ID_HIDE, "hide", 224, 620, 198, true);
        if let Some(key) = self.notice {
            self.label(0, self.text(key), 24, 674, 398, 76, 2);
        } else if let Some(available) = &self.update_available {
            let message = i18n::format(
                self.settings.language,
                "available",
                "version",
                &available.version.to_string(),
            );
            self.label(0, &message, 24, 674, 398, 76, 2);
        }
    }

    fn choice(&mut self, input: bool, names: &mut Vec<String>) -> usize {
        let (choices, selected) = if input {
            (&mut self.inputs, &self.settings.input_id)
        } else {
            (&mut self.outputs, &self.settings.output_id)
        };
        if let Some(index) = choices.iter().position(|id| id == selected) {
            return index;
        }
        if selected.is_some() {
            choices.push(selected.clone());
            names.push(tr(self.settings.language, "unavailable").into());
            return choices.len() - 1;
        }
        0
    }

    fn preferences(&mut self) {
        self.button(ID_BACK, "back", 24, 18, 98, true);
        self.label(0, self.text("settings"), 140, 18, 282, 36, 1);
        let version = i18n::format(
            self.settings.language,
            "version",
            "version",
            updates::VERSION,
        );
        self.label(0, &version, 24, 76, 398, 28, 2);
        self.label(0, self.text("appearance"), 24, 120, 398, 30, 0);
        self.label(0, self.text("theme"), 24, 160, 188, 24, 2);
        self.label(0, self.text("language"), 234, 160, 188, 24, 2);
        self.combo(
            ID_THEME,
            &[self.text("light").into(), self.text("dark").into()],
            usize::from(self.settings.theme == Theme::Dark),
            24,
            187,
            188,
            true,
        );
        self.combo(
            ID_LANGUAGE,
            &i18n::LANGUAGE_NAMES.map(str::to_owned),
            i18n::index(self.settings.language),
            234,
            187,
            188,
            true,
        );
        self.checkbox(
            ID_MOTION,
            "reduced_motion",
            24,
            232,
            398,
            self.settings.reduced_motion,
            true,
        );
        self.label(0, self.text("updates"), 24, 294, 398, 30, 0);
        self.checkbox(
            ID_STARTUP,
            "startup_updates",
            24,
            330,
            398,
            self.settings.startup_update_check,
            true,
        );
        let busy = matches!(
            self.update,
            UpdateState::Checking | UpdateState::Downloading | UpdateState::Installing
        );
        self.button(ID_CHECK, "check_updates", 24, 378, 398, !busy);
        if self.update_available.is_some() && !matches!(self.update, UpdateState::Downloading) {
            self.button(ID_INSTALL, "download_install", 24, 426, 398, !busy);
        }
        if matches!(self.update, UpdateState::Downloading) {
            self.button(ID_CANCEL, "cancel", 24, 426, 398, true);
        }
        self.label(ID_UPDATE_STATUS, &self.update_text(), 24, 478, 398, 74, 2);
        self.label(0, self.text("about"), 24, 570, 398, 30, 0);
        self.label(0, self.text("engine_info"), 24, 610, 398, 104, 2);
        self.label(0, self.text("latency_note"), 24, 726, 398, 68, 2);
        self.button(ID_LICENSES, "licenses", 24, 812, 188, true);
        self.button(ID_SETUP, "setup", 224, 812, 198, true);
        self.label(0, self.text("audio_details"), 24, 872, 398, 30, 0);
        let detail = match &self.audio {
            AudioState::Running(info) => format!(
                "{} → {} Hz\n{}: {}\n{}\n{}",
                info.input_rate,
                info.output_rate,
                self.text("discontinuities"),
                self.signals
                    .as_ref()
                    .map(|s| s.discontinuities.load(Ordering::Relaxed))
                    .unwrap_or(0),
                info.input_name,
                info.call_microphone
            ),
            _ => self.text(self.status_key()).into(),
        };
        self.label(0, &detail, 24, 912, 398, 120, 2);
        if !self.detail.is_empty() {
            self.label(0, self.text("diagnostic"), 24, 1046, 398, 28, 0);
            self.label(0, &self.detail.clone(), 24, 1084, 398, 160, 2);
        }
    }

    fn setup(&mut self) {
        self.button(ID_BACK, "back", 24, 18, 98, true);
        self.label(0, self.text("setup"), 24, 84, 398, 76, 1);
        self.label(0, self.text("no_cable"), 24, 176, 398, 84, 0);
        self.label(0, self.text("setup_steps"), 24, 276, 398, 236, 2);
        self.button(ID_WEBSITE, "provider_site", 24, 526, 398, true);
        self.label(0, self.text("before_after"), 24, 590, 398, 128, 2);
        self.label(0, self.text("privacy"), 24, 736, 398, 90, 2);
    }

    fn licenses(&mut self) {
        self.button(ID_BACK, "back", 24, 18, 98, true);
        self.label(0, self.text("licenses"), 140, 18, 282, 62, 0);
        let height = (self.client_height * 96 / self.dpi as i32 - 104).max(120);
        let legal_text = format!(
            "{}\r\n\r\n{}",
            include_str!("../LICENSE").replace('\n', "\r\n"),
            include_str!("../THIRD_PARTY_NOTICES.txt").replace('\n', "\r\n")
        );
        self.child(
            "EDIT",
            0,
            &legal_text,
            WS_BORDER
                | WS_VSCROLL
                | ES_MULTILINE as u32
                | ES_READONLY as u32
                | ES_AUTOVSCROLL as u32
                | WS_TABSTOP,
            24,
            92,
            398,
            height,
            2,
        );
    }

    fn content_height(&self) -> i32 {
        match self.policy.screen {
            Screen::Controls => {
                if self.notice.is_some() || self.update_available.is_some() {
                    772
                } else {
                    676
                }
            }
            Screen::Settings => {
                if self.detail.is_empty() {
                    1052
                } else {
                    1264
                }
            }
            Screen::Setup => 848,
            Screen::Licenses => 0,
        }
    }

    fn update_scrollbar(&mut self) {
        let content = self.px(self.content_height());
        self.scroll = self.scroll.clamp(0, (content - self.client_height).max(0));
        // SAFETY: The scroll data describes this window's finite content range.
        unsafe {
            let info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: (content - 1).max(0),
                nPage: self.client_height.max(1) as u32,
                nPos: self.scroll,
                nTrackPos: 0,
            };
            SetScrollInfo(self.hwnd, SB_VERT, &info, 1);
        }
    }

    fn scroll_by(&mut self, pixels: i32) {
        let old = self.scroll;
        self.scroll = (self.scroll + pixels).clamp(
            0,
            (self.px(self.content_height()) - self.client_height).max(0),
        );
        if old == self.scroll {
            return;
        }
        // SAFETY: ScrollWindowEx moves the owned child controls without reconstructing their state
        // or focus, including keyboard-driven scrolls on long translated screens.
        unsafe {
            ScrollWindowEx(
                self.hwnd,
                0,
                old - self.scroll,
                null(),
                null(),
                null_mut(),
                null_mut(),
                SW_SCROLLCHILDREN | SW_INVALIDATE | SW_ERASE,
            );
        }
        self.update_scrollbar();
    }

    fn update_text(&self) -> String {
        match &self.update {
            UpdateState::Idle => String::new(),
            UpdateState::Checking => self.text("checking").into(),
            UpdateState::Current => self.text("current").into(),
            UpdateState::Available(available) => i18n::format(
                self.settings.language,
                "available",
                "version",
                &available.version.to_string(),
            ),
            UpdateState::Downloading => i18n::format(
                self.settings.language,
                "downloading",
                "percent",
                &self.percent.to_string(),
            ),
            UpdateState::Installing => self.text("installing").into(),
            UpdateState::Failed(error) => self
                .text(match error.kind {
                    updates::ErrorKind::Network => "update_network",
                    updates::ErrorKind::NoRelease => "update_no_release",
                    updates::ErrorKind::InvalidRelease => "update_invalid",
                    updates::ErrorKind::Incompatible => "update_incompatible",
                    updates::ErrorKind::Integrity => "update_integrity",
                    updates::ErrorKind::Storage => "update_storage",
                    updates::ErrorKind::Cancelled => "update_cancelled",
                    updates::ErrorKind::Install => "update_install",
                })
                .into(),
        }
    }

    fn reveal_keyboard_focus(&mut self) {
        if !self.policy.visible {
            return;
        }
        // SAFETY: Only an owned child can be scrolled into view. Convert its actual screen
        // rectangle to this window's client coordinates without changing focus or activation.
        unsafe {
            let focus = GetFocus();
            if IsChild(self.hwnd, focus) == 0 {
                return;
            }
            let mut rectangle: RECT = zeroed();
            if GetWindowRect(focus, &mut rectangle) == 0 {
                return;
            }
            MapWindowPoints(
                null_mut(),
                self.hwnd,
                &mut rectangle as *mut RECT as *mut POINT,
                2,
            );
            let margin = self.px(12);
            if rectangle.top < margin {
                self.scroll_by(rectangle.top - margin);
            } else if rectangle.bottom > self.client_height - margin {
                self.scroll_by(rectangle.bottom - self.client_height + margin);
            }
        }
    }

    fn start_check(&mut self) {
        if matches!(
            self.update,
            UpdateState::Checking | UpdateState::Downloading | UpdateState::Installing
        ) {
            return;
        }
        if let Some(worker) = self.update_worker.take() {
            let _ = worker.join();
        }
        self.update = UpdateState::Checking;
        let events = self.events.clone();
        match thread::Builder::new()
            .name("update-check".into())
            .spawn(move || events.send(Event::Checked(updates::check())))
        {
            Ok(worker) => self.update_worker = Some(worker),
            Err(error) => {
                self.update = UpdateState::Failed(UpdateError {
                    kind: updates::ErrorKind::Network,
                    detail: error.to_string(),
                })
            }
        }
        self.changed(false);
    }

    fn install(&mut self) {
        let Some(available) = self.update_available.clone() else {
            return;
        };
        if matches!(
            self.update,
            UpdateState::Checking | UpdateState::Downloading | UpdateState::Installing
        ) {
            return;
        }
        if let Some(worker) = self.update_worker.take() {
            let _ = worker.join();
        }
        self.progress = Arc::new(Progress::default());
        self.percent = 0;
        let progress = self.progress.clone();
        let events = self.events.clone();
        self.update = UpdateState::Downloading;
        match thread::Builder::new()
            .name("update-download".into())
            .spawn(move || {
                let result = std::env::current_exe()
                    .map_err(|e| UpdateError {
                        kind: updates::ErrorKind::Storage,
                        detail: e.to_string(),
                    })
                    .and_then(|exe| updates::stage(&available, &exe, &progress));
                events.send(Event::Staged(result));
            }) {
            Ok(worker) => self.update_worker = Some(worker),
            Err(error) => {
                self.update = UpdateState::Failed(UpdateError {
                    kind: updates::ErrorKind::Storage,
                    detail: error.to_string(),
                })
            }
        }
        self.changed(false);
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.receiver.try_recv() {
            match event {
                Event::Catalog(Ok(catalog)) => self.catalog = catalog,
                Event::Catalog(Err(error)) => {
                    self.notice = Some("device_refresh_error");
                    self.detail = error;
                }
                Event::Started(Ok((info, signals))) => {
                    self.audio = AudioState::Running(info);
                    self.signals = Some(signals);
                    self.levels = [-90; 2];
                }
                Event::Started(Err(error)) => {
                    self.audio = AudioState::Failed;
                    self.detail = error;
                }
                Event::Stopped => {
                    self.audio = AudioState::Stopped;
                    self.signals = None;
                    self.levels = [-90; 2];
                }
                Event::Lost => {
                    self.audio = AudioState::Lost;
                    self.signals = None;
                    self.levels = [-90; 2];
                }
                Event::Checked(result) => match result {
                    Ok(Check::Current) => {
                        self.update_available = None;
                        self.update = UpdateState::Current;
                    }
                    Ok(Check::Available(available)) => {
                        if self
                            .update_available
                            .as_ref()
                            .is_none_or(|current| current.version != available.version)
                        {
                            self.notify_update(&available);
                        }
                        self.update_available = Some(available.clone());
                        self.update = UpdateState::Available(available);
                    }
                    Err(error) => {
                        self.detail = error.to_string();
                        self.update = UpdateState::Failed(error);
                    }
                },
                Event::Staged(result) => match result {
                    Ok(plan) => {
                        if self.progress.cancel.load(Ordering::Acquire) {
                            updates::cleanup_staging(&plan);
                            self.update = UpdateState::Failed(UpdateError {
                                kind: updates::ErrorKind::Cancelled,
                                detail: "Cancelled before installation".into(),
                            });
                        } else {
                            if let Some(worker) = self.update_worker.take() {
                                let _ = worker.join();
                            }
                            self.update = UpdateState::Installing;
                            let events = self.events.clone();
                            match thread::Builder::new().name("update-install".into()).spawn(
                                move || {
                                    let result = updates::spawn_helper(&plan);
                                    if result.is_err() {
                                        updates::cleanup_staging(&plan);
                                    }
                                    events.send(Event::Armed(result));
                                },
                            ) {
                                Ok(worker) => self.update_worker = Some(worker),
                                Err(error) => {
                                    self.update = UpdateState::Failed(UpdateError {
                                        kind: updates::ErrorKind::Install,
                                        detail: error.to_string(),
                                    });
                                }
                            }
                        }
                    }
                    Err(error) => {
                        self.detail = error.to_string();
                        self.update = UpdateState::Failed(error);
                    }
                },
                Event::Armed(Ok(())) => {
                    self.quit();
                    return;
                }
                Event::Armed(Err(error)) => {
                    self.detail = error.to_string();
                    self.update = UpdateState::Failed(error);
                }
            }
        }
        self.changed(true);
    }

    fn selected(&self, id: usize) -> usize {
        // SAFETY: IDs identify standard combo boxes owned by this window.
        unsafe {
            SendMessageW(GetDlgItem(self.hwnd, id as i32), CB_GETCURSEL, 0, 0).max(0) as usize
        }
    }
    fn checked(&self, id: usize) -> bool {
        // SAFETY: IDs identify standard checkbox controls owned by this window.
        unsafe {
            SendMessageW(GetDlgItem(self.hwnd, id as i32), BM_GETCHECK, 0, 0)
                == BST_CHECKED as isize
        }
    }

    fn action(&mut self, id: usize, notification: u32) {
        if notification != BN_CLICKED && notification != CBN_SELCHANGE {
            return;
        }
        let combo = notification == CBN_SELCHANGE;
        match id {
            ID_INPUT if combo => {
                self.settings.input_id = self.inputs.get(self.selected(id)).cloned().flatten();
                self.save();
            }
            ID_OUTPUT if combo => {
                self.settings.output_id = self.outputs.get(self.selected(id)).cloned().flatten();
                self.save();
            }
            ID_STRENGTH if combo => {
                self.settings.strength = if self.selected(id) == 1 {
                    Strength::Strong
                } else {
                    Strength::Natural
                };
                if let Some(signals) = &self.signals {
                    signals.strong.store(
                        self.settings.strength == Strength::Strong,
                        Ordering::Relaxed,
                    );
                }
                self.save();
            }
            ID_THEME if combo => {
                self.settings.theme = if self.selected(id) == 1 {
                    Theme::Dark
                } else {
                    Theme::Light
                };
                self.reset_resources();
                self.save();
            }
            ID_LANGUAGE if combo => {
                self.settings.language = *i18n::LANGUAGES
                    .get(self.selected(id))
                    .unwrap_or(&Language::En);
                self.save();
            }
            ID_MOTION => {
                self.settings.reduced_motion = self.checked(id);
                self.save();
            }
            ID_STARTUP => {
                self.settings.startup_update_check = self.checked(id);
                self.save();
            }
            ID_CLEANUP => {
                self.bypass = !self.checked(id);
                if let Some(signals) = &self.signals {
                    signals.bypass.store(self.bypass, Ordering::Relaxed);
                }
            }
            ID_START if !self.pending() => {
                if self.is_running() {
                    if self.command(AudioCommand::Stop) {
                        self.audio = AudioState::Stopping;
                    }
                } else if let (Some(input), Some(output)) = (
                    self.settings.input_id.clone(),
                    self.settings.output_id.clone(),
                ) {
                    if self.command(AudioCommand::Start {
                        input,
                        output,
                        strength: self.settings.strength,
                        bypass: self.bypass,
                    }) {
                        self.audio = AudioState::Starting;
                        self.notice = None;
                    }
                } else {
                    self.notice = Some("select_first");
                }
            }
            ID_REFRESH => {
                self.command(AudioCommand::Refresh);
            }
            ID_SETTINGS => {
                self.policy.screen = Screen::Settings;
                self.scroll = 0;
            }
            ID_SETUP => {
                self.policy.screen = Screen::Setup;
                self.scroll = 0;
            }
            ID_LICENSES => {
                self.policy.screen = Screen::Licenses;
                self.scroll = 0;
            }
            ID_BACK => {
                self.policy.screen = if matches!(self.policy.screen, Screen::Licenses) {
                    Screen::Settings
                } else {
                    Screen::Controls
                };
                self.scroll = 0;
            }
            ID_HIDE | 2 => {
                self.hide();
                return;
            }
            ID_QUIT => {
                self.quit();
                return;
            }
            ID_CHECK => {
                self.start_check();
                return;
            }
            ID_INSTALL => {
                self.install();
                return;
            }
            ID_CANCEL => {
                self.progress.cancel.store(true, Ordering::Release);
            }
            ID_WEBSITE => {
                // SAFETY: Only an explicit click opens this fixed, publisher-owned HTTPS page.
                unsafe {
                    ShellExecuteW(
                        self.hwnd,
                        wide("open").as_ptr(),
                        wide(provider::PROVIDER_URL).as_ptr(),
                        null(),
                        null(),
                        SW_SHOWNORMAL,
                    );
                }
            }
            _ => return,
        }
        self.changed(true);
    }

    fn quit(&mut self) {
        self.progress.cancel.store(true, Ordering::Release);
        if let Some(signals) = &self.signals {
            signals.stop.store(true, Ordering::Release);
        }
        self.hide();
        // SAFETY: A normal UI-thread quit ends the message loop; Drop shuts down owned workers.
        unsafe {
            PostQuitMessage(0);
        }
    }

    fn reset_resources(&mut self) {
        // SAFETY: Resources are selected only for individual synchronous draw calls and restored
        // before this method. They are not shared across threads.
        unsafe {
            if !self.brush.is_null() {
                DeleteObject(self.brush);
            }
            self.brush = CreateSolidBrush(self.palette().background);
            for font in &mut self.fonts {
                if !font.is_null() {
                    DeleteObject(*font);
                }
            }
            self.fonts = [
                self.make_font(15, 400),
                self.make_font(24, 600),
                self.make_font(14, 400),
            ];
            let dark: i32 = i32::from(self.settings.theme == Theme::Dark);
            DwmSetWindowAttribute(
                self.hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                &dark as *const i32 as *const _,
                size_of::<i32>() as u32,
            );
        }
    }

    fn make_font(&self, pixels: i32, weight: i32) -> HFONT {
        // SAFETY: CreateFontW copies the NUL-terminated family name; the caller owns the font.
        unsafe {
            CreateFontW(
                -self.px(pixels),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                DEFAULT_PITCH as u32,
                wide("Segoe UI").as_ptr(),
            )
        }
    }

    fn update_tray(&self) {
        let mut title = format!("Microphone Cleaner · {}", self.text(self.status_key()));
        if let Some(available) = &self.update_available {
            title.push_str(&format!(" · v{}", available.version));
        }
        if *self.last_tray_tip.borrow() == title {
            return;
        }
        // SAFETY: NOTIFYICONDATAW and all copied strings live through Shell_NotifyIconW.
        unsafe {
            let mut data: NOTIFYICONDATAW = zeroed();
            data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            data.hWnd = self.hwnd;
            data.uID = 1;
            data.uFlags = NIF_TIP;
            for (destination, value) in data.szTip.iter_mut().take(127).zip(title.encode_utf16()) {
                *destination = value;
            }
            if Shell_NotifyIconW(NIM_MODIFY, &data) != 0 {
                *self.last_tray_tip.borrow_mut() = title;
            }
        }
    }

    fn add_tray(&self) -> bool {
        // SAFETY: The icon and callback window remain alive until NIM_DELETE in Drop.
        unsafe {
            let mut data: NOTIFYICONDATAW = zeroed();
            data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            data.hWnd = self.hwnd;
            data.uID = 1;
            data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            data.uCallbackMessage = TRAY;
            data.hIcon = self.icon;
            let added = Shell_NotifyIconW(NIM_ADD, &data) != 0;
            self.last_tray_tip.borrow_mut().clear();
            self.update_tray();
            added
        }
    }

    fn notify_update(&self, available: &updates::Available) {
        let message = i18n::format(
            self.settings.language,
            "available",
            "version",
            &available.version.to_string(),
        );
        // SAFETY: This silent, non-modal notification is emitted only for a newly found update.
        unsafe {
            let mut data: NOTIFYICONDATAW = zeroed();
            data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            data.hWnd = self.hwnd;
            data.uID = 1;
            data.uFlags = NIF_INFO;
            data.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
            for (destination, value) in data
                .szInfoTitle
                .iter_mut()
                .take(63)
                .zip("Microphone Cleaner".encode_utf16())
            {
                *destination = value;
            }
            for (destination, value) in data.szInfo.iter_mut().take(255).zip(message.encode_utf16())
            {
                *destination = value;
            }
            Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }

    fn tray_menu(&mut self) {
        // SAFETY: This finite native context menu is created/destroyed on the UI thread. String
        // arguments are copied by AppendMenuW. TPM_RETURNCMD avoids reentrant command handling.
        unsafe {
            let menu = CreatePopupMenu();
            AppendMenuW(
                menu,
                MF_STRING,
                RESTORE as usize,
                wide(self.text("open")).as_ptr(),
            );
            AppendMenuW(
                menu,
                MF_STRING,
                ID_SETTINGS,
                wide(self.text("settings")).as_ptr(),
            );
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(menu, MF_STRING, ID_QUIT, wide(self.text("quit")).as_ptr());
            let mut point = zeroed();
            GetCursorPos(&mut point);
            SetForegroundWindow(self.hwnd);
            let selected = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                point.x,
                point.y,
                0,
                self.hwnd,
                null(),
            );
            DestroyMenu(menu);
            if selected as u32 == RESTORE {
                self.policy.screen = Screen::Controls;
                self.show(true);
            } else if selected as usize == ID_SETTINGS {
                self.policy.screen = Screen::Settings;
                self.show(true);
            } else if selected as usize == ID_QUIT {
                self.quit();
            }
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
        }
    }

    fn paint(&mut self) {
        // SAFETY: BeginPaint/EndPaint are paired on WM_PAINT. GDI objects are selected only into
        // this temporary memory DC and restored/deleted before returning.
        unsafe {
            let mut paint: PAINTSTRUCT = zeroed();
            let dc = BeginPaint(self.hwnd, &mut paint);
            if !self.policy.visible {
                EndPaint(self.hwnd, &paint);
                return;
            }
            let mut client: RECT = zeroed();
            GetClientRect(self.hwnd, &mut client);
            let memory = CreateCompatibleDC(dc);
            let bitmap = CreateCompatibleBitmap(dc, client.right.max(1), client.bottom.max(1));
            let old = SelectObject(memory, bitmap);
            FillRect(memory, &client, self.brush);
            let colors = self.palette();
            if self.policy.screen == Screen::Controls {
                let color = match self.audio {
                    AudioState::Running(_) if !self.bypass => colors.accent,
                    AudioState::Failed | AudioState::Lost => colors.warning,
                    _ => colors.muted,
                };
                let brush = CreateSolidBrush(color);
                let previous = SelectObject(memory, brush);
                let pen = SelectObject(memory, GetStockObject(NULL_PEN));
                Ellipse(
                    memory,
                    self.px(25),
                    self.px(115) - self.scroll,
                    self.px(35),
                    self.px(125) - self.scroll,
                );
                SelectObject(memory, previous);
                SelectObject(memory, pen);
                DeleteObject(brush);
                if self.is_running() {
                    for (index, left) in [24, 234].into_iter().enumerate() {
                        let top = self.px(547) - self.scroll;
                        let rect = RECT {
                            left: self.px(left),
                            top,
                            right: self.px(left + 188),
                            bottom: top + self.px(5),
                        };
                        let brush = CreateSolidBrush(colors.border);
                        FillRect(memory, &rect, brush);
                        DeleteObject(brush);
                        let width = (188.0
                            * ((self.levels[index] + 60) as f32 / 60.0).clamp(0.0, 1.0))
                            as i32;
                        let rect = RECT {
                            right: self.px(left + width),
                            ..rect
                        };
                        let brush = CreateSolidBrush(colors.accent);
                        FillRect(memory, &rect, brush);
                        DeleteObject(brush);
                    }
                }
            }
            if self.transition > 0.0 {
                let rect = RECT {
                    left: 0,
                    top: 0,
                    right: (client.right as f32 * self.transition) as i32,
                    bottom: self.px(3),
                };
                let brush = CreateSolidBrush(colors.accent);
                FillRect(memory, &rect, brush);
                DeleteObject(brush);
            }
            BitBlt(
                dc,
                paint.rcPaint.left,
                paint.rcPaint.top,
                paint.rcPaint.right - paint.rcPaint.left,
                paint.rcPaint.bottom - paint.rcPaint.top,
                memory,
                paint.rcPaint.left,
                paint.rcPaint.top,
                SRCCOPY,
            );
            SelectObject(memory, old);
            DeleteObject(bitmap);
            DeleteDC(memory);
            EndPaint(self.hwnd, &paint);
            self.policy.raster_count += 1;
        }
    }

    fn draw_button(&self, item: &DRAWITEMSTRUCT) {
        // SAFETY: WM_DRAWITEM supplies a live button HDC and rectangle. All temporary selections
        // are restored; GetWindowTextW writes into a fixed capacity UTF-16 buffer.
        unsafe {
            let colors = self.palette();
            let primary = item.CtlID as usize == ID_START || item.CtlID as usize == ID_INSTALL;
            let disabled = item.itemState & ODS_DISABLED != 0;
            let pressed = item.itemState & ODS_SELECTED != 0;
            let fill = if primary && !disabled {
                colors.accent
            } else if pressed {
                colors.border
            } else {
                colors.surface
            };
            let brush = CreateSolidBrush(fill);
            FillRect(item.hDC, &item.rcItem, brush);
            DeleteObject(brush);
            let border = CreateSolidBrush(colors.border);
            FrameRect(item.hDC, &item.rcItem, border);
            DeleteObject(border);
            SetBkMode(item.hDC, TRANSPARENT as i32);
            SetTextColor(
                item.hDC,
                if disabled {
                    colors.muted
                } else if primary {
                    colors.accent_text
                } else {
                    colors.text
                },
            );
            let previous = SelectObject(item.hDC, self.fonts[0]);
            let mut text = [0u16; 512];
            let length = GetWindowTextW(item.hwndItem, text.as_mut_ptr(), text.len() as i32);
            let mut rect = item.rcItem;
            rect.left += self.px(7);
            rect.right -= self.px(7);
            let mut measure = rect;
            DrawTextW(
                item.hDC,
                text.as_ptr(),
                length,
                &mut measure,
                DT_CENTER | DT_WORDBREAK | DT_CALCRECT | DT_NOPREFIX,
            );
            rect.top += ((rect.bottom - rect.top - (measure.bottom - measure.top)) / 2).max(0);
            DrawTextW(
                item.hDC,
                text.as_ptr(),
                length,
                &mut rect,
                DT_CENTER | DT_WORDBREAK | DT_NOPREFIX,
            );
            if item.itemState & ODS_FOCUS != 0 {
                InflateRect(&mut rect, -self.px(3), -self.px(4));
                DrawFocusRect(item.hDC, &rect);
            }
            SelectObject(item.hDC, previous);
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.progress.cancel.store(true, Ordering::Release);
        if let Some(signals) = &self.signals {
            signals.stop.store(true, Ordering::Release);
        }
        let _ = self.commands.send(AudioCommand::Quit);
        if let Some(controller) = self.controller.take() {
            let _ = controller.join();
        }
        // Update requests have a finite network timeout. A cancelled download never installs.
        // Do not block window shutdown for a background check; it owns no UI or audio resources.
        self.update_worker.take();
        // SAFETY: This is the UI-thread teardown of exclusively owned native resources.
        unsafe {
            KillTimer(self.hwnd, PULSE);
            let mut data: NOTIFYICONDATAW = zeroed();
            data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            data.hWnd = self.hwnd;
            data.uID = 1;
            Shell_NotifyIconW(NIM_DELETE, &data);
            DestroyWindow(self.hwnd);
            for font in self.fonts {
                DeleteObject(font);
            }
            DeleteObject(self.brush);
            DestroyIcon(self.icon);
            DestroyIcon(self.large_icon);
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: The OS invokes this callback with a valid window/message. Pointer-bearing messages
    // are interpreted only for their documented message IDs. RefCell rejects reentrant mutable use.
    unsafe {
        APP.with(|cell| {
            let Ok(mut slot) = cell.try_borrow_mut() else {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            };
            let Some(app) = slot.as_mut() else {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            };
            if message == app.taskbar_created {
                app.add_tray();
                return 0;
            }
            match message {
                WM_CLOSE => {
                    app.hide();
                    0
                }
                WM_SYSCOMMAND if wparam as u32 & 0xFFF0 == SC_MINIMIZE => {
                    app.hide();
                    0
                }
                WAKE => {
                    app.drain_events();
                    0
                }
                RESTORE => {
                    app.show(false);
                    0
                }
                TRAY => {
                    match lparam as u32 {
                        WM_LBUTTONUP => {
                            app.policy.screen = Screen::Controls;
                            if app.policy.visible {
                                app.hide();
                            } else {
                                app.show(true);
                            }
                        }
                        WM_RBUTTONUP | WM_CONTEXTMENU => app.tray_menu(),
                        _ => {}
                    }
                    0
                }
                WM_COMMAND => {
                    app.action(wparam & 0xffff, (wparam >> 16) as u32);
                    0
                }
                WM_TIMER if wparam == PULSE => {
                    app.tick();
                    0
                }
                WM_PAINT => {
                    app.paint();
                    0
                }
                WM_ERASEBKGND => 1,
                WM_DRAWITEM if lparam != 0 => {
                    app.draw_button(&*(lparam as *const DRAWITEMSTRUCT));
                    1
                }
                WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN => {
                    let dc = wparam as HDC;
                    SetTextColor(dc, app.palette().text);
                    SetBkColor(dc, app.palette().background);
                    SetBkMode(dc, TRANSPARENT as i32);
                    app.brush as LRESULT
                }
                WM_SIZE => {
                    if wparam as u32 == SIZE_MINIMIZED {
                        app.hide();
                    } else {
                        let mut client = zeroed();
                        GetClientRect(hwnd, &mut client);
                        app.client_height = client.bottom;
                        if app.policy.visible {
                            app.rebuild();
                        } else {
                            app.dirty = true;
                        }
                    }
                    0
                }
                WM_DPICHANGED => {
                    app.dpi = (wparam & 0xffff) as u32;
                    app.reset_resources();
                    let rect = &*(lparam as *const RECT);
                    SetWindowPos(
                        hwnd,
                        null_mut(),
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    app.changed(false);
                    0
                }
                WM_VSCROLL => {
                    let mut info: SCROLLINFO = zeroed();
                    info.cbSize = size_of::<SCROLLINFO>() as u32;
                    info.fMask = SIF_ALL;
                    GetScrollInfo(hwnd, SB_VERT, &mut info);
                    let delta = match (wparam & 0xffff) as i32 {
                        SB_LINEUP => -app.px(32),
                        SB_LINEDOWN => app.px(32),
                        SB_PAGEUP => -app.client_height,
                        SB_PAGEDOWN => app.client_height,
                        SB_THUMBTRACK | SB_THUMBPOSITION => info.nTrackPos - app.scroll,
                        SB_TOP => -app.scroll,
                        SB_BOTTOM => app.px(app.content_height()) - app.scroll,
                        _ => 0,
                    };
                    app.scroll_by(delta);
                    0
                }
                WM_MOUSEWHEEL => {
                    let delta = ((wparam >> 16) as u16 as i16) as i32;
                    app.scroll_by(-delta * app.px(48) / 120);
                    0
                }
                WM_KEYDOWN if wparam as u32 == VK_ESCAPE as u32 => {
                    app.hide();
                    0
                }
                _ => DefWindowProcW(hwnd, message, wparam, lparam),
            }
        })
    }
}

pub fn run() -> Result<(), String> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.first().is_some_and(|s| s == "--apply-update") {
        let path = arguments.get(1).ok_or("Missing update plan")?;
        return updates::run_helper(&PathBuf::from(path)).map_err(|e| e.to_string());
    }
    let acknowledgement = if arguments.first().is_some_and(|s| s == "--update-ack") {
        arguments.get(1).map(PathBuf::from)
    } else {
        None
    };
    let background = arguments.iter().any(|s| s == "--background") || acknowledgement.is_some();
    let path = config::settings_path()?;
    let loaded = Settings::load(&path);
    let load_error = loaded.as_ref().err().cloned();
    let settings = loaded.unwrap_or_default();
    // SAFETY: This initializes the UI thread's native resources. No audio stream is opened here.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let mutex = CreateMutexW(
            null(),
            0,
            wide("Local\\MicrophoneCleaner.SingleInstance.v1").as_ptr(),
        );
        if mutex.is_null() {
            return Err("Cannot create the application instance guard".into());
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let existing = FindWindowW(wide(CLASS).as_ptr(), null());
            if !existing.is_null() {
                let mut pid = 0;
                GetWindowThreadProcessId(existing, &mut pid);
                AllowSetForegroundWindow(pid);
                PostMessageW(existing, RESTORE, 0, 0);
            }
            CloseHandle(mutex);
            return Ok(());
        }
        let result = (|| {
            let instance = GetModuleHandleW(null());
            let class = wide(CLASS);
            let registered = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                ..zeroed()
            };
            if RegisterClassExW(&registered) == 0 {
                return Err("Cannot register the application window".into());
            }
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_CONTROLPARENT,
                class.as_ptr(),
                wide("Microphone Cleaner").as_ptr(),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VSCROLL | WS_CLIPCHILDREN,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                460,
                704,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            if hwnd.is_null() {
                return Err("Cannot create the application window".into());
            }
            let (sender, receiver) = mpsc::sync_channel(32);
            let events = Events {
                sender,
                hwnd: hwnd as isize,
            };
            let (commands, controller) = audio_controller(events.clone())?;
            let mut icon = null_mut();
            // MAKEINTRESOURCEW(1): the API interprets a low pointer value as an integer resource ID.
            let resource_id = std::ptr::without_provenance::<u16>(1);
            let icon_result = LoadIconMetric(instance, resource_id, LIM_SMALL, &mut icon);
            if icon_result < 0 || icon.is_null() {
                DestroyWindow(hwnd);
                return Err("The embedded application icon could not be loaded".into());
            }
            let mut large_icon = null_mut();
            LoadIconMetric(instance, resource_id, LIM_LARGE, &mut large_icon);
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, icon as isize);
            SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, large_icon as isize);
            let startup_update_check = settings.startup_update_check;
            let mut app = App {
                hwnd,
                instance,
                settings,
                settings_path: path,
                catalog: Catalog::default(),
                audio: AudioState::Stopped,
                signals: None,
                bypass: false,
                policy: RenderPolicy::default(),
                events,
                receiver,
                commands,
                controller: Some(controller),
                update_worker: None,
                update: UpdateState::Idle,
                progress: Arc::new(Progress::default()),
                update_available: None,
                children: Vec::new(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                brush: null_mut(),
                fonts: [null_mut(); 3],
                icon,
                large_icon,
                last_tray_tip: RefCell::new(String::new()),
                dpi: GetDpiForWindow(hwnd).max(96),
                scroll: 0,
                client_height: 660,
                timer: None,
                dirty: true,
                notice: load_error.as_ref().map(|_| "load_error"),
                detail: load_error.unwrap_or_default(),
                levels: [-90; 2],
                percent: 0,
                transition: 0.0,
                taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            };
            app.reset_resources();
            if !app.add_tray() {
                return Err(
                    "The notification area is unavailable; the app did not start hidden".into(),
                );
            }
            app.command(AudioCommand::Refresh);
            if !background {
                app.show(false);
            }
            if startup_update_check {
                app.start_check();
            }
            APP.with(|cell| *cell.borrow_mut() = Some(app));
            if let Some(path) = acknowledgement {
                updates::acknowledge(&path).map_err(|e| e.to_string())?;
            }
            let mut message: MSG = zeroed();
            loop {
                let read = GetMessageW(&mut message, null_mut(), 0, 0);
                if read <= 0 {
                    break;
                }
                if IsDialogMessageW(hwnd, &message) == 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                APP.with(|cell| {
                    if let Ok(mut slot) = cell.try_borrow_mut()
                        && let Some(app) = slot.as_mut()
                    {
                        app.reveal_keyboard_focus();
                    }
                });
            }
            APP.with(|cell| {
                cell.borrow_mut().take();
            });
            Ok(())
        })();
        APP.with(|cell| {
            cell.borrow_mut().take();
        });
        CloseHandle(mutex);
        result
    }
}

pub fn show_startup_error(error: &str) {
    // SAFETY: The message box copies these strings for the duration of this synchronous call.
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(error).as_ptr(),
            wide("Microphone Cleaner").as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}
