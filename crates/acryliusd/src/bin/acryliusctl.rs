//! The control CLI. Talks to the daemon over its `0600` Unix socket with a
//! `SO_PEERCRED` uid check; there is no network equivalent.

use std::path::PathBuf;

use anyhow::Context;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use std::io::{IsTerminal, Write};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser, Debug)]
#[command(
    name = "acryliusctl",
    version,
    about = "Talk to the local acrylius daemon"
)]
struct Args {
    #[arg(long, env = "ACRYLIUS_STATE", global = true)]
    state: Option<PathBuf>,
    /// Print what came back as JSON instead of a table.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Top,
}

// Every `device` positional carries `allow_hyphen_values`: a base64url id can
// start with '-', which clap would otherwise read as a flag.
#[derive(Subcommand, Debug)]
enum Top {
    /// Daemon identity, port and negotiated capabilities.
    Status,
    /// Open a pairing window, or answer one.
    Pair(Pair),
    /// The computers this machine knows.
    Device(Device),
    /// A peer's desktop session.
    Screen(Screen),
    /// A peer's clipboard.
    Clip(Clip),
    /// What is playing on a peer.
    Play(Play),
    /// The commands a peer offers.
    Cmd(CmdGroup),
    /// Files, in either direction.
    File(FileGroup),
    /// Ask a peer to wake a third machine by MAC.
    Wake {
        #[arg(allow_hyphen_values = true)]
        device: String,
        mac: String,
    },
}

#[derive(clap::Args, Debug)]
struct Pair {
    #[command(subcommand)]
    what: Option<PairCmd>,
    /// Accept without asking. For scripts; a human should compare the codes.
    #[arg(long)]
    yes: bool,
}

