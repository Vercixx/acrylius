use std::sync::OnceLock;

use clap::Command;
use clap::builder::StyledStr;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Ru,
}

/// POSIX precedence: the first non-empty of `LC_ALL`, `LC_MESSAGES`, `LANG` decides.
#[must_use]
pub fn detect(lc_all: Option<&str>, lc_messages: Option<&str>, lang: Option<&str>) -> Lang {
    let locale = [lc_all, lc_messages, lang]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .unwrap_or("");
    match locale.split(['_', '.', '@']).next() {
        Some("ru") => Lang::Ru,
        _ => Lang::En,
    }
}

#[must_use]
pub fn lang() -> Lang {
    static LANG: OnceLock<Lang> = OnceLock::new();
    *LANG.get_or_init(|| {
        let var = |k| std::env::var(k).ok();
        detect(
            var("LC_ALL").as_deref(),
            var("LC_MESSAGES").as_deref(),
            var("LANG").as_deref(),
        )
    })
}

/// Picks the Russian plural form for `n`: 1 файл, 2 файла, 5 файлов.
#[must_use]
pub fn ru_plural<'a>(n: u64, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    match (n % 10, n % 100) {
        (1, r) if r != 11 => one,
        (2..=4, r) if !(12..=14).contains(&r) => few,
        _ => many,
    }
}

pub type HelpTable = &'static [(&'static str, &'static str)];

const CLAP_BUILTIN: HelpTable = &[
    ("Print help", "Показать справку"),
    (
        "Print help (see more with '--help')",
        "Показать справку (подробнее: --help)",
    ),
    (
        "Print help (see a summary with '-h')",
        "Показать справку (кратко: -h)",
    ),
    ("Print version", "Показать версию"),
    (
        "Print this message or the help of the given subcommand(s)",
        "Показать эту справку или справку по командам",
    ),
    (
        "Print help for the subcommand(s)",
        "Показать справку по командам",
    ),
];

fn lookup(table: HelpTable, text: &StyledStr) -> Option<&'static str> {
    let text = text.to_string();
    table
        .iter()
        .chain(CLAP_BUILTIN)
        .find(|(en, _)| *en == text)
        .map(|(_, ru)| *ru)
}

/// Swaps clap's English help for `table`'s Russian under a Russian locale.
/// Text missing from `table` stays English; `untranslated_help` finds it.
#[must_use]
pub fn localize_help(cmd: Command, table: HelpTable) -> Command {
    localize_help_in(lang(), cmd, table)
}

fn localize_help_in(lang: Lang, mut cmd: Command, table: HelpTable) -> Command {
    if lang != Lang::Ru {
        return cmd;
    }
    cmd.build();
    russian_help(cmd, table)
}

fn russian_help(cmd: Command, table: HelpTable) -> Command {
    let about = cmd.get_about().and_then(|s| lookup(table, s));
    let long_about = cmd.get_long_about().and_then(|s| lookup(table, s));
    let mut cmd = cmd
        .help_template(
            "{before-help}{about-with-newline}\nИспользование: {usage}\n\n{all-args}{after-help}",
        )
        .subcommand_help_heading("Команды")
        .mut_args(|arg| {
            let help = arg.get_help().and_then(|s| lookup(table, s));
            let long_help = arg.get_long_help().and_then(|s| lookup(table, s));
            let heading = if arg.is_positional() {
                "Аргументы"
            } else {
                "Параметры"
            };
            let mut arg = arg.help_heading(heading);
            if let Some(help) = help {
                arg = arg.help(help);
            }
            if let Some(long_help) = long_help {
                arg = arg.long_help(long_help);
            }
            arg
        })
        .mut_subcommands(|sub| russian_help(sub, table));
    if let Some(about) = about {
        cmd = cmd.about(about);
    }
    if let Some(long_about) = long_about {
        cmd = cmd.long_about(long_about);
    }
    cmd
}

/// Every help text in `cmd`'s tree that `table` has no Russian for.
#[must_use]
pub fn untranslated_help(mut cmd: Command, table: HelpTable) -> Vec<String> {
    cmd.build();
    let mut missing = Vec::new();
    collect_untranslated(&cmd, table, &mut missing);
    missing.sort();
    missing.dedup();
    missing
}

