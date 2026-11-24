//! The Linux version: `slc daemon` holds the displays' gamma, and every other command talks to it over a
//! socket. No GUI: bars, keybindings and menus call the command line.

mod backlight;
mod cli;
mod core;
mod daemon;
mod wayland;

use crate::{cities, VERSION};
use cli::{Command, Request};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// Separates arguments in a request line.
const ARG_SEPARATOR: char = '\x1f';

/// `$var/slc`, or `~/<fallback>/slc`.
fn xdg(var: &str, fallback: &str) -> PathBuf {
    let dir = std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute());
    dir.unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback))
        .join("slc")
}

fn config_path() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("slc.ini")
}

fn state_path() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("state.ini")
}

/// One daemon per Wayland session.
fn socket_path() -> Result<PathBuf, String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
    let session = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    Ok(PathBuf::from(dir).join(format!("slc-{}.sock", session.replace('/', "_"))))
}

/// Sends a request to the daemon and prints its answer.
fn request(args: &[String]) -> Result<(), String> {
    let mut s = UnixStream::connect(socket_path()?)
        .map_err(|_| "SLC is not running; start it with `slc daemon`".to_string())?;
    let line = args.join(&ARG_SEPARATOR.to_string()).replace('\n', " ");
    writeln!(s, "{line}").map_err(|e| e.to_string())?;
    let mut answer = BufReader::new(s);
    let mut verdict = String::new();
    answer.read_line(&mut verdict).map_err(|e| e.to_string())?;
    match verdict.trim_end() {
        "ok" => {}
        "error" => {
            let mut text = String::new();
            answer.read_line(&mut text).map_err(|e| e.to_string())?;
            return Err(text.trim_end().to_string());
        }
        _ => return Err("the daemon did not answer".into()),
    }
    let mut out = io::stdout();
    for line in answer.lines() {
        let line = line.map_err(|e| e.to_string())?;
        // A closed pipe (the bar went away) ends `slc watch`.
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
    Ok(())
}

pub fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match cli::parse(&args) {
        Err(e) => {
            eprintln!("slc: {e}\nTry `slc help`.");
            std::process::exit(2);
        }
        Ok(Command::Help) => {
            print!("{}", cli::HELP);
            Ok(())
        }
        Ok(Command::Version) => {
            println!("SLC {VERSION}");
            Ok(())
        }
        Ok(Command::SampleConfig) => {
            print!("{}", include_str!("sample.ini"));
            Ok(())
        }
        Ok(Command::UdevRules) => {
            print!("{}", include_str!("60-slc.rules"));
            Ok(())
        }
        Ok(Command::Cities(q)) => {
            let hits = cities::search(&q, 10);
            hits.iter().for_each(|c| println!("{}  {:.2} {:.2}", c.label(), c.lat, c.lon));
            if hits.is_empty() {
                Err("no such city; use lat and lon instead".to_string())
            } else {
                Ok(())
            }
        }
        Ok(Command::Daemon) => daemon::run(),
        Ok(Command::Request(Request::Watch)) => request(&args).and(Err("the daemon stopped".into())),
        Ok(Command::Request(_)) => request(&args),
    };
    if let Err(e) = result {
        eprintln!("slc: {e}");
        std::process::exit(1);
    }
}
