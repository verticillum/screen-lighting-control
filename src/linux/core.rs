//! What SLC is doing and what each display should show. No I/O: the daemon feeds in requests and the
//! time, and applies `outputs()`.

use super::cli::{Change, Request, DEEP_DIM};
use crate::cities;
use crate::color;
use crate::config::Ini;
use crate::engine::{self, MAX_BRIGHTNESS, MIN_BRIGHTNESS};
use crate::model::{MonitorSettings, Settings};
use crate::schedule::{self, Phase, Target};

/// `slc panic` pauses for this long.
const PANIC_MINUTES: u64 = 60;

/// A moment, as the schedule needs it.
#[derive(Clone, Copy, Debug)]
pub struct Now {
    /// Seconds since the Unix epoch.
    pub unix: u64,
    pub year: i32,
    pub day_of_year: u32,
    /// Minutes after local midnight.
    pub minute: f64,
    /// Local − UTC, minutes.
    pub utc_offset: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Pause {
    No,
    Until(u64),
    UntilResumed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    /// Connector, e.g. `DP-2`.
    pub name: String,
    /// Make, model and serial: follows the display when it changes ports.
    pub description: String,
    pub brightness: f32,
    pub enabled: bool,
    pub hw_share: f32,
    /// The real backlight can be controlled; `None` until that is known.
    pub has_hardware: Option<bool>,
    /// Whether the compositor lets us set this display's gamma.
    pub has_gamma: bool,
}

impl Display {
    /// Section name in the state file.
    fn key(&self) -> String {
        let id = if self.description.is_empty() { &self.name } else { &self.description };
        id.replace(['[', ']', '\r', '\n'], "_")
    }
}

/// What one display should show.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Output {
    pub kelvin: u32,
    /// Gamma dimming factor (1 = none).
    pub software: f32,
    /// Backlight level 0–100; `None` = leave it alone.
    pub hardware: Option<f32>,
}

pub struct Core {
    settings: Settings,
    displays: Vec<Display>,
    /// Manual warmth, until the schedule's phase changes.
    manual: Option<(u32, Phase)>,
    /// Manual warmth when there is no schedule.
    kelvin: u32,
    pause: Pause,
    target: Target,
    /// Remembered brightness of displays seen so far, and warmth.
    saved: Ini,
}

/// Reads settings from the config file, filling in the location from `city`.
pub fn settings(ini: &Ini) -> Settings {
    let mut s = Settings::from_ini(ini);
    if !s.schedule.has_location() && !s.schedule.city.is_empty() {
        if let Some(c) = cities::search(&s.schedule.city, 1).first() {
            (s.schedule.lat, s.schedule.lon) = (c.lat, c.lon);
        }
    }
    s
}

fn target(s: &Settings, now: &Now) -> Target {
    let e = schedule::events(&s.schedule, now.year, now.day_of_year, now.utc_offset);
    schedule::target_at(&s.schedule, &e, now.minute)
}

impl Core {
    pub fn new(settings: Settings, saved: Ini, now: &Now) -> Core {
        let kelvin = saved.get_parse("warmth", "kelvin").map(color::clamp_kelvin).unwrap_or(settings.kelvin);
        Core {
            target: target(&settings, now),
            settings,
            displays: Vec::new(),
            manual: None,
            kelvin,
            pause: Pause::No,
            saved,
        }
    }

    pub fn set_settings(&mut self, s: Settings, now: &Now) {
        self.settings = s;
        for i in 0..self.displays.len() {
            let (enabled, hw_share) = self.configured(&self.displays[i].name, &self.displays[i].description);
            self.displays[i].enabled = enabled;
            self.displays[i].hw_share = hw_share;
        }
        self.tick(now);
    }

    /// `[monitor.<connector or description>]` from the config.
    fn configured(&self, name: &str, description: &str) -> (bool, f32) {
        let d = MonitorSettings::default();
        let m = self.settings.monitor(name).or_else(|| self.settings.monitor(description));
        m.map_or((d.enabled, d.hw_share), |m| (m.enabled, m.hw_share))
    }

    pub fn displays(&self) -> &[Display] {
        &self.displays
    }

