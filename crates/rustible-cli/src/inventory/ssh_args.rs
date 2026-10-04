//! What the inventory's `ssh_args` may say, read the way `ssh` reads them.
//!
//! `transport::master_argv` puts `ssh_args` before Rustible's own options,
//! because OpenSSH keeps the *first* value it sees for `-l`, `-p` and every
//! `-o` option: that is what lets `ssh_args` override a default of ours. It
//! also means an `ssh_args` that sets something the connection depends on, or
//! that the login, port and address parameters own, would win silently, and
//! that a word which is not an option, or a `--`, changes the destination.
//! Those are refused at load instead, naming the node; [`SshOption`] is the
//! classification, and `master_argv`'s tests check that every option it
//! passes has one.
//!
//! Modes that replace what `ssh` does (`-G`, `-O`, `-W`, `-V`, `-Q`, `-s`)
//! are not refused here: they make the master fail loudly, not differently.

/// The `ssh` letters that take a value, from OpenSSH's getopt string
/// (`1246ab:c:e:fgi:kl:m:no:p:qstvxAB:CD:E:F:GI:J:KL:MNO:P:Q:R:S:TVw:W:XYy`).
const TAKES_VALUE: &str = "bceilmopBDEFIJLOPQRSwW";

/// What one option in an `ssh` command line is to Rustible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshOption {
    /// Something the connection depends on; `ssh_args` may not set it.
    Owned {
        /// The setting, as a message names it.
        setting: &'static str,
        /// Why the connection needs Rustible's value.
        why: &'static str,
    },
    /// What an inventory parameter sets; `ssh_args` may not set it.
    Param {
        /// The setting, as a message names it.
        setting: &'static str,
        /// The parameter to use instead.
        param: &'static str,
    },
    /// A default of Rustible's, which `ssh_args` overrides.
    Default,
    /// Anything else, passed to `ssh` untouched.
    Free,
}

const CONTROL_MASTER: SshOption = SshOption::Owned {
    setting: "`ControlMaster`",
    why: "every command of the run is multiplexed over the master it starts",
};
const CONTROL_PATH: SshOption = SshOption::Owned {
    setting: "`ControlPath`",
    why: "the run reaches the master through a control socket it chooses",
};
const LOG_FILE: SshOption = SshOption::Owned {
    setting: "ssh's log file",
    why: "a failed connection is reported from the log Rustible chooses",
};
const FORK: SshOption = SshOption::Owned {
    setting: "`ForkAfterAuthentication`",
    why: "the run waits for the master to go to the background once logged in",
};
const SESSION_TYPE: SshOption = SshOption::Owned {
    setting: "`SessionType`",
    why: "the master runs no remote command of its own",
};
const LOGIN: SshOption = SshOption::Param {
    setting: "the login user",
    param: "ssh_user",
};
const PORT: SshOption = SshOption::Param {
    setting: "the port",
    param: "port",
};
const ADDR: SshOption = SshOption::Param {
    setting: "the address",
    param: "addr",
};

fn flag(letter: char) -> SshOption {
    match letter {
        'M' => CONTROL_MASTER,
        'f' => FORK,
        'N' => SESSION_TYPE,
        _ => SshOption::Free,
    }
}

fn with_value(letter: char) -> SshOption {
    match letter {
        'S' => CONTROL_PATH,
        'E' => LOG_FILE,
        'l' => LOGIN,
        'p' => PORT,
        _ => SshOption::Free,
    }
}

/// The keyword of an `-o` option and its value, read as loosely as `ssh`
/// does: it skips leading whitespace, `=` and `"` before the keyword
/// (`-o=User=x`, `-o '"User" x'`), removes quotes inside it (`U"ser"` is
/// `User`), and `Key=Value`, `Key Value` and `Key = Value` are one option.
/// Keywords are letters, and only alphabetic ones are classified, so the
/// keyword is the leading run of letters and quotes, without the quotes.
fn config_option(option: &str) -> (String, &str) {
    let skip = |c: char| c == '=' || c == '"' || c.is_whitespace();
    let option = option.trim_start_matches(skip);
    let end = option
        .find(|c: char| !c.is_ascii_alphabetic() && c != '"')
        .unwrap_or(option.len());
    let value = option[end..].trim_start_matches(|c: char| c == '"' || c.is_whitespace());
    let value = value
        .strip_prefix('=')
        .unwrap_or(value)
        .trim()
        .trim_matches('"');
    (option[..end].replace('"', ""), value)
}

