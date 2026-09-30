//! `~/.config/dv/config.toml`: named themes and mode settings.

use std::io;
use std::path::{Path, PathBuf};

use ratatui::style::{Color, Modifier, Style};
use thiserror::Error;
use toml::{Table, Value};

use crate::ui::theme::Theme;

/// Settings from the config file; `None` keeps the built-in value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub theme: Theme,
    /// Files larger than this open in streaming mode (`mode.threshold`).
    pub threshold: Option<u64>,
    /// Memory for streaming mode's caches and buffers (`mode.memory_budget`).
    pub memory_budget: Option<u64>,
    /// The rule and key-hint rows under the tree (`ui.footer`).
    pub footer: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            threshold: None,
            memory_budget: None,
            footer: true,
        }
    }
}

/// Why a config file was rejected; always one line, for the status bar.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    #[error("line {line}: {message}")]
    Syntax { line: usize, message: String },
    #[error("unknown theme {0:?}")]
    UnknownTheme(String),
    #[error("unknown theme token {0:?}")]
    UnknownToken(String),
    #[error("{token}: unknown color {word:?}")]
    BadColor { token: String, word: String },
    #[error("{0}: expected a string")]
    NotText(String),
    #[error("{0}: expected a table")]
    NotTable(String),
    #[error("{0}: expected a size such as 256MB")]
    BadSize(String),
    #[error("{0}: expected true or false")]
    NotBool(String),
}

/// `$XDG_CONFIG_HOME/dv/config.toml`, else `~/.config/dv/config.toml`.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    let home = || std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"));
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(home)?;
    Some(dir.join("dv").join("config.toml"))
}

/// The config at `path`: the default when missing, or the default plus a warning when invalid.
#[must_use]
pub fn load(path: &Path) -> (Config, Option<String>) {
    let parsed = match std::fs::read_to_string(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return (Config::default(), None),
        Err(err) => Err(err.to_string()),
        Ok(text) => parse(&text).map_err(|err| err.to_string()),
    };
    match parsed {
        Ok(config) => (config, None),
        Err(message) => (Config::default(), Some(format!("config: {message}"))),
    }
}

/// Parses a config file's text.
///
/// # Errors
/// Invalid TOML, unknown themes or tokens, bad colors or sizes.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let table: Table = text.parse().map_err(|err| syntax(text, &err))?;
    let name = match table.get("theme") {
        None => "default",
        Some(Value::String(name)) => name.as_str(),
        Some(_) => return Err(ConfigError::NotText("theme".to_owned())),
    };
    let mode = table.get("mode").and_then(Value::as_table);
    let ui = table.get("ui").and_then(Value::as_table);
    Ok(Config {
        theme: theme_named(table.get("themes"), name)?,
        threshold: size(mode, "threshold")?,
        memory_budget: size(mode, "memory_budget")?,
        footer: flag(ui, "footer", true)?,
    })
}

/// A TOML error as `line N: message`.
fn syntax(text: &str, err: &toml::de::Error) -> ConfigError {
    let at = err.span().map_or(0, |span| span.start).min(text.len());
    ConfigError::Syntax {
        line: text[..at].matches('\n').count() + 1,
        message: err.message().replace('\n', " "),
    }
}

/// Every theme is checked; `name` is picked ("default" needs no table).
fn theme_named(themes: Option<&Value>, name: &str) -> Result<Theme, ConfigError> {
    let themes = match themes {
        None => &Table::new(),
        Some(Value::Table(themes)) => themes,
        Some(_) => return Err(ConfigError::NotTable("themes".to_owned())),
    };
    let mut picked = (name == "default").then(Theme::default);
    for (theme_name, tokens) in themes {
        let Value::Table(tokens) = tokens else {
            return Err(ConfigError::NotTable(format!("themes.{theme_name}")));
        };
        let theme = theme_from(tokens)?;
        if theme_name == name {
            picked = Some(theme);
        }
    }
    picked.ok_or_else(|| ConfigError::UnknownTheme(name.to_owned()))
}