    pub fn add_display(&mut self, name: &str, description: &str) {
        if self.displays.iter().any(|d| d.name == name) {
            return;
        }
        // Some compositors append the connector, which changes with the port.
        let description = description.strip_suffix(&format!(" ({name})")).unwrap_or(description);
        let (enabled, hw_share) = self.configured(name, description);
        let mut d = Display {
            name: name.into(),
            description: description.into(),
            brightness: MAX_BRIGHTNESS,
            enabled,
            hw_share,
            has_hardware: None,
            has_gamma: false,
        };
        if let Some(b) = self.saved.get_parse::<f32>(&format!("display.{}", d.key()), "brightness") {
            d.brightness =
                if b.is_finite() { b.clamp(MIN_BRIGHTNESS, MAX_BRIGHTNESS) } else { MAX_BRIGHTNESS };
        }
        self.displays.push(d);
        self.displays.sort_by(|a, b| a.name.cmp(&b.name));
    }

    pub fn remove_display(&mut self, name: &str) {
        self.displays.retain(|d| d.name != name);
    }

    pub fn set_gamma(&mut self, name: &str, works: bool) {
        if let Some(d) = self.displays.iter_mut().find(|d| d.name == name) {
            d.has_gamma = works;
        }
    }

    /// A display's backlight was found at `level` (0–100), or can't be controlled. The first time a display
    /// is seen, its current level becomes its brightness, so starting SLC doesn't change the screen.
    pub fn set_hardware(&mut self, name: &str, level: Option<f32>) {
        let saved = &self.saved;
        if let Some(d) = self.displays.iter_mut().find(|d| d.name == name) {
            d.has_hardware = Some(level.is_some());
            let known = saved.get(&format!("display.{}", d.key()), "brightness").is_some();
            if let (Some(l), false, true) = (level, known, d.hw_share > 0.0) {
                d.brightness = (MAX_BRIGHTNESS - d.hw_share + l * d.hw_share / 100.0).max(MIN_BRIGHTNESS);
            }
        }
    }

    pub fn tick(&mut self, now: &Now) {
        self.target = target(&self.settings, now);
        if self.manual.is_some_and(|(_, phase)| !self.settings.schedule.enabled || phase != self.target.phase)
        {
            self.manual = None;
        }
        if let Pause::Until(t) = self.pause {
            if now.unix >= t {
                self.pause = Pause::No;
            }
        }
    }

    /// Seconds until the pause ends by itself.
    pub fn pause_left(&self, now: &Now) -> Option<u64> {
        match self.pause {
            Pause::Until(t) => Some(t.saturating_sub(now.unix)),
            _ => None,
        }
    }

    pub fn scheduled(&self) -> bool {
        self.settings.schedule.enabled
    }

    fn kelvin(&self) -> u32 {
        match self.manual {
            _ if !self.scheduled() => self.kelvin,
            Some((k, _)) => k,
            None => self.target.kelvin,
        }
    }

    fn set_kelvin(&mut self, k: f32) {
        let k = color::clamp_kelvin(k.round() as i64);
        if self.scheduled() {
            self.manual = Some((k, self.target.phase));
        } else {
            self.kelvin = k;
        }
    }

    /// Brightness after the schedule's night ceiling.
    fn effective(&self, b: f32) -> f32 {
        match self.settings.schedule.night_brightness {
            Some(nb) if self.scheduled() => b.min(MAX_BRIGHTNESS + (nb - MAX_BRIGHTNESS) * self.target.night),
            _ => b,
        }
    }

    pub fn output(&self, d: &Display) -> Output {
        let neutral = Output { kelvin: color::NEUTRAL_KELVIN, software: 1.0, hardware: None };
        if !d.enabled {
            return neutral;
        }
        // While unknown, assume there is a backlight: too bright for a moment beats a dark flash.
        let mut s = engine::split(self.effective(d.brightness), d.hw_share, d.has_hardware != Some(false));
        if d.has_hardware.is_none() {
            s.hardware = None;
        }
        if self.pause != Pause::No {
            // Pausing leaves the backlight where it is (panic has already set full brightness).
            return Output { hardware: s.hardware, ..neutral };
        }
        Output { kelvin: self.kelvin(), software: s.software, hardware: s.hardware }
    }

    /// Displays matching a number, connector, or part of a description.
    fn find(&self, sel: &str) -> Result<usize, String> {
        if let Ok(n) = sel.parse::<usize>() {
            return n
                .checked_sub(1)
                .filter(|i| *i < self.displays.len())
                .ok_or_else(|| format!("there is no display {n}"));
        }
        if let Some(i) = self.displays.iter().position(|d| d.name.eq_ignore_ascii_case(sel)) {
            return Ok(i);
        }
        let l = sel.to_lowercase();
        let hits: Vec<usize> = (0..self.displays.len())
            .filter(|&i| self.displays[i].description.to_lowercase().contains(&l))
            .collect();
        match hits[..] {
            [i] => Ok(i),
            [] => Err(format!("no display matches {sel}")),
            _ => Err(format!("{sel} matches more than one display")),
        }
    }

