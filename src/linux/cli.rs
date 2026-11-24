//! Linux command line: parsing only, no I/O.

use crate::color;
use crate::engine::{MAX_BRIGHTNESS, MIN_BRIGHTNESS};

/// Going darker than this needs `--force` (the screen may become unreadable).
pub const DEEP_DIM: f32 = 5.0;

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Help,
    Version,
    Daemon,
    /// Print a commented sample config.
    SampleConfig,
    /// Print the udev rule that allows backlight control.
    UdevRules,
    Cities(String),
    /// Anything the running daemon answers.
    Request(Request),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Change {
    To(f32),
    By(f32),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Brightness {
        change: Change,
        display: Option<String>,
        force: bool,
    },
    Warmth(Change),
    Auto,
    /// Minutes; `None` = until resumed.
    Pause(Option<u64>),
    Resume,
    Toggle,
    Panic,
    Status {
        json: bool,
    },
    Watch,
    Reload,
    Quit,
}

pub const HELP: &str = "\
Screen Lighting Control (SLC): screen brightness and color temperature

Usage: slc <command>

  daemon                        run SLC (start this with your session)
  set <1-100 | +N | -N>         brightness, for all displays or one:
      [-d <display>]              number, connector (DP-1) or part of its description
      [--force]                   allow going below 5%
  warmth <1200-6500 | +N | -N>  color temperature in kelvin (lower is warmer)
  auto                          return to the schedule
  pause [minutes]               neutral colors, no software dimming
  resume | toggle               end the pause, or switch it on and off
  panic                         full brightness, neutral colors, pause for 1 hour
  status [--json]               what SLC is doing now
  watch                         print the JSON status now and on every change
  reload                        read the config file again
  quit                          stop the daemon (colors go back to normal)

  cities <name>                 search the built-in city list
  sample-config                 print a config file with the defaults
  udev-rules                    print the rule that allows backlight control (see README)
  help | version

Config: $XDG_CONFIG_HOME/slc/slc.ini (SLC never writes to it)
";

fn change(s: &str) -> Result<Change, String> {
    let n = s.trim_end_matches(['%', 'k', 'K']);
    let v: f32 =
        n.parse().ok().filter(|v: &f32| v.is_finite()).ok_or_else(|| format!("not a number: {s}"))?;
    Ok(if n.starts_with(['+', '-']) { Change::By(v) } else { Change::To(v) })
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut words = args.iter().map(String::as_str);
    let Some(cmd) = words.next() else { return Ok(Command::Help) };
    let rest: Vec<&str> = words.collect();
    let req = |r| Ok(Command::Request(r));
    match (cmd, rest.as_slice()) {
        ("help" | "--help" | "-h", _) => Ok(Command::Help),
        ("version" | "--version" | "-V", _) => Ok(Command::Version),
        ("daemon", []) => Ok(Command::Daemon),
        ("sample-config", []) => Ok(Command::SampleConfig),
        ("udev-rules", []) => Ok(Command::UdevRules),
        ("cities", [_, ..]) => Ok(Command::Cities(rest.join(" "))),
        ("set", [value, opts @ ..]) => {
            let change = change(value)?;
            let (mut display, mut force) = (None, false);
            let mut it = opts.iter();
            while let Some(o) = it.next() {
                match *o {
                    "--force" => force = true,
                    "-d" | "--display" => {
                        display = Some(it.next().ok_or("-d needs a display")?.to_string());
                    }
                    o => return Err(format!("unknown option: {o}")),
                }
            }
            if let Change::To(v) = change {
                if !(MIN_BRIGHTNESS..=MAX_BRIGHTNESS).contains(&v) {
                    return Err("brightness goes from 1 to 100".into());
                }
                if v < DEEP_DIM && !force {
                    return Err("below 5% the screen may be unreadable; add --force if you mean it \
                                (`slc panic` brings it back)"
                        .into());
                }
            }
            req(Request::Brightness { change, display, force })
        }
        ("warmth", [value]) => {
            let change = change(value)?;
            if let Change::To(v) = change {
                if !(color::MIN_KELVIN as f32..=color::MAX_KELVIN as f32).contains(&v) {
                    return Err("warmth goes from 1200 to 6500 kelvin".into());
                }
            }
            req(Request::Warmth(change))
        }
        ("auto", []) => req(Request::Auto),
        ("pause", []) => req(Request::Pause(None)),
        ("pause", [m]) => {
            let m =
                m.parse().ok().filter(|m| (1..=10_080).contains(m)).ok_or("pause takes 1-10080 minutes")?;
            req(Request::Pause(Some(m)))
        }
        ("resume", []) => req(Request::Resume),
        ("toggle", []) => req(Request::Toggle),
        ("panic", []) => req(Request::Panic),
        ("status", []) => req(Request::Status { json: false }),
        ("status", ["--json"]) => req(Request::Status { json: true }),
        ("watch", []) => req(Request::Watch),
        ("reload", []) => req(Request::Reload),
        ("quit", []) => req(Request::Quit),
        _ => Err(format!("don't understand: {}", args.join(" "))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: &str) -> Result<Command, String> {
        parse(&line.split_whitespace().map(str::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn brightness() {
        let set = |change, display: Option<&str>, force| {
            Ok(Command::Request(Request::Brightness { change, display: display.map(str::to_string), force }))
        };
        assert_eq!(p("set 40%"), set(Change::To(40.0), None, false));
        assert_eq!(p("set -5 -d DP-2"), set(Change::By(-5.0), Some("DP-2"), false));
        assert_eq!(p("set 2 --force"), set(Change::To(2.0), None, true));
        for bad in
            ["set 2", "set 0 --force", "set 101", "set NaN", "set +inf", "set", "set 40 -d", "set 40 x"]
        {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn other_commands() {
        assert_eq!(p(""), Ok(Command::Help));
        assert_eq!(p("warmth 3400K"), Ok(Command::Request(Request::Warmth(Change::To(3400.0)))));
        assert_eq!(p("pause 60"), Ok(Command::Request(Request::Pause(Some(60)))));
        assert_eq!(p("cities sao paulo"), Ok(Command::Cities("sao paulo".into())));
        for bad in ["scene Night", "warmth 900", "pause 0", "pause -3", "bogus", "quit now"] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }
}
