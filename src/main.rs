use std::io::{self, Read};
use std::path::PathBuf;

#[derive(Debug, Default, Eq, PartialEq)]
struct Options {
    paths: Vec<PathBuf>,
    width: usize,
    help: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("mdrs: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let options = parse_args(std::env::args().skip(1))?;
    if options.help {
        print_usage();
        return Ok(());
    }

    let mut config = mdrs::PagerConfig {
        paths: options.paths,
        width: options.width,
        ..Default::default()
    };
    if config.paths.is_empty() {
        io::stdin().read_to_end(&mut config.initial_source)?;
        config.label = "stdin".into();
    }
    mdrs::run_pager(&config)?;
    Ok(())
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    let mut positional_only = false;

    while let Some(argument) = args.next() {
        if positional_only {
            options.paths.push(argument.into());
            continue;
        }
        match argument.as_str() {
            "--" => positional_only = true,
            "-h" | "--help" => options.help = true,
            "-w" | "--width" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{argument} requires a value"))?;
                options.width = value
                    .parse()
                    .map_err(|_| format!("invalid width: {value}"))?;
                if options.width == 0 {
                    return Err("width must be a positive integer".into());
                }
            }
            _ if argument.starts_with('-') => {
                return Err(format!("unknown option: {argument}"));
            }
            _ => options.paths.push(argument.into()),
        }
    }
    Ok(options)
}

fn print_usage() {
    println!("Usage: mdrs [options] [file ...]");
    println!();
    println!("Options:");
    println!("  -w, --width WIDTH  render at a fixed width");
    println!("  -h, --help         show this help");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_paths_and_width() {
        assert_eq!(
            parse_args(["-w", "72", "one.md", "two.md"].map(str::to_owned)).unwrap(),
            Options {
                paths: vec!["one.md".into(), "two.md".into()],
                width: 72,
                help: false,
            }
        );
    }

    #[test]
    fn supports_dash_prefixed_paths_after_separator() {
        let options = parse_args(["--", "-notes.md"].map(str::to_owned)).unwrap();
        assert_eq!(options.paths, vec![PathBuf::from("-notes.md")]);
    }

    #[test]
    fn rejects_invalid_options() {
        assert!(parse_args(["--wat"].map(str::to_owned)).is_err());
        assert!(parse_args(["--width", "0"].map(str::to_owned)).is_err());
    }
}