    /// Applies a request that changes state; returns an error message for the user.
    /// `Status`, `Watch`, `Reload` and `Quit` are the daemon's.
    pub fn handle(&mut self, r: &Request, now: &Now) -> Result<(), String> {
        match r {
            Request::Brightness { change, display, force } => {
                let only = display.as_deref().map(|s| self.find(s)).transpose()?;
                let floor = if *force { MIN_BRIGHTNESS } else { DEEP_DIM };
                for (i, d) in self.displays.iter_mut().enumerate() {
                    if only.is_none_or(|o| o == i) {
                        let b = match change {
                            Change::To(v) => *v,
                            // Steps stop at the floor, and never jump up to it.
                            Change::By(v) if *v < 0.0 => (d.brightness + v).max(floor.min(d.brightness)),
                            Change::By(v) => d.brightness + v,
                        };
                        d.brightness = b.clamp(MIN_BRIGHTNESS, MAX_BRIGHTNESS);
                    }
                }
                self.pause = Pause::No;
            }
            Request::Warmth(c) => {
                let k = match c {
                    Change::To(v) => *v,
                    Change::By(v) => self.kelvin() as f32 + v,
                };
                self.set_kelvin(k);
                self.pause = Pause::No;
            }
            Request::Auto => {
                if !self.scheduled() {
                    return Err("the schedule is off in the config file".into());
                }
                self.manual = None;
                self.pause = Pause::No;
            }
            Request::Pause(m) => {
                self.pause = m.map_or(Pause::UntilResumed, |m| Pause::Until(now.unix + m * 60));
            }
            Request::Resume => self.pause = Pause::No,
            Request::Toggle => {
                self.pause = if self.pause == Pause::No { Pause::UntilResumed } else { Pause::No };
            }
            Request::Panic => {
                self.displays.iter_mut().for_each(|d| d.brightness = MAX_BRIGHTNESS);
                self.set_kelvin(color::NEUTRAL_KELVIN as f32);
                self.pause = Pause::Until(now.unix + PANIC_MINUTES * 60);
            }
            Request::Status { .. } | Request::Watch | Request::Reload | Request::Quit => {}
        }
        Ok(())
    }

    /// The state file's contents.
    pub fn state(&mut self) -> &Ini {
        self.saved.set("warmth", "kelvin", self.kelvin);
        // Until the probe, brightness is only a placeholder.
        for d in self.displays.iter().filter(|d| d.has_hardware.is_some()) {
            self.saved.set(&format!("display.{}", d.key()), "brightness", format!("{:.1}", d.brightness));
        }
        &self.saved
    }

    /// Average over enabled displays.
    fn brightness(&self) -> u32 {
        let on: Vec<f32> = self.displays.iter().filter(|d| d.enabled).map(|d| d.brightness).collect();
        if on.is_empty() {
            return 100;
        }
        (on.iter().sum::<f32>() / on.len() as f32).round() as u32
    }

    fn mode(&self) -> &'static str {
        match self.manual {
            _ if self.pause != Pause::No => "paused",
            _ if !self.scheduled() => "manual",
            Some(_) => "override",
            None => "schedule",
        }
    }

    fn phase(&self) -> &'static str {
        match self.target.phase {
            Phase::Day => "day",
            Phase::Evening => "evening",
            Phase::Night => "night",
        }
    }

    pub fn status_text(&self, now: &Now) -> String {
        let mut out = format!("{}%  {}K  {}", self.brightness(), self.kelvin(), self.mode());
        if let Some(s) = self.pause_left(now) {
            out.push_str(&format!(" ({} min left)", s.div_ceil(60)));
        }
        if self.scheduled() {
            out.push_str(&format!("  {}", self.phase()));
            if !self.settings.schedule.has_location()
                && self.settings.schedule.mode != crate::model::ScheduleMode::Fixed
            {
                out.push_str(" (fixed times: no city in the config)");
            }
        }
        out.push('\n');
        for (i, d) in self.displays.iter().enumerate() {
            let how = match (d.enabled, d.has_hardware, d.has_gamma) {
                (false, ..) => "off in config",
                (_, Some(true), true) => "backlight + gamma",
                (_, Some(true), false) => "backlight only",
                (_, _, true) => "gamma",
                (_, _, false) => "no control",
            };
            out.push_str(&format!("{}  {}  {:.0}%  {how}  {}\n", i + 1, d.name, d.brightness, d.description));
        }
        out
    }

    /// One line of JSON. Values change only when the state does, so `watch` can compare lines.
    pub fn status_json(&self) -> String {
        let displays: Vec<String> = self
            .displays
            .iter()
            .map(|d| {
                format!(
                    "{{\"name\":{},\"description\":{},\"brightness\":{:.0},\"enabled\":{},\"backlight\":{},\"gamma\":{}}}",
                    json(&d.name), json(&d.description), d.brightness, d.enabled, d.has_hardware == Some(true), d.has_gamma
                )
            })
            .collect();
        let until = match self.pause {
            Pause::Until(t) => t.to_string(),
            _ => "null".into(),
        };
        format!(
            "{{\"brightness\":{},\"kelvin\":{},\"mode\":\"{}\",\"paused_until\":{until},\"phase\":\"{}\",\"displays\":[{}]}}",
            self.brightness(),
            self.kelvin(),
            self.mode(),
            self.phase(),
            displays.join(",")
        )
    }
}

