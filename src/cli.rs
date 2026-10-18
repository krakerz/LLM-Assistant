//! argv parsing, kept pure (no filesystem access) so it can be unit-tested.
//! Options are recognized only before the first positional (the folder);
//! everything after the folder is the headless message, taken verbatim.

use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Gui {
        folder: Option<PathBuf>,
    },
    Headless {
        folder: PathBuf,
        message: String,
    },
    FolderRepl {
        folder: PathBuf,
    },
    PersonaChat {
        persona: Option<String>,
        session: Option<String>,
    },
    ListPersonas,
    ListSessions,
    Server {
        bind: Option<String>,
        port: Option<u16>,
    },
}

#[derive(Default)]
struct Scan {
    help: bool,
    version: bool,
    chat: bool,
    persona_chat: bool,
    list_personas: bool,
    list_sessions: bool,
    server: bool,
    persona: Option<String>,
    session: Option<String>,
    bind: Option<String>,
    port: Option<u16>,
    folder: Option<String>,
    rest: Vec<String>,
    /// First error seen; reported unless `--help`/`--version` also appeared.
    err: Option<String>,
}

impl Scan {
    fn fail(&mut self, msg: String) {
        self.err.get_or_insert(msg);
    }
}

/// `args` excludes argv[0].
pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut s = Scan::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            if let Some(folder) = args.get(i + 1) {
                s.folder = Some(folder.clone());
                s.rest = args[i + 2..].to_vec();
            }
            break;
        }
        if !a.starts_with('-') {
            s.folder = Some(a.to_string());
            s.rest = args[i + 1..].to_vec();
            break;
        }
        match a {
            "-h" | "--help" => s.help = true,
            "-v" | "--version" => s.version = true,
            "--chat" => s.chat = true,
            "--persona-chat" => s.persona_chat = true,
            "--list-personas" => s.list_personas = true,
            "--list-sessions" => s.list_sessions = true,
            "--server" => s.server = true,
            "--persona" | "--session" | "--bind" | "--port" => {
                // A following option is a missing value, not the value.
                match args.get(i + 1).filter(|v| !v.starts_with('-')) {
                    Some(v) => {
                        i += 1;
                        match a {
                            "--persona" => s.persona = Some(v.clone()),
                            "--session" => s.session = Some(v.clone()),
                            "--bind" => s.bind = Some(v.clone()),
                            _ => match v.parse::<u16>() {
                                Ok(p) if p != 0 => s.port = Some(p),
                                _ => s.fail(format!("invalid port '{v}' (expected 1-65535)")),
                            },
                        }
                    }
                    None => s.fail(format!("option '{a}' requires a value")),
                }
            }
            _ => s.fail(format!("unknown option '{a}' (see --help)")),
        }
        i += 1;
    }
    resolve(s)
}

