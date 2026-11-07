use std::sync::OnceLock;

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