fn json(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' | '\\' => out.extend(['\\', c]),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: f64) -> Now {
        Now { unix: 1_000_000, year: 2026, day_of_year: 180, minute: hour * 60.0, utc_offset: 0.0 }
    }

    const FIXED: &str = "[schedule]\nmode=fixed\nfixed_day=07:00\nfixed_evening=19:00\n";

    fn core(config: &str, hour: f64) -> Core {
        let mut c = Core::new(settings(&Ini::parse(config)), Ini::default(), &at(hour));
        c.add_display("DP-3", "Lenovo B");
        c.add_display("DP-2", "Lenovo A (DP-2)");
        c.set_hardware("DP-2", None);
        c.set_hardware("DP-3", None);
        c
    }

    fn bright(c: &Core) -> Vec<f32> {
        c.displays.iter().map(|d| d.brightness).collect()
    }

    fn set(c: &mut Core, change: Change, display: Option<&str>) -> Result<(), String> {
        c.handle(
            &Request::Brightness { change, display: display.map(str::to_string), force: false },
            &at(12.0),
        )
    }

    #[test]
    fn displays_are_picked_by_number_connector_or_description() {
        let mut c = core(FIXED, 12.0);
        set(&mut c, Change::To(40.0), Some("1")).unwrap();
        assert_eq!(bright(&c), [40.0, 100.0], "sorted by connector: DP-2 is 1");
        set(&mut c, Change::To(50.0), Some("dp-3")).unwrap();
        set(&mut c, Change::To(60.0), Some("lenovo a")).unwrap();
        assert_eq!(bright(&c), [60.0, 50.0]);
        for bad in ["0", "3", "nope", "lenovo"] {
            assert!(set(&mut c, Change::To(10.0), Some(bad)).is_err(), "{bad}");
        }
        assert_eq!(bright(&c), [60.0, 50.0]);
    }

    #[test]
    fn steps_stop_at_the_floor_without_jumping_up() {
        let mut c = core(FIXED, 12.0);
        set(&mut c, Change::To(8.0), Some("1")).unwrap();
        set(&mut c, Change::To(2.0), Some("2")).unwrap();
        set(&mut c, Change::By(-5.0), None).unwrap();
        assert_eq!(bright(&c), [5.0, 2.0]);
        set(&mut c, Change::By(500.0), None).unwrap();
        assert_eq!(bright(&c), [100.0, 100.0]);
    }

    #[test]
    fn manual_warmth_lasts_until_the_phase_changes() {
        let mut c = core(FIXED, 12.0);
        assert_eq!(c.output(&c.displays[0]).kelvin, 6500);
        c.handle(&Request::Warmth(Change::By(-1500.0)), &at(12.0)).unwrap();
        c.tick(&at(13.0));
        assert_eq!(c.output(&c.displays[0]).kelvin, 5000);
        c.tick(&at(23.5));
        assert_eq!(c.output(&c.displays[0]).kelvin, 2700);
        c.handle(&Request::Warmth(Change::To(4000.0)), &at(23.5)).unwrap();
        c.handle(&Request::Auto, &at(23.5)).unwrap();
        assert_eq!(c.output(&c.displays[0]).kelvin, 2700);
    }

    #[test]
    fn without_a_schedule_warmth_is_remembered() {
        let mut c = core("[schedule]\nenabled=0\n", 23.5);
        assert_eq!(c.output(&c.displays[0]).kelvin, 6500);
        c.handle(&Request::Warmth(Change::To(3000.0)), &at(23.5)).unwrap();
        set(&mut c, Change::To(30.0), Some("1")).unwrap();
        assert!(c.handle(&Request::Auto, &at(23.5)).is_err());
        let saved = Ini::parse(&c.state().to_text());
        let mut again = Core::new(settings(&Ini::parse("[schedule]\nenabled=0\n")), saved, &at(1.0));
        again.add_display("HDMI-A-1", "Lenovo A"); // same display, another port
        again.set_hardware("HDMI-A-1", None);
        assert_eq!(again.output(&again.displays[0]), Output { kelvin: 3000, software: 0.3, hardware: None });
    }

    #[test]
    fn pause_panic_and_expiry() {
        let mut c = core(FIXED, 23.5);
        set(&mut c, Change::To(30.0), None).unwrap();
        let dim = Output { kelvin: 2700, software: 0.3, hardware: None };
        let neutral = Output { kelvin: 6500, software: 1.0, hardware: None };
        c.handle(&Request::Pause(Some(10)), &at(23.5)).unwrap();
        assert_eq!(c.output(&c.displays[0]), neutral);
        c.tick(&Now { unix: 1_000_599, ..at(23.5) });
        assert_eq!(c.output(&c.displays[0]), neutral);
        c.tick(&Now { unix: 1_000_600, ..at(23.5) });
        assert_eq!(c.output(&c.displays[0]), dim);
        c.handle(&Request::Panic, &at(23.5)).unwrap();
        assert_eq!(c.output(&c.displays[0]), neutral);
        assert_eq!(bright(&c), [100.0, 100.0]);
        c.tick(&Now { unix: 1_003_600, ..at(23.5) });
        assert_eq!(c.output(&c.displays[0]), neutral, "still neutral after the hour: no surprise dimming");
    }

    #[test]
    fn panic_raises_the_backlight_but_pause_leaves_it() {
        let mut c = core(FIXED, 12.0);
        c.displays.iter_mut().for_each(|d| d.has_hardware = Some(true));
        set(&mut c, Change::To(25.0), None).unwrap();
        let o = c.output(&c.displays[0]);
        assert_eq!((o.hardware, o.software), (Some(0.0), 0.5));
        c.handle(&Request::Toggle, &at(12.0)).unwrap();
        let o = c.output(&c.displays[0]);
        assert_eq!((o.hardware, o.software), (Some(0.0), 1.0));
        c.handle(&Request::Panic, &at(12.0)).unwrap();
        assert_eq!(c.output(&c.displays[0]).hardware, Some(100.0));
    }

    #[test]
    fn a_new_display_keeps_its_backlight_level() {
        let mut c = core(FIXED, 12.0);
        c.displays.iter_mut().for_each(|d| d.has_hardware = None);
        assert_eq!(c.output(&c.displays[0]).hardware, None, "not known yet");
        assert!(c.state().get("display.Lenovo A", "brightness").is_none(), "nothing worth remembering");
        c.set_hardware("DP-2", Some(80.0));
        c.set_hardware("DP-3", None);
        assert_eq!(bright(&c), [90.0, 100.0]);
        assert_eq!(c.output(&c.displays[0]), Output { kelvin: 6500, software: 1.0, hardware: Some(80.0) });
        assert_eq!(c.output(&c.displays[1]).hardware, None);
        // A remembered brightness wins.
        let mut again = Core::new(settings(&Ini::parse(FIXED)), c.state().clone(), &at(12.0));
        again.add_display("DP-2", "Lenovo A");
        again.set_hardware("DP-2", Some(10.0));
        assert_eq!(bright(&again), [90.0]);
    }

    #[test]
    fn config_options_apply() {
        let night = format!("{FIXED}night_brightness=40\n[monitor.DP-3]\nenabled=0\n");
        let c = core(&night, 23.5);
        assert_eq!(c.output(&c.displays[0]).software, 0.4);
        assert_eq!(c.output(&c.displays[1]).software, 1.0, "disabled display is left alone");
        let s = settings(&Ini::parse("[schedule]\ncity=Berlin\n"));
        assert!((s.schedule.lat - 52.5).abs() < 0.2 && (s.schedule.lon - 13.4).abs() < 0.2);
    }

    #[test]
    fn json_is_escaped() {
        assert_eq!(json("a\"b\\c\n"), r#""a\"b\\c\u000a""#);
        let mut c = core(FIXED, 12.0);
        c.remove_display("DP-3");
        assert_eq!(
            c.status_json(),
            r#"{"brightness":100,"kelvin":6500,"mode":"schedule","paused_until":null,"phase":"day","displays":[{"name":"DP-2","description":"Lenovo A","brightness":100,"enabled":true,"backlight":false,"gamma":false}]}"#
        );
    }
}
