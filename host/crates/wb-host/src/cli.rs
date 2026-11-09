//! Command-line parsing for `wb-host` (SPEC 3.5). Hand-rolled, std only.

use std::fmt::Display;
use std::path::PathBuf;
use std::str::FromStr;

pub const USAGE: &str = "\
usage: wb-host [options]

  --output <index|name>      output to capture (default: primary)
  --fps <n>                  target frame rate (default 60)
  --idle-fps <n>             repeat rate while the screen is unchanged, 1..=60 (default 5)
  --bitrate-kbps <n>         H.264 bitrate in kbit/s (default 20000)
  --port <n>                 iPad TCP port (default 7878)
  --list-outputs             list capturable outputs and exit
  --dump <file.h264>         write the Annex-B stream to a file instead of serving the iPad
  --seconds <n>              length of a --dump run (default 10)
  --width <w> --height <h>   encode size (default: capture size rounded down to even)
  -h, --help                 show this help";

/// Which output to capture, as typed on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputSpec {
    Primary,
    Index(u32),
    NameContains(String),
}

/// What the process does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Serve the iPad over usbmuxd until Ctrl+C.
    Serve,
    /// Write the encoded stream to a file.
    Dump(PathBuf),
    ListOutputs,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub mode: Mode,
    pub output: OutputSpec,
    pub fps: u32,
    /// Re-encode rate of an unchanged screen.
    pub idle_fps: u32,
    pub bitrate_kbps: u32,
    pub port: u16,
    pub seconds: u64,
    /// Encode size override; `None` means the capture size.
    pub size: Option<(u32, u32)>,
}

/// Parses the arguments after the program name. Accepts `--flag value` and `--flag=value`.
pub fn parse<I>(args: I) -> Result<Args, String>
where
    I: IntoIterator<Item = String>,
{
    let mut rest = args.into_iter();
    let mut out = Args {
        mode: Mode::Serve,
        output: OutputSpec::Primary,
        fps: 60,
        idle_fps: 5,
        bitrate_kbps: 20_000,
        port: 7878,
        seconds: 10,
        size: None,
    };
    let mut help = false;
    let mut list = false;
    let mut dump: Option<PathBuf> = None;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;

    while let Some(raw) = rest.next() {
        let (flag, inline) = split_flag(&raw);
        match flag {
            "--output" => out.output = parse_output(&take_value(flag, inline, &mut rest)?)?,
            "--fps" => {
                out.fps = number(flag, &take_value(flag, inline, &mut rest)?, 1, 240)?;
            }
            "--idle-fps" => {
                out.idle_fps = number(flag, &take_value(flag, inline, &mut rest)?, 1, 60)?;
            }
            "--bitrate-kbps" => {
                out.bitrate_kbps =
                    number(flag, &take_value(flag, inline, &mut rest)?, 100, 200_000)?;
            }
            "--port" => {
                out.port = number(flag, &take_value(flag, inline, &mut rest)?, 1, u16::MAX)?;
            }
            "--seconds" => {
                out.seconds = number(flag, &take_value(flag, inline, &mut rest)?, 1, 86_400)?;
            }
            "--width" => {
                width = Some(number(
                    flag,
                    &take_value(flag, inline, &mut rest)?,
                    2,
                    8192,
                )?);
            }
            "--height" => {
                height = Some(number(
                    flag,
                    &take_value(flag, inline, &mut rest)?,
                    2,
                    8192,
                )?);
            }
            "--dump" => {
                dump = Some(PathBuf::from(take_value(flag, inline, &mut rest)?));
            }
            "--list-outputs" => {
                no_value(flag, inline)?;
                list = true;
            }
            "--help" | "-h" => {
                no_value(flag, inline)?;
                help = true;
            }
            _ => return Err(format!("unknown argument '{raw}'")),
        }
    }

    out.mode = if help {
        Mode::Help
    } else if list {
        Mode::ListOutputs
    } else if let Some(path) = dump {
        Mode::Dump(path)
    } else {
        Mode::Serve
    };
    out.size = match (width, height) {
        (None, None) => None,
        (Some(w), Some(h)) => Some((w, h)),
        _ => return Err("--width and --height must be given together".to_string()),
    };
    Ok(out)
}

/// Splits `--flag=value` into its parts. Anything else is returned unchanged.
fn split_flag(raw: &str) -> (&str, Option<&str>) {
    if raw.starts_with("--") {
        if let Some((flag, value)) = raw.split_once('=') {
            return (flag, Some(value));
        }
    }
    (raw, None)
}