/// `-o` keywords, which `ssh` matches without regard to case.
fn config_key(key: &str) -> SshOption {
    match key.to_ascii_lowercase().as_str() {
        "controlmaster" => CONTROL_MASTER,
        "controlpath" => CONTROL_PATH,
        "controlpersist" => SshOption::Owned {
            setting: "`ControlPersist`",
            why: "it is the master's idle timeout, which has to outlast the build \
                  and still end the master if the run dies",
        },
        "batchmode" => SshOption::Owned {
            setting: "`BatchMode`",
            why: "a run cannot answer a prompt",
        },
        "forkafterauthentication" => FORK,
        "sessiontype" => SESSION_TYPE,
        "user" => LOGIN,
        "port" => PORT,
        "hostname" => ADDR,
        "stricthostkeychecking" => SshOption::Default,
        _ => SshOption::Free,
    }
}

/// One option as found: how it was written, its value, and what it is.
struct Found {
    spelling: String,
    value: Option<String>,
    class: SshOption,
}

/// A word of `ssh_args` that is not an option. Since `ssh_args` come
/// first, each one moves where the run goes.
enum Stray {
    /// `ssh` reads it as the destination.
    Word(String),
    /// Ends `ssh`'s options, so Rustible's own become the destination.
    DoubleDash,
    /// An option left without its value at the end, which would take
    /// Rustible's first option as its value.
    Dangling(String),
}

/// Walk `args` as `ssh`'s getopt does: grouped flags (`-4M`), a value
/// attached (`-p2222`, `-oUser=x`) or in the next word, and nothing after
/// `--`. A word that is not an option is not classified; `ssh` reads it as
/// the destination and goes on parsing.
fn scan(args: &[String]) -> (Vec<Found>, Vec<Stray>) {
    let mut found = vec![];
    let mut stray = vec![];
    let mut words = args.iter();
    while let Some(word) = words.next() {
        if word == "--" {
            stray.push(Stray::DoubleDash);
            break;
        }
        let Some(letters) = word.strip_prefix('-').filter(|l| !l.is_empty()) else {
            stray.push(Stray::Word(word.clone()));
            continue;
        };
        for (at, letter) in letters.char_indices() {
            if !TAKES_VALUE.contains(letter) {
                found.push(Found {
                    spelling: word.clone(),
                    value: None,
                    class: flag(letter),
                });
                continue;
            }
            let attached = &letters[at + letter.len_utf8()..];
            let (value, spelling) = if !attached.is_empty() {
                (attached.to_string(), word.clone())
            } else if let Some(next) = words.next() {
                (next.clone(), format!("{word} {next}"))
            } else {
                stray.push(Stray::Dangling(word.clone()));
                return (found, stray);
            };
            let (class, value) = if letter == 'o' {
                let (key, value) = config_option(&value);
                (config_key(&key), value.to_string())
            } else {
                (with_value(letter), value)
            };
            found.push(Found {
                spelling,
                value: Some(value),
                class,
            });
            break;
        }
    }
    (found, stray)
}

/// Every option in an `ssh` command line up to `--`, with how it was written
/// and what it is to Rustible. Words that are not options, and an option
/// left without its value, are not listed; see [`ssh_args_problems`].
pub fn ssh_options(args: &[String]) -> Vec<(String, SshOption)> {
    scan(args)
        .0
        .into_iter()
        .map(|f| (f.spelling, f.class))
        .collect()
}