/// The default theme with the given tokens restyled.
fn theme_from(tokens: &Table) -> Result<Theme, ConfigError> {
    tokens
        .iter()
        .try_fold(Theme::default(), |theme, (token, value)| {
            let text = value
                .as_str()
                .ok_or_else(|| ConfigError::NotText(token.clone()))?;
            let style = parse_style(text).map_err(|word| ConfigError::BadColor {
                token: token.clone(),
                word,
            })?;
            with_token(theme, token, style)
        })
}

fn with_token(mut theme: Theme, token: &str, style: Style) -> Result<Theme, ConfigError> {
    let slot = match token {
        "key" => &mut theme.key,
        "string" => &mut theme.string,
        "number" => &mut theme.number,
        "bool" => &mut theme.bool,
        "null" => &mut theme.null,
        "punctuation" => &mut theme.punct,
        "badge" => &mut theme.badge,
        "marker" => &mut theme.marker,
        "selection" => &mut theme.selection,
        "error" => &mut theme.error,
        _ => return Err(ConfigError::UnknownToken(token.to_owned())),
    };
    *slot = style;
    Ok(theme)
}

/// `mode.<key>` as bytes: an integer or a string such as `"256MB"`.
fn size(mode: Option<&Table>, key: &str) -> Result<Option<u64>, ConfigError> {
    let bad = || ConfigError::BadSize(format!("mode.{key}"));
    match mode.and_then(|mode| mode.get(key)) {
        None => Ok(None),
        Some(Value::Integer(n)) => u64::try_from(*n).map(Some).map_err(|_| bad()),
        Some(Value::String(text)) => parse_size(text).map(Some).ok_or_else(bad),
        Some(_) => Err(bad()),
    }
}

/// `ui.<key>` as a boolean, `default` when unset.
fn flag(ui: Option<&Table>, key: &str, default: bool) -> Result<bool, ConfigError> {
    match ui.and_then(|ui| ui.get(key)) {
        None => Ok(default),
        Some(Value::Boolean(on)) => Ok(*on),
        Some(_) => Err(ConfigError::NotBool(format!("ui.{key}"))),
    }
}

/// `1234`, `256MB`, `2GB` (decimal units, case-insensitive, `B` optional).
#[must_use]
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let factor = match text[digits..].trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" => 1_000,
        "M" | "MB" => 1_000_000,
        "G" | "GB" => 1_000_000_000,
        _ => return None,
    };
    text[..digits].parse::<u64>().ok()?.checked_mul(factor)
}

/// Space-separated words: colors (foreground), `bg:<color>`, and modifiers; `Err` is the bad word.
///
/// # Errors
/// The first word that is neither a color nor a modifier.
pub fn parse_style(text: &str) -> Result<Style, String> {
    text.split_whitespace()
        .try_fold(Style::new(), |style, word| {
            style_word(style, word).ok_or_else(|| word.to_owned())
        })
}

fn style_word(style: Style, word: &str) -> Option<Style> {
    if let Some(bg) = word.strip_prefix("bg:") {
        return color(bg).map(|c| style.bg(c));
    }
    modifier(word)
        .map(|m| style.add_modifier(m))
        .or_else(|| color(word).map(|c| style.fg(c)))
}

fn modifier(word: &str) -> Option<Modifier> {
    Some(match word.to_ascii_lowercase().as_str() {
        "bold" => Modifier::BOLD,
        "dim" => Modifier::DIM,
        "italic" => Modifier::ITALIC,
        "underline" | "underlined" => Modifier::UNDERLINED,
        "reverse" | "reversed" => Modifier::REVERSED,
        _ => return None,
    })
}