fn take_value(
    flag: &str,
    inline: Option<&str>,
    rest: &mut impl Iterator<Item = String>,
) -> Result<String, String> {
    match inline {
        Some(value) => Ok(value.to_string()),
        None => rest.next().ok_or_else(|| format!("{flag} needs a value")),
    }
}

fn no_value(flag: &str, inline: Option<&str>) -> Result<(), String> {
    match inline {
        Some(_) => Err(format!("{flag} takes no value")),
        None => Ok(()),
    }
}

fn number<T>(flag: &str, value: &str, min: T, max: T) -> Result<T, String>
where
    T: FromStr + PartialOrd + Display + Copy,
{
    let n: T = value
        .parse()
        .map_err(|_| format!("{flag}: '{value}' is not a valid number"))?;
    if !(min..=max).contains(&n) {
        return Err(format!("{flag} must be between {min} and {max}"));
    }
    Ok(n)
}

fn parse_output(value: &str) -> Result<OutputSpec, String> {
    if value.is_empty() {
        return Err("--output needs an index or a name".to_string());
    }
    Ok(match value.parse::<u32>() {
        Ok(index) => OutputSpec::Index(index),
        Err(_) => OutputSpec::NameContains(value.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults() {
        let a = p(&[]).unwrap();
        assert_eq!(a.mode, Mode::Serve);
        assert_eq!(a.output, OutputSpec::Primary);
        assert_eq!(
            (a.fps, a.bitrate_kbps, a.port, a.seconds),
            (60, 20_000, 7878, 10)
        );
        assert_eq!(a.idle_fps, 5);
        assert_eq!(a.size, None);
    }

    #[test]
    fn idle_fps_parses_both_forms() {
        assert_eq!(p(&["--idle-fps", "2"]).unwrap().idle_fps, 2);
        assert_eq!(p(&["--idle-fps=60"]).unwrap().idle_fps, 60);
        assert_eq!(p(&["--idle-fps", "1"]).unwrap().idle_fps, 1);
    }

    #[test]
    fn space_and_equals_forms() {
        let a = p(&["--output", "1", "--fps=30", "--port", "9000"]).unwrap();
        assert_eq!(a.output, OutputSpec::Index(1));
        assert_eq!(a.fps, 30);
        assert_eq!(a.port, 9000);
    }

    #[test]
    fn output_name_selector() {
        let a = p(&["--output", "Virtual"]).unwrap();
        assert_eq!(a.output, OutputSpec::NameContains("Virtual".to_string()));
    }

    #[test]
    fn dump_and_seconds() {
        let a = p(&["--dump", "out.h264", "--seconds", "3"]).unwrap();
        assert_eq!(a.mode, Mode::Dump(PathBuf::from("out.h264")));
        assert_eq!(a.seconds, 3);
    }

    #[test]
    fn list_and_help_modes() {
        assert_eq!(p(&["--list-outputs"]).unwrap().mode, Mode::ListOutputs);
        assert_eq!(p(&["-h"]).unwrap().mode, Mode::Help);
        assert_eq!(p(&["--help"]).unwrap().mode, Mode::Help);
    }

    #[test]
    fn size_needs_both_dimensions() {
        assert!(p(&["--width", "1280"]).is_err());
        assert!(p(&["--height", "720"]).is_err());
        let a = p(&["--width", "1280", "--height", "720"]).unwrap();
        assert_eq!(a.size, Some((1280, 720)));
    }

    #[test]
    fn unknown_flag_is_an_error() {
        let err = p(&["--bogus"]).unwrap_err();
        assert!(err.contains("--bogus"));
    }

    #[test]
    fn positional_argument_is_an_error() {
        assert!(p(&["stray"]).is_err());
    }

    #[test]
    fn missing_value_is_an_error() {
        assert!(p(&["--fps"]).is_err());
        assert!(p(&["--dump"]).is_err());
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        assert!(p(&["--fps", "0"]).is_err());
        assert!(p(&["--fps", "1000"]).is_err());
        assert!(p(&["--idle-fps", "0"]).is_err());
        assert!(p(&["--idle-fps", "61"]).is_err());
        assert!(p(&["--port", "0"]).is_err());
        assert!(p(&["--bitrate-kbps", "5"]).is_err());
        assert!(p(&["--seconds", "0"]).is_err());
    }

    #[test]
    fn non_numeric_values_are_rejected() {
        assert!(p(&["--fps", "sixty"]).is_err());
        assert!(p(&["--port", "-1"]).is_err());
    }

    #[test]
    fn flag_without_value_rejects_equals_form() {
        assert!(p(&["--list-outputs=yes"]).is_err());
    }
}