/// Why each part of an `ssh_args` cannot be used, worded to follow the
/// node it is on ("`ssh_args` on host `h` "). Empty when every option
/// may pass.
pub fn ssh_args_problems(args: &[String]) -> Vec<String> {
    let (found, stray) = scan(args);
    let mut problems: Vec<String> = found
        .into_iter()
        .filter_map(|f| match f.class {
            SshOption::Owned { setting, why } => Some(format!(
                "sets {setting} (`{}`), which Rustible's ssh connection depends on: {why}; \
                 remove it from `ssh_args`",
                f.spelling
            )),
            SshOption::Param { setting, param } => {
                let value = f.value.unwrap_or_default();
                let suggestion = match param {
                    "port" if value.parse::<u16>().is_err() => {
                        "the `port` parameter (a number)".to_string()
                    }
                    "port" => format!("the parameter `port={value}`"),
                    _ => format!("the parameter `{param}={value:?}`"),
                };
                Some(format!(
                    "sets {setting} (`{}`); use {suggestion} instead",
                    f.spelling
                ))
            }
            SshOption::Default | SshOption::Free => None,
        })
        .collect();
    problems.extend(stray.into_iter().map(|s| {
        match s {
            Stray::Word(word) => format!(
                "contains `{word}`, which is not an option: ssh would connect to it instead of \
             the host's `addr`; remove it, or put it after the option it belongs to"
            ),
            Stray::DoubleDash => "contains `--`, after which ssh would read Rustible's own \
             options as the machine to connect to; remove it"
                .to_string(),
            Stray::Dangling(word) => format!("ends with `{word}`, which needs a value"),
        }
    }));
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn problems(words: &[&str]) -> Vec<String> {
        ssh_args_problems(&args(words))
    }

    #[test]
    fn every_spelling_of_an_owned_option_is_refused() {
        for (words, setting) in [
            (&["-M"][..], "`ControlMaster`"),
            (&["-4M"], "`ControlMaster`"),
            (&["-NM"], "`SessionType`"),
            (&["-f"], "`ForkAfterAuthentication`"),
            (&["-S", "/tmp/s"], "`ControlPath`"),
            (&["-S/tmp/s"], "`ControlPath`"),
            (&["-E", "/tmp/log"], "ssh's log file"),
            (&["-o", "BatchMode=no"], "`BatchMode`"),
            (&["-oBatchMode=no"], "`BatchMode`"),
            (&["-o", "batchmode no"], "`BatchMode`"),
            (&["-o", " BatchMode = no"], "`BatchMode`"),
            (&["-o", "ControlMaster=auto"], "`ControlMaster`"),
            (&["-o", "ControlPath=none"], "`ControlPath`"),
            (&["-o", "ControlPersist=5"], "`ControlPersist`"),
            (
                &["-o", "ForkAfterAuthentication=no"],
                "`ForkAfterAuthentication`",
            ),
            (&["-o", "SessionType=default"], "`SessionType`"),
            // `ssh` skips `=`, `"` and whitespace before the keyword, and
            // strips quotes from it (measured with `ssh -G`).
            (&["-o=BatchMode=no"], "`BatchMode`"),
            (&["-o", "=BatchMode=no"], "`BatchMode`"),
            (&["-o", " =BatchMode=no"], "`BatchMode`"),
            (&["-o", "\"BatchMode\" no"], "`BatchMode`"),
            (&["-o", "Bat\"chMode\" no"], "`BatchMode`"),
        ] {
            let p = problems(words);
            assert!(!p.is_empty(), "{words:?} passed");
            assert!(
                p[0].starts_with(&format!("sets {setting} (")),
                "{words:?}: {p:?}"
            );
            assert!(p[0].ends_with("remove it from `ssh_args`"), "{p:?}");
        }
        assert_eq!(
            problems(&["-o", "BatchMode=no"]),
            [
                "sets `BatchMode` (`-o BatchMode=no`), which Rustible's ssh connection \
              depends on: a run cannot answer a prompt; remove it from `ssh_args`"
            ]
        );
        // Each one is named, not only the first.
        assert_eq!(problems(&["-NM"]).len(), 2);
    }

    #[test]
    fn the_login_port_and_address_point_at_their_parameters() {
        for (words, want) in [
            (
                &["-l", "admin"][..],
                "sets the login user (`-l admin`); use the parameter `ssh_user=\"admin\"` instead",
            ),
            (
                &["-ladmin"],
                "sets the login user (`-ladmin`); use the parameter `ssh_user=\"admin\"` instead",
            ),
            (
                &["-4ladmin"],
                "sets the login user (`-4ladmin`); use the parameter `ssh_user=\"admin\"` instead",
            ),
            (
                &["-o", "User=admin"],
                "sets the login user (`-o User=admin`); use the parameter `ssh_user=\"admin\"` instead",
            ),
            (
                &["-o", "user admin"],
                "sets the login user (`-o user admin`); use the parameter `ssh_user=\"admin\"` instead",
            ),
            (
                &["-o=User=admin"],
                "sets the login user (`-o=User=admin`); use the parameter `ssh_user=\"admin\"` instead",
            ),
            (
                &["-o", "=User=admin"],
                "sets the login user (`-o =User=admin`); use the parameter `ssh_user=\"admin\"` instead",
            ),
            (
                &["-o", " =User=admin"],
                "sets the login user (`-o  =User=admin`); use the parameter `ssh_user=\"admin\"` instead",
            ),
            (
                &["-o", "\"User\" admin"],
                "sets the login user (`-o \"User\" admin`); use the parameter `ssh_user=\"admin\"` instead",
            ),
            (
                &["-o", "U\"ser\" admin"],
                "sets the login user (`-o U\"ser\" admin`); use the parameter `ssh_user=\"admin\"` instead",
            ),
            (
                &["-o", "Po\"rt\" 1"],
                "sets the port (`-o Po\"rt\" 1`); use the parameter `port=1` instead",
            ),
            (
                &["-o", "Host\"Name\" 192.0.2.9"],
                "sets the address (`-o Host\"Name\" 192.0.2.9`); use the parameter `addr=\"192.0.2.9\"` instead",
            ),
            (
                &["-o=Port=1"],
                "sets the port (`-o=Port=1`); use the parameter `port=1` instead",
            ),
            (
                &["-p", "ssh"],
                "sets the port (`-p ssh`); use the `port` parameter (a number) instead",
            ),
            (
                &["-o", "HostName=192.0.2.9"],
                "sets the address (`-o HostName=192.0.2.9`); use the parameter `addr=\"192.0.2.9\"` instead",
            ),
            (
                &["-p", "2222"],
                "sets the port (`-p 2222`); use the parameter `port=2222` instead",
            ),
            (
                &["-p2222"],
                "sets the port (`-p2222`); use the parameter `port=2222` instead",
            ),
            (
                &["-o", "Port=2222"],
                "sets the port (`-o Port=2222`); use the parameter `port=2222` instead",
            ),
        ] {
            assert_eq!(problems(words), [want], "{words:?}");
        }
    }

    #[test]
    fn an_option_without_its_value_is_refused() {
        assert_eq!(
            problems(&["-o", "IdentitiesOnly=yes", "-i"]),
            ["ends with `-i`, which needs a value"]
        );
        assert_eq!(problems(&["-4o"]), ["ends with `-4o`, which needs a value"]);
    }

    /// `ssh_args` come first, so a word that is not an option becomes the
    /// destination, and a `--` makes Rustible's own options the destination.
    #[test]
    fn words_that_are_not_options_are_refused() {
        assert_eq!(
            problems(&["-4", "stray"]),
            [
                "contains `stray`, which is not an option: ssh would connect to it instead of \
              the host's `addr`; remove it, or put it after the option it belongs to"
            ]
        );
        assert_eq!(
            problems(&["-4", "--", "-M"]),
            [
                "contains `--`, after which ssh would read Rustible's own options as the \
              machine to connect to; remove it"
            ]
        );
        assert_eq!(problems(&["-"]).len(), 1);
    }

    #[test]
    fn everything_else_passes() {
        for words in [
            &["-4"][..],
            &[
                "-i",
                "/home/me/.ssh/deploy_ed25519",
                "-o",
                "IdentitiesOnly=yes",
            ],
            &["-o", "StrictHostKeyChecking=no", "-o", "ProxyJump=bastion"],
            &["-J", "admin@bastion"],
            // `-i` takes the next word whatever it looks like.
            &["-i", "-M"],
            // The value is the option's own, not a second option.
            &["-o", "ProxyCommand=ssh -W %h:%p -l admin bastion"],
            // A keyword with digits is not one of ours.
            &["-o", "ForwardX11=no"],
            // What `vagrant up` writes into its inventory.
            &[
                "-i",
                "/home/me/.vagrant.d/insecure_private_key",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
            ],
        ] {
            assert_eq!(problems(words), Vec::<String>::new(), "{words:?}");
        }
    }

    #[test]
    fn strict_host_key_checking_is_a_default_to_override() {
        assert_eq!(
            ssh_options(&args(&["-o", "StrictHostKeyChecking=no", "-4"])),
            [
                (
                    "-o StrictHostKeyChecking=no".to_string(),
                    SshOption::Default
                ),
                ("-4".to_string(), SshOption::Free),
            ]
        );
    }
}
