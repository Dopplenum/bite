use std::path::PathBuf;

use crate::{assert_exit, exit};

const HELP: &str = "OVERVIEW: Decompilation tool

USAGE: bite [options] <OBJECT>

OPTIONS:
  -H, --help          Print usage information
  -L, --libs          Print linked shared libraries 
  -N, --names         Print all symbols exposed by object
  -S, --simplify      Replace common types with shortened paths
  -D, --disassemble   Path to object you're disassembling
  -C, --config        Path to config used for disassembling";

const ABBRV: &[&str] = &["-H", "-L", "-S", "-D", "-C"];
const NAMES: &[&str] = &[
    "--help",
    "--libs",
    "--names",
    "--simplify",
    "--disassemble",
    "--config",
];

#[derive(Debug, Clone)]
pub struct Cli {
    /// Print shared libraries the object is linked against.
    pub libs: bool,

    /// Print all symbols exposed by object.
    pub names: bool,

    /// Strip symbols into a simpler format.
    pub simplify: bool,

    /// Disassemble object into `readable` assembly,
    pub disassemble: bool,

    /// Path to symbol being disassembled.
    pub path: Option<PathBuf>,

    /// Optional path to config.
    pub config: Option<PathBuf>,
}

impl Cli {
    pub fn parse() -> Self {
        let mut cli = Cli {
            libs: false,
            names: false,
            simplify: false,
            disassemble: false,
            config: None,
            path: None,
        };

        let mut args = std::env::args().skip(1).peekable();

        // when no argument is given, run the gui
        if args.peek().is_none() {
            cli.disassemble = true;
            return cli;
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-H" | "--help" => exit!("{HELP}"),
                "-S" | "--simplify" => cli.simplify = true,
                "-N" | "--names" => {
                    cli.names = true;

                    if let Some(path) = args.next().as_deref() {
                        if !NAMES.contains(&path) && !ABBRV.contains(&path) {
                            cli.path = Some(PathBuf::from(path));
                        }
                    }
                }
                "-L" | "--libs" => {
                    cli.libs = true;

                    if let Some(path) = args.next().as_deref() {
                        if !NAMES.contains(&path) && !ABBRV.contains(&path) {
                            cli.path = Some(PathBuf::from(path));
                        }
                    }
                }
                "-D" | "--disassemble" => {
                    cli.disassemble = true;

                    if let Some(path) = args.next().as_deref() {
                        if !NAMES.contains(&path) && !ABBRV.contains(&path) {
                            cli.path = Some(PathBuf::from(path));
                        }
                    }
                }
                "-C" | "--config" => {
                    if let Some(path) = args.next().as_deref() {
                        if !NAMES.contains(&path) && !ABBRV.contains(&path) {
                            cli.config = Some(PathBuf::from(path));
                            continue;
                        }
                    }

                    exit!("Missing path to a config.");
                }
                unknown => {
                    let mut distance = u32::MAX;
                    let mut best_guess = "";
                    for name in NAMES {
                        let d = triple_accel::levenshtein_exp(unknown.as_bytes(), name.as_bytes());
                        if d < distance {
                            distance = d;
                            best_guess = name;
                        }
                    }

                    // A guess that's less than 3 `steps` away from a correct arg.
                    if distance < 4 {
                        exit!("Unknown cmd arg '{unknown}' did you mean '{best_guess}'?");
                    } else {
                        exit!("Unknown cmd arg '{unknown}' was entered.");
                    }
                }
            }
        }

        cli.validate_args();
        cli
    }

    fn validate_args(&mut self) {
        if self.disassemble || self.libs || self.names {
            assert_exit!(self.path.is_some(), "Missing path to an object.");
        }

        assert_exit!(
            self.disassemble ^ self.libs ^ self.names,
            "Invalid combination of arguements.\n\n{HELP}"
        );
    }
}