#[derive(Subcommand, Debug)]
enum PairCmd {
    /// Accept the device currently waiting, after checking the codes match.
    Approve,
    /// Refuse it.
    Deny,
    /// Dial a device and try to pair with it.
    With {
        /// `host:port`.
        addr: String,
        /// Accept without asking. For scripts.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(clap::Args, Debug)]
struct Device {
    #[command(subcommand)]
    what: DeviceCmd,
}

#[derive(Subcommand, Debug)]
enum DeviceCmd {
    /// Paired devices.
    List,
    /// Machines on this network that are not paired with.
    Nearby,
    /// Forget one. Its next connection is a stranger's.
    Forget {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    /// Open a session, and say why if it will not open.
    Connect {
        #[arg(allow_hyphen_values = true)]
        device: String,
        /// Skip discovery and dial this `host:port` directly, for a network
        /// that filters mDNS.
        #[arg(long)]
        addr: Option<String>,
    },
    /// Round-trip a ping.
    Ping {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
}

#[derive(clap::Args, Debug)]
struct Screen {
    #[command(subcommand)]
    what: ScreenCmd,
}

#[derive(Subcommand, Debug)]
enum ScreenCmd {
    /// Is it locked?
    Query {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Lock {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Unlock {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
}

impl ScreenCmd {
    fn parts(self) -> (String, &'static str) {
        match self {
            Self::Query { device } => (device, "query"),
            Self::Lock { device } => (device, "lock"),
            Self::Unlock { device } => (device, "unlock"),
        }
    }
}

#[derive(clap::Args, Debug)]
struct Clip {
    #[command(subcommand)]
    what: ClipCmd,
}

#[derive(Subcommand, Debug)]
enum ClipCmd {
    /// Read a peer's clipboard.
    Get {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    /// Send it text.
    Put {
        #[arg(allow_hyphen_values = true)]
        device: String,
        text: String,
    },
}

/// Control what is playing on a peer. A command naming no player goes to the
/// active one, which `play status` marks with a `*`.
#[derive(clap::Args, Debug)]
struct Play {
    #[command(subcommand)]
    what: PlayCmd,
    /// Which player, by the id shown in `play status`.
    #[arg(long, global = true)]
    player: Option<String>,
}

#[derive(Subcommand, Debug)]
enum PlayCmd {
    /// What is playing, and on what.
    Status {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Play {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Pause {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    /// Whichever of the two it is not doing.
    Toggle {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Next {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Previous {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    Stop {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    /// Move by a number of milliseconds, which may be negative.
    Seek {
        #[arg(allow_hyphen_values = true)]
        device: String,
        #[arg(allow_hyphen_values = true)]
        offset_ms: i64,
    },
    /// Jump to a millisecond.
    Position {
        #[arg(allow_hyphen_values = true)]
        device: String,
        ms: i64,
    },
    /// Set the volume, 0 to 100. With no `--player` this moves the machine's
    /// own output, not a player's.
    Volume {
        #[arg(allow_hyphen_values = true)]
        device: String,
        percent: i64,
    },
}

impl PlayCmd {
    fn parts(self) -> (String, &'static str, Option<i64>) {
        match self {
            Self::Status { device } => (device, "query", None),
            Self::Play { device } => (device, "play", None),
            Self::Pause { device } => (device, "pause", None),
            Self::Toggle { device } => (device, "playpause", None),
            Self::Next { device } => (device, "next", None),
            Self::Previous { device } => (device, "previous", None),
            Self::Stop { device } => (device, "stop", None),
            Self::Seek { device, offset_ms } => (device, "seek", Some(offset_ms)),
            Self::Position { device, ms } => (device, "position", Some(ms)),
            Self::Volume { device, percent } => (device, "volume", Some(percent)),
        }
    }
}

#[derive(clap::Args, Debug)]
struct CmdGroup {
    #[command(subcommand)]
    what: CmdCmd,
}

#[derive(Subcommand, Debug)]
enum CmdCmd {
    /// What a peer is willing to run.
    List {
        #[arg(allow_hyphen_values = true)]
        device: String,
    },
    /// Run one of them, by the id `cmd list` shows.
    Run {
        #[arg(allow_hyphen_values = true)]
        device: String,
        id: String,
    },
}

#[derive(clap::Args, Debug)]
struct FileGroup {
    #[command(subcommand)]
    what: FileCmd,
}

#[derive(Subcommand, Debug)]
enum FileCmd {
    /// Offer a file to a peer. It decides whether to take it.
    Send {
        #[arg(allow_hyphen_values = true)]
        device: String,
        path: String,
    },
    /// Files offered to this machine that nobody has answered yet.
    Offers,
    /// Take one.
    Accept { transfer: u64 },
    /// Refuse one.
    Reject { transfer: u64 },
}

use acryliusd::ipc::{Request, Response};
use acryliusd::tr;

const HELP_RU: acryliusd::lang::HelpTable = &[
    (
        "Talk to the local acrylius daemon",
        "Управление локальным демоном acrylius",
    ),
    (
        "Print what came back as JSON instead of a table",
        "Выводить ответ в JSON, а не таблицей",
    ),
    (
        "Daemon identity, port and negotiated capabilities",
        "Идентификатор демона, порт и согласованные возможности",
    ),
    (
        "Open a pairing window, or answer one",
        "Начать сопряжение или ответить на запрос",
    ),
    (
        "The computers this machine knows",
        "Устройства, известные этому компьютеру",
    ),
    (
        "A peer's desktop session",
        "Сеанс рабочего стола устройства",
    ),
    ("A peer's clipboard", "Буфер обмена устройства"),
    ("What is playing on a peer", "Что играет на устройстве"),
    (
        "The commands a peer offers",
        "Команды, которые предлагает устройство",
    ),
    ("Files, in either direction", "Файлы в обе стороны"),
    (
        "Ask a peer to wake a third machine by MAC",
        "Попросить устройство разбудить другой компьютер по MAC",
    ),
    (
        "Accept without asking. For scripts; a human should compare the codes",
        "Принять без вопроса. Для скриптов; человеку стоит сверить коды",
    ),
    (
        "Accept the device currently waiting, after checking the codes match",
        "Принять ожидающее устройство, сверив коды",
    ),
    ("Refuse it", "Отклонить его"),
    (
        "Dial a device and try to pair with it",
        "Подключиться к устройству и выполнить сопряжение",
    ),
    ("`host:port`", "`хост:порт`"),
    (
        "Accept without asking. For scripts",
        "Принять без вопроса. Для скриптов",
    ),
    ("Paired devices", "Сопряжённые устройства"),
    (
        "Machines on this network that are not paired with",
        "Несопряжённые компьютеры в этой сети",
    ),
    (
        "Forget one. Its next connection is a stranger's",
        "Забыть устройство. Следующее подключение будет как от чужого",
    ),
    (
        "Open a session, and say why if it will not open",
        "Открыть соединение и объяснить, если оно не открывается",
    ),
    (
        "Skip discovery and dial this `host:port` directly, for a network that filters mDNS",
        "Не искать, а подключиться прямо к `хост:порт`, для сети, где режется mDNS",
    ),
    ("Round-trip a ping", "Отправить ping и дождаться ответа"),
    ("Is it locked?", "Заблокирован ли экран?"),
    (
        "Read a peer's clipboard",
        "Прочитать буфер обмена устройства",
    ),
    ("Send it text", "Отправить в него текст"),
    (
        "Which player, by the id shown in `play status`",
        "Плеер по id из `play status`",
    ),
    ("What is playing, and on what", "Что играет и где"),
    (
        "Whichever of the two it is not doing",
        "Переключить воспроизведение и паузу",
    ),
    (
        "Move by a number of milliseconds, which may be negative",
        "Сдвинуть на число миллисекунд, можно отрицательное",
    ),
    ("Jump to a millisecond", "Перейти к миллисекунде"),
    (
        "Set the volume, 0 to 100. With no `--player` this moves the machine's own output, not a player's",
        "Задать громкость от 0 до 100. Без `--player` меняет громкость системы, а не плеера",
    ),
    (
        "What a peer is willing to run",
        "Что устройство готово запустить",
    ),
    (
        "Run one of them, by the id `cmd list` shows",
        "Запустить команду по id из `cmd list`",
    ),
    (
        "Offer a file to a peer. It decides whether to take it",
        "Предложить файл устройству. Оно решает, принять ли его",
    ),
    (
        "Files offered to this machine that nobody has answered yet",
        "Файлы, предложенные этому компьютеру и ещё без ответа",
    ),
    ("Take one", "Принять"),
    ("Refuse one", "Отклонить"),
];

/// `println!` panics when its pipe closes (Rust ignores SIGPIPE); dropping the
/// write error ends output quietly instead.
macro_rules! outln {
    () => {{ let _ = writeln!(std::io::stdout()); }};
    ($($arg:tt)*) => {{ let _ = writeln!(std::io::stdout(), $($arg)*); }};
}

/// Mirrors the daemon's rule: an explicitly named state directory keeps its
/// socket beside it.
fn socket_path(state: Option<PathBuf>) -> PathBuf {
    if let Some(s) = state {
        return s.join("acrylius.sock");
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("acrylius.sock")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let matches = acryliusd::lang::localize_help(Args::command(), HELP_RU).get_matches();
    let args = Args::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let path = socket_path(args.state);
    let stream = UnixStream::connect(&path).await.with_context(|| {
        tr!(
            "no daemon at {}; is acryliusd running?",
            "демон не найден по адресу {}; acryliusd запущен?",
            path.display()
        )
    })?;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let mut assume_yes = false;
    let (req, streaming) = match args.cmd {
        Top::Status => (Request::Status, false),
        Top::Pair(p) => match p.what {
            None => {
                assume_yes = p.yes;
                (Request::Pair, true)
            }
            Some(PairCmd::Approve) => (Request::Approve, false),
            Some(PairCmd::Deny) => (Request::Deny, false),
            Some(PairCmd::With { addr, yes }) => {
                assume_yes = yes;
                (Request::PairWith { addr }, true)
            }
        },
        Top::Device(d) => match d.what {
            DeviceCmd::List => (Request::Devices, false),
            DeviceCmd::Nearby => (Request::Nearby, false),
            DeviceCmd::Forget { device } => (Request::Revoke { device }, false),
            DeviceCmd::Connect { device, addr } => (Request::Connect { device, addr }, false),
            DeviceCmd::Ping { device } => (Request::Ping { device }, false),
        },
        Top::Screen(s) => {
            let (device, action) = s.what.parts();
            (
                Request::Session {
                    device,
                    action: action.to_string(),
                },
                false,
            )
        }
        Top::Clip(c) => match c.what {
            ClipCmd::Get { device } => (Request::Clipboard { device, push: None }, false),
            ClipCmd::Put { device, text } => (
                Request::Clipboard {
                    device,
                    push: Some(text),
                },
                false,
            ),
        },
        Top::Play(p) => {
            let (device, action, value) = p.what.parts();
            (
                Request::Media {
                    device,
                    action: action.to_string(),
                    player: p.player,
                    value,
                },
                false,
            )
        }
        Top::Cmd(c) => match c.what {
            CmdCmd::List { device } => (Request::Commands { device }, false),
            CmdCmd::Run { device, id } => (Request::Run { device, id }, false),
        },
        Top::File(f) => match f.what {
            FileCmd::Send { device, path } => (
                Request::Send {
                    device,
                    // Resolved here: the daemon's working directory is `/` under systemd.
                    path: absolute(&path)?,
                },
                false,
            ),
            FileCmd::Offers => (Request::Offers, false),
            FileCmd::Accept { transfer } => (
                Request::Answer {
                    transfer,
                    accept: true,
                },
                false,
            ),
            FileCmd::Reject { transfer } => (
                Request::Answer {
                    transfer,
                    accept: false,
                },
                false,
            ),
        },
        Top::Wake { device, mac } => (Request::Wake { device, mac }, false),
    };

    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    wr.write_all(line.as_bytes()).await?;

    let mut failed = false;
    while let Some(l) = lines.next_line().await? {
        let response: Response = serde_json::from_str(&l)?;
        if args.json {
            outln!("{}", serde_json::to_string(&response)?);
            if let Response::Error { .. } = response {
                failed = true;
            }
            if streaming {
                continue;
            }
            break;
        }
        match response {
            Response::Ok => outln!("{}", tr!("ok", "готово")),
            Response::Report { report } => outln!("{}", report.render()),
            Response::Status(s) => {
                outln!("{}  {}", s.name, s.device_id);
                outln!(
                    "{}",
                    tr!("  fingerprint  {}", "  отпечаток    {}", s.fingerprint)
                );
                outln!("{}", tr!("  port         {}", "  порт         {}", s.port));
                outln!("{}", tr!("  peers        {}", "  устройства   {}", s.peers));
                outln!(
                    "{}",
                    tr!("  accepts      {}", "  принимает    {}", join(&s.caps_in))
                );
                outln!(
                    "{}",
                    tr!("  sends        {}", "  отправляет   {}", join(&s.caps_out))
                );
            }
            Response::Devices { devices } if devices.is_empty() => {
                outln!("{}", tr!("no paired devices", "нет сопряжённых устройств"));
            }
            Response::Devices { devices } => {
                for x in devices {
                    let reach = if x.reachable {
                        tr!("reachable", "доступно")
                    } else {
                        tr!("unreachable", "недоступно")
                    };
                    outln!("{}  {} ({})  {reach}", x.device_id, x.name, x.platform);
                    outln!(
                        "{}",
                        tr!("  fingerprint {}", "  отпечаток {}", x.fingerprint)
                    );
                }
            }
            Response::Nearby { nearby } if nearby.is_empty() => {
                outln!(
                    "{}",
                    tr!(
                        "nothing nearby that is not already paired",
                        "рядом нет несопряжённых устройств"
                    )
                );
            }
            Response::Nearby { nearby } => {
                for x in nearby {
                    let busy = if x.pairing {
                        tr!("  (busy pairing)", "  (занято сопряжением)")
                    } else {
                        String::new()
                    };
                    outln!("{}  {}{busy}", x.addr, x.name);
                    outln!(
                        "{}",
                        tr!("  fingerprint {}", "  отпечаток {}", x.fingerprint)
                    );
                }
                outln!();
                outln!(
                    "{}",
                    tr!(
                        "Pair with one: acryliusctl pair with <address>",
                        "Сопряжение: acryliusctl pair with <адрес>"
                    )
                );
            }
            Response::Event { text } => outln!("{text}"),
            Response::Confirm {
                name,
                fingerprint,
                sas,
            } => {
                outln!();
                outln!(
                    "{}",
                    tr!(
                        "{name} wants to pair.",
                        "{name} хочет выполнить сопряжение."
                    )
                );
                outln!(
                    "{}",
                    tr!("  fingerprint  {fingerprint}", "  отпечаток  {fingerprint}")
                );
                outln!();
                outln!(
                    "{}",
                    tr!(
                        "  It should be showing:  {sas}",
                        "  На экране должно быть:  {sas}"
                    )
                );
                outln!();
                let accept = if assume_yes {
                    outln!("{}", tr!("  accepting (--yes)", "  принимаю (--yes)"));
                    true
                } else if std::io::stdin().is_terminal() {
                    prompt(&tr!(
                        "  Do the codes match? [Y/n] ",
                        "  Коды совпадают? [Y/n] "
                    ))
                    .await?
                } else {
                    // Piped or backgrounded: nobody to ask, so say what to run
                    // instead of blocking on a read that will never return.
                    outln!(
                        "{}",
                        tr!(
                            "  Not a terminal, so nothing to ask.",
                            "  Это не терминал, спросить некого."
                        )
                    );
                    outln!(
                        "{}",
                        tr!(
                            "  Run `acryliusctl pair approve` (or `pair deny`) to answer.",
                            "  Ответьте командой `acryliusctl pair approve` (или `pair deny`)."
                        )
                    );
                    continue;
                };
                // A fresh connection: this one is busy streaming the pairing.
                answer(&path, accept).await?;
                if !accept {
                    failed = true;
                }
            }
            Response::Error { message } => {
                eprintln!("{}", tr!("error: {message}", "ошибка: {message}"));
                failed = true;
            }
        }
        if !streaming {
            break;
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// Ask a yes/no question; anything but an explicit "n" is a yes.
async fn prompt(question: &str) -> anyhow::Result<bool> {
    use tokio::io::AsyncBufReadExt;
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "{question}");
    let _ = stdout.flush();
    let mut line = String::new();
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    reader.read_line(&mut line).await?;
    Ok(!is_no(&line))
}

/// "н" starts "нет"; "т" is the N key on a Russian layout. Either refuses the pairing.
fn is_no(answer: &str) -> bool {
    answer
        .trim()
        .chars()
        .next()
        .is_some_and(|c| matches!(c.to_lowercase().next(), Some('n' | 'н' | 'т')))
}

async fn answer(path: &std::path::Path, accept: bool) -> anyhow::Result<()> {
    let stream = UnixStream::connect(path).await?;
    let (rd, mut wr) = stream.into_split();
    let req = if accept {
        Request::Approve
    } else {
        Request::Deny
    };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    wr.write_all(line.as_bytes()).await?;
    // Read the acknowledgement so the daemon is not writing into a closed pipe.
    let mut lines = BufReader::new(rd).lines();
    let _ = lines.next_line().await?;
    Ok(())
}

/// Expand `~` (shells only expand it unquoted), then make the path absolute
/// against this process's working directory. Symlinks are left alone.
fn absolute(path: &str) -> anyhow::Result<String> {
    let home = std::env::var_os("HOME").unwrap_or_default();
    Ok(resolve(path, home.as_ref(), &std::env::current_dir()?)
        .to_string_lossy()
        .into_owned())
}

fn resolve(path: &str, home: &std::ffi::OsStr, cwd: &std::path::Path) -> PathBuf {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(home).join(rest),
        None => PathBuf::from(path),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

fn join(v: &[String]) -> String {
    if v.is_empty() {
        tr!("(none)", "(нет)")
    } else {
        v.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn resolved(path: &str) -> PathBuf {
        resolve(
            path,
            std::ffi::OsStr::new("/home/someone"),
            std::path::Path::new("/home/someone/photos"),
        )
    }

    #[test]
    fn every_help_text_has_a_russian_one() {
        let missing = acryliusd::lang::untranslated_help(Args::command(), HELP_RU);
        assert!(missing.is_empty(), "no Russian for: {missing:#?}");
    }

    #[test]
    fn only_an_explicit_no_refuses() {
        for no in ["n", "No\n", " нет", "Н", "т"] {
            assert!(is_no(no), "{no:?}");
        }
        for yes in ["", "\n", "y", "yes", "да", "д"] {
            assert!(!is_no(yes), "{yes:?}");
        }
    }

    #[test]
    fn a_path_is_resolved_where_it_was_typed() {
        assert_eq!(
            resolved("holiday.jpg"),
            PathBuf::from("/home/someone/photos/holiday.jpg")
        );
    }

    #[test]
    fn an_absolute_path_is_left_alone() {
        assert_eq!(resolved("/etc/hostname"), PathBuf::from("/etc/hostname"));
    }

    #[test]
    fn a_tilde_becomes_the_home_directory() {
        assert_eq!(
            resolved("~/Pictures/a b.png"),
            PathBuf::from("/home/someone/Pictures/a b.png")
        );
        // `~other` stays literal rather than becoming a wrong guess.
        assert_eq!(
            resolved("~other/file"),
            PathBuf::from("/home/someone/photos/~other/file")
        );
    }

    /// A base64url id can start with `-`, which clap reads as a flag unless the
    /// positional says otherwise.
    const HYPHEN_ID: &str = "-4IEPRyU7ZslU335_83cWw";

    fn parse(args: &[&str]) -> Args {
        let mut argv = vec!["acryliusctl"];
        argv.extend_from_slice(args);
        Args::try_parse_from(argv).expect("a device id starting with '-' must parse")
    }

    /// The device this invocation names, whichever group it went through.
    fn device_of(a: Args) -> Option<String> {
        Some(match a.cmd {
            Top::Device(d) => match d.what {
                DeviceCmd::Forget { device }
                | DeviceCmd::Connect { device, .. }
                | DeviceCmd::Ping { device } => device,
                DeviceCmd::List | DeviceCmd::Nearby => return None,
            },
            Top::Screen(s) => s.what.parts().0,
            Top::Clip(c) => match c.what {
                ClipCmd::Get { device } | ClipCmd::Put { device, .. } => device,
            },
            Top::Play(p) => p.what.parts().0,
            Top::Cmd(c) => match c.what {
                CmdCmd::List { device } | CmdCmd::Run { device, .. } => device,
            },
            Top::File(f) => match f.what {
                FileCmd::Send { device, .. } => device,
                _ => return None,
            },
            Top::Wake { device, .. } => device,
            _ => return None,
        })
    }

    #[test]
    fn every_device_positional_accepts_a_leading_hyphen() {
        for argv in [
            vec!["device", "forget", HYPHEN_ID],
            vec!["device", "ping", HYPHEN_ID],
            vec!["device", "connect", HYPHEN_ID],
            vec!["screen", "query", HYPHEN_ID],
            vec!["screen", "lock", HYPHEN_ID],
            vec!["clip", "get", HYPHEN_ID],
            vec!["clip", "put", HYPHEN_ID, "hello"],
            vec!["play", "status", HYPHEN_ID],
            vec!["play", "volume", HYPHEN_ID, "40"],
            vec!["cmd", "list", HYPHEN_ID],
            vec!["cmd", "run", HYPHEN_ID, "screenshot"],
            vec!["file", "send", HYPHEN_ID, "/tmp/x"],
            vec!["wake", HYPHEN_ID, "00:11:22:33:44:55"],
        ] {
            assert_eq!(
                device_of(parse(&argv)).as_deref(),
                Some(HYPHEN_ID),
                "{argv:?} lost the device id"
            );
        }
    }

    #[test]
    fn a_negative_seek_is_an_offset_and_not_a_flag() {
        let a = parse(&["play", "seek", HYPHEN_ID, "-5000"]);
        let Top::Play(p) = a.cmd else {
            panic!("wrong subcommand")
        };
        assert_eq!(p.what.parts(), (HYPHEN_ID.to_string(), "seek", Some(-5000)));
    }

    #[test]
    fn options_still_parse_alongside_such_an_id() {
        let a = parse(&["device", "connect", HYPHEN_ID, "--addr", "127.0.0.1:1971"]);
        let Top::Device(d) = a.cmd else {
            panic!("wrong subcommand")
        };
        let DeviceCmd::Connect { device, addr } = d.what else {
            panic!("wrong subcommand")
        };
        assert_eq!(device, HYPHEN_ID);
        assert_eq!(addr.as_deref(), Some("127.0.0.1:1971"));
    }

    #[test]
    fn a_genuinely_unknown_flag_is_still_refused() {
        assert!(
            Args::try_parse_from(["acryliusctl", "device", "ping", HYPHEN_ID, "--nonsense"])
                .is_err()
        );
    }
}