fn collect_untranslated(cmd: &Command, table: HelpTable, missing: &mut Vec<String>) {
    let texts = [cmd.get_about(), cmd.get_long_about()]
        .into_iter()
        .chain(
            cmd.get_arguments()
                .flat_map(|a| [a.get_help(), a.get_long_help()]),
        )
        .flatten();
    for text in texts {
        if lookup(table, text).is_none() {
            missing.push(text.to_string());
        }
    }
    for sub in cmd.get_subcommands() {
        collect_untranslated(sub, table, missing);
    }
}

/// `cfg!(test)` expands in the calling crate, so every crate's unit tests see English.
#[macro_export]
macro_rules! tr {
    ($en:literal, $ru:literal $(, $arg:expr)* $(,)?) => {
        if !cfg!(test) && $crate::lang::lang() == $crate::lang::Lang::Ru {
            format!($ru $(, $arg)*)
        } else {
            format!($en $(, $arg)*)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Arg;

    const TABLE: HelpTable = &[
        ("Talk to it", "Поговорить"),
        ("Be loud", "Громко"),
        ("Leave", "Уйти"),
    ];

    fn sample() -> Command {
        Command::new("demo")
            .about("Talk to it")
            .arg(Arg::new("loud").long("loud").help("Be loud").num_args(0))
            .arg(Arg::new("quiet").long("quiet").help("Be quiet").num_args(0))
            .subcommand(Command::new("go").about("Leave"))
    }

    fn rendered(lang: Lang, args: &[&str]) -> String {
        let err = localize_help_in(lang, sample(), TABLE)
            .try_get_matches_from(args)
            .unwrap_err();
        err.render().to_string()
    }

    #[test]
    fn russian_help_replaces_known_text_and_keeps_the_rest() {
        let help = rendered(Lang::Ru, &["demo", "--help"]);
        for want in [
            "Поговорить",
            "Громко",
            "Уйти",
            "Be quiet",
            "Показать справку",
            "Использование:",
            "Команды:",
            "Параметры:",
        ] {
            assert!(help.contains(want), "{want:?} missing from:\n{help}");
        }
        assert!(
            !help.contains("Be loud") && !help.contains("Usage:"),
            "{help}"
        );
        let sub = rendered(Lang::Ru, &["demo", "go", "--help"]);
        assert!(
            sub.contains("Уйти") && sub.contains("Использование:"),
            "{sub}"
        );
    }

    #[test]
    fn english_help_is_left_alone() {
        let help = rendered(Lang::En, &["demo", "--help"]);
        assert!(
            help.contains("Be loud") && help.contains("Usage:"),
            "{help}"
        );
        assert!(!help.contains("Громко"), "{help}");
    }

    #[test]
    fn untranslated_help_names_only_what_is_missing() {
        assert_eq!(untranslated_help(sample(), TABLE), ["Be quiet"]);
    }

    #[test]
    fn the_first_set_variable_decides() {
        assert_eq!(detect(Some("C"), Some("ru_RU.UTF-8"), None), Lang::En);
        assert_eq!(detect(Some(""), Some("ru_RU.UTF-8"), Some("C")), Lang::Ru);
        assert_eq!(detect(None, None, Some("ru_RU.UTF-8")), Lang::Ru);
        assert_eq!(detect(None, Some("en_US.UTF-8"), Some("ru_RU")), Lang::En);
        assert_eq!(detect(None, None, None), Lang::En);
    }

    #[test]
    fn only_russian_locales_count_as_russian() {
        for ru in ["ru", "ru_RU", "ru_UA.UTF-8", "ru.UTF-8", "ru@latin"] {
            assert_eq!(detect(None, None, Some(ru)), Lang::Ru, "{ru}");
        }
        for other in ["run", "rue_XX", "C.UTF-8", "POSIX", "en_RU.UTF-8"] {
            assert_eq!(detect(None, None, Some(other)), Lang::En, "{other}");
        }
    }

    #[test]
    fn russian_plurals_follow_the_last_two_digits() {
        let form = |n| ru_plural(n, "one", "few", "many");
        for (n, want) in [
            (0, "many"),
            (1, "one"),
            (2, "few"),
            (4, "few"),
            (5, "many"),
            (11, "many"),
            (12, "many"),
            (14, "many"),
            (21, "one"),
            (22, "few"),
            (25, "many"),
            (101, "one"),
            (111, "many"),
            (112, "many"),
        ] {
            assert_eq!(form(n), want, "{n}");
        }
    }
}