/// An ANSI color name or `#rrggbb`.
fn color(word: &str) -> Option<Color> {
    if let Some(hex) = word.strip_prefix('#') {
        return hex_color(hex);
    }
    Some(match word.to_ascii_lowercase().as_str() {
        "reset" | "default" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        _ => return None,
    })
}

fn hex_color(hex: &str) -> Option<Color> {
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let [_, r, g, b] = u32::from_str_radix(hex, 16).ok()?.to_be_bytes();
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use ratatui::style::{Color, Modifier, Style};

    use super::*;

    proptest! {
        #[test]
        fn hex_colors_parse(r: u8, g: u8, b: u8) {
            prop_assert_eq!(parse_style(&format!("#{r:02x}{g:02X}{b:02x}")), Ok(Style::new().fg(Color::Rgb(r, g, b))));
        }

        #[test]
        fn sizes_parse_with_decimal_units(n in 0u64..1_000_000, unit in 0usize..4) {
            let (suffix, factor) = [("", 1), ("KB", 1_000), ("MB", 1_000_000), ("GB", 1_000_000_000)][unit];
            prop_assert_eq!(parse_size(&format!("{n}{suffix}")), Some(n * factor));
        }
    }

    #[test]
    fn styles_combine_colors_backgrounds_and_modifiers() {
        let style = Style::new()
            .fg(Color::LightCyan)
            .bg(Color::Rgb(0x30, 0x30, 0x30))
            .add_modifier(Modifier::BOLD | Modifier::REVERSED);
        assert_eq!(parse_style("lightcyan bg:#303030 bold reverse"), Ok(style));
        assert!(parse_style("chartreuse").is_err());
        assert!(parse_style("#12345").is_err());
    }

    #[test]
    fn a_missing_or_empty_config_is_the_default() {
        assert_eq!(parse(""), Ok(Config::default()));
        let missing = std::path::Path::new("/nonexistent/dv/config.toml");
        assert_eq!(load(missing), (Config::default(), None));
    }

    #[test]
    fn named_themes_override_only_the_tokens_they_set() {
        let text = r##"
            theme = "dusk"
            [themes.dusk]
            key = "#ff8800 bold"
            punctuation = "white"
            [themes.other]
            key = "red"
            [mode]
            threshold = "100MB"
            memory_budget = 1000000000
        "##;
        let config = parse(text).unwrap();
        let default = Theme::default();
        assert_eq!(
            config.theme.key,
            Style::new()
                .fg(Color::Rgb(0xff, 0x88, 0))
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(config.theme.punct, Style::new().fg(Color::White));
        assert_eq!(config.theme.string, default.string);
        assert_eq!(
            (config.threshold, config.memory_budget),
            (Some(100_000_000), Some(1_000_000_000))
        );
    }

    #[test]
    fn the_footer_is_on_unless_turned_off() {
        assert!(parse("").unwrap().footer);
        assert!(!parse("[ui]\nfooter = false").unwrap().footer);
        let err = parse("[ui]\nfooter = 1").unwrap_err().to_string();
        assert!(err.contains("ui.footer"), "{err}");
    }

    #[test]
    fn invalid_configs_fall_back_with_a_one_line_warning() {
        let cases = [
            ("theme = \"nope\"", "unknown theme \"nope\""),
            (
                "[themes.default]\nkeys = \"red\"",
                "unknown theme token \"keys\"",
            ),
            (
                "[themes.default]\nkey = \"mauve\"",
                "key: unknown color \"mauve\"",
            ),
            ("[mode]\nthreshold = \"lots\"", "mode.threshold"),
            ("theme = ", "line 1"),
        ];
        for (text, expected) in cases {
            let err = parse(text).unwrap_err().to_string();
            assert!(
                err.contains(expected) && !err.contains('\n'),
                "{text:?}: {err}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theme = \"nope\"").unwrap();
        let (config, warning) = load(&path);
        assert_eq!(config, Config::default());
        assert!(warning.is_some_and(|w| w.starts_with("config: ") && w.contains("nope")));
    }
}