fn resolve(s: Scan) -> Result<Command, String> {
    if s.help {
        return Ok(Command::Help);
    }
    if s.version {
        return Ok(Command::Version);
    }
    if let Some(e) = s.err {
        return Err(e);
    }

    let modes: Vec<&str> = [
        (s.server, "--server"),
        (s.persona_chat, "--persona-chat"),
        (s.list_personas, "--list-personas"),
        (s.list_sessions, "--list-sessions"),
        (s.folder.is_some(), "a folder"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, name)| *name)
    .collect();
    if modes.len() > 1 {
        return Err(format!("{} cannot be combined", modes.join(" and ")));
    }

    if !s.persona_chat {
        if s.persona.is_some() {
            return Err("--persona is only valid with --persona-chat".into());
        }
        if s.session.is_some() {
            return Err("--session is only valid with --persona-chat".into());
        }
    }
    if !s.server {
        if s.bind.is_some() {
            return Err("--bind is only valid with --server".into());
        }
        if s.port.is_some() {
            return Err("--port is only valid with --server".into());
        }
    }

    if s.server {
        if s.chat {
            return Err("--chat needs a folder".into());
        }
        return Ok(Command::Server {
            bind: s.bind,
            port: s.port,
        });
    }
    if s.persona_chat {
        if s.chat {
            return Err("--chat needs a folder".into());
        }
        return Ok(Command::PersonaChat {
            persona: s.persona,
            session: s.session,
        });
    }
    if s.list_personas || s.list_sessions {
        if s.chat {
            return Err("--chat needs a folder".into());
        }
        return Ok(if s.list_personas {
            Command::ListPersonas
        } else {
            Command::ListSessions
        });
    }

    let Some(folder) = s.folder else {
        if s.chat {
            return Err("--chat needs a folder".into());
        }
        return Ok(Command::Gui { folder: None });
    };
    let folder = PathBuf::from(folder);
    let chat_after = s.rest.len() == 1 && s.rest[0] == "--chat";
    if s.chat {
        if !s.rest.is_empty() && !chat_after {
            return Err("--chat takes no message".into());
        }
        return Ok(Command::FolderRepl { folder });
    }
    if s.rest.is_empty() {
        return Ok(Command::Gui {
            folder: Some(folder),
        });
    }
    if chat_after {
        return Ok(Command::FolderRepl { folder });
    }
    let message = s.rest.join(" ");
    if message.trim().is_empty() {
        return Err("message is empty".into());
    }
    Ok(Command::Headless { folder, message })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Command, String> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn no_args_is_gui() {
        assert_eq!(p(&[]), Ok(Command::Gui { folder: None }));
    }

    #[test]
    fn help_and_version() {
        assert_eq!(p(&["-h"]), Ok(Command::Help));
        assert_eq!(p(&["--help"]), Ok(Command::Help));
        assert_eq!(p(&["-v"]), Ok(Command::Version));
        assert_eq!(p(&["--version"]), Ok(Command::Version));
    }

    #[test]
    fn help_wins_over_everything_seen_before_folder() {
        assert_eq!(p(&["--server", "-h", "--port", "x"]), Ok(Command::Help));
        assert_eq!(p(&["--bogus", "-h"]), Ok(Command::Help));
        assert_eq!(p(&["-v", "--bogus"]), Ok(Command::Version));
        assert_eq!(p(&["-v", "-h"]), Ok(Command::Help));
        assert_eq!(p(&["--persona-chat", "--server", "-h"]), Ok(Command::Help));
        assert_eq!(p(&["-h", "dir", "msg"]), Ok(Command::Help));
    }

    #[test]
    fn help_after_folder_is_message() {
        assert_eq!(
            p(&["d", "what", "does", "-h", "do"]),
            Ok(Command::Headless {
                folder: pb("d"),
                message: "what does -h do".into()
            })
        );
        assert_eq!(
            p(&["d", "--version"]),
            Ok(Command::Headless {
                folder: pb("d"),
                message: "--version".into()
            })
        );
    }

    #[test]
    fn folder_only_is_gui() {
        assert_eq!(
            p(&["d"]),
            Ok(Command::Gui {
                folder: Some(pb("d"))
            })
        );
    }

    #[test]
    fn headless_message_joined_verbatim() {
        assert_eq!(
            p(&["d", "list", "the  files", "--chat"]),
            Ok(Command::Headless {
                folder: pb("d"),
                message: "list the  files --chat".into()
            })
        );
        assert_eq!(
            p(&["d", "--chat", "x"]),
            Ok(Command::Headless {
                folder: pb("d"),
                message: "--chat x".into()
            })
        );
    }

    #[test]
    fn empty_message_is_error() {
        assert!(p(&["d", ""]).is_err());
        assert!(p(&["d", " ", ""]).is_err());
    }

    #[test]
    fn folder_repl() {
        assert_eq!(
            p(&["d", "--chat"]),
            Ok(Command::FolderRepl { folder: pb("d") })
        );
        assert_eq!(
            p(&["--chat", "d"]),
            Ok(Command::FolderRepl { folder: pb("d") })
        );
        assert_eq!(
            p(&["--chat", "d", "--chat"]),
            Ok(Command::FolderRepl { folder: pb("d") })
        );
    }

    #[test]
    fn chat_before_folder_rejects_message() {
        assert_eq!(
            p(&["--chat", "d", "hello"]),
            Err("--chat takes no message".into())
        );
    }

    #[test]
    fn chat_without_folder_is_error() {
        assert!(p(&["--chat"]).is_err());
        assert!(p(&["--chat", "--persona-chat"]).is_err());
        assert!(p(&["--chat", "--server"]).is_err());
        assert!(p(&["--chat", "--list-personas"]).is_err());
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(
            p(&["--", "-weird-dir"]),
            Ok(Command::Gui {
                folder: Some(pb("-weird-dir"))
            })
        );
        assert_eq!(
            p(&["--", "-d", "hi", "there"]),
            Ok(Command::Headless {
                folder: pb("-d"),
                message: "hi there".into()
            })
        );
        assert_eq!(p(&["--"]), Ok(Command::Gui { folder: None }));
        assert_eq!(
            p(&["--chat", "--", "-d"]),
            Ok(Command::FolderRepl { folder: pb("-d") })
        );
    }

    #[test]
    fn unknown_option() {
        assert_eq!(
            p(&["--bogus"]),
            Err("unknown option '--bogus' (see --help)".into())
        );
        assert_eq!(
            p(&["-x", "d"]),
            Err("unknown option '-x' (see --help)".into())
        );
        assert!(p(&["-"]).is_err());
    }

    #[test]
    fn first_error_is_reported() {
        assert_eq!(
            p(&["--bogus", "--port", "x"]),
            Err("unknown option '--bogus' (see --help)".into())
        );
    }

    #[test]
    fn persona_chat() {
        assert_eq!(
            p(&["--persona-chat"]),
            Ok(Command::PersonaChat {
                persona: None,
                session: None
            })
        );
        assert_eq!(
            p(&["--persona-chat", "--persona", "Ada", "--session", "abc"]),
            Ok(Command::PersonaChat {
                persona: Some("Ada".into()),
                session: Some("abc".into())
            })
        );
        assert_eq!(
            p(&["--session", "abc", "--persona", "Ada", "--persona-chat"]),
            Ok(Command::PersonaChat {
                persona: Some("Ada".into()),
                session: Some("abc".into())
            })
        );
    }

    #[test]
    fn persona_session_require_persona_chat() {
        assert!(p(&["--persona", "x"]).is_err());
        assert!(p(&["--session", "x"]).is_err());
        assert!(p(&["--server", "--persona", "x"]).is_err());
        assert!(p(&["--persona", "x", "d"]).is_err());
    }

    #[test]
    fn lists() {
        assert_eq!(p(&["--list-personas"]), Ok(Command::ListPersonas));
        assert_eq!(p(&["--list-sessions"]), Ok(Command::ListSessions));
        assert!(p(&["--list-personas", "--list-sessions"]).is_err());
        assert!(p(&["--list-personas", "d"]).is_err());
    }

    #[test]
    fn server() {
        assert_eq!(
            p(&["--server"]),
            Ok(Command::Server {
                bind: None,
                port: None
            })
        );
        assert_eq!(
            p(&["--server", "--bind", "0.0.0.0", "--port", "8080"]),
            Ok(Command::Server {
                bind: Some("0.0.0.0".into()),
                port: Some(8080)
            })
        );
    }

    #[test]
    fn bind_port_require_server() {
        assert!(p(&["--bind", "0.0.0.0"]).is_err());
        assert!(p(&["--port", "80"]).is_err());
        assert!(p(&["--persona-chat", "--port", "80"]).is_err());
        assert!(p(&["--port", "80", "d"]).is_err());
    }

    #[test]
    fn conflicting_modes() {
        assert!(p(&["--server", "--persona-chat"]).is_err());
        assert!(p(&["--server", "d"]).is_err());
        assert!(p(&["--server", "d", "msg"]).is_err());
        assert!(p(&["--persona-chat", "d"]).is_err());
        assert!(p(&["--server", "--list-sessions"]).is_err());
    }

    #[test]
    fn missing_values() {
        assert!(p(&["--persona-chat", "--persona"]).is_err());
        assert!(p(&["--persona-chat", "--session"]).is_err());
        assert!(p(&["--server", "--bind"]).is_err());
        assert!(p(&["--server", "--port"]).is_err());
        // A following option is not a value.
        assert!(p(&["--persona-chat", "--persona", "--session", "x"]).is_err());
    }

    #[test]
    fn bad_ports() {
        for bad in ["abc", "70000", "0", "-1", "", "80x"] {
            assert!(p(&["--server", "--port", bad]).is_err(), "{bad}");
        }
        assert_eq!(
            p(&["--server", "--port", "65535"]),
            Ok(Command::Server {
                bind: None,
                port: Some(65535)
            })
        );
    }
}
