//! The launcher (CLAUDE.md section 6): what running an app or web pin means. Ranking
//! lives in the picker; frecency in [`crate::frecency`]. Pure: the worker runs the
//! [`Launch`] (ShellExecute, with `%ENV%` expanded, as wtm does).

use crate::pins::{Pin, PinKind};

/// One ShellExecute: program, arguments (one string, as typed), working folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub command: String,
    pub args: String,
    pub dir: String,
    /// The program is a browser to find (msedge.exe…) through PATH / App Paths first.
    pub find: bool,
}

/// `edge` → `msedge.exe`; a path stays as it is. Edge is the default: always on
/// Windows 11.
pub fn browser_exe(browser: Option<&str>) -> (String, bool) {
    match browser.map(|b| b.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("edge") | Some("msedge") => ("msedge.exe".into(), true),
        Some("chrome") => ("chrome.exe".into(), true),
        Some("brave") => ("brave.exe".into(), true),
        Some("vivaldi") => ("vivaldi.exe".into(), true),
        _ => (browser.unwrap_or_default().trim().to_string(), false),
    }
}

/// Quotes an argument that has spaces or quotes (Windows command-line rules for the
/// cases that occur: URLs and paths).
fn quote(s: &str) -> String {
    if s.is_empty() || s.contains([' ', '\t', '"']) {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// What opening an app or web pin runs. None for other kinds.
pub fn launch_of(p: &Pin) -> Option<Launch> {
    match p.kind {
        PinKind::App => Some(Launch {
            command: p.command.clone()?,
            args: p.args.clone().unwrap_or_default(),
            dir: p.dir.clone().unwrap_or_default(),
            find: false,
        }),
        PinKind::Web => {
            let url = p.url.clone()?;
            let (exe, find) = browser_exe(p.browser.as_deref());
            let mut args = format!("--app={}", quote(&url));
            if let Some(profile) = p.extra.get("profile").and_then(|v| v.as_str()) {
                args.push_str(&format!(" --profile-directory={}", quote(profile)));
            }
            Some(Launch {
                command: exe,
                args,
                dir: String::new(),
                find,
            })
        }
        _ => None,
    }
}

/// The launcher's "nothing matched: run what I typed": the first word (or quoted part)
/// is the program, the rest its arguments. `notepad x.txt`, `"C:\My App\a.exe" -v`,
/// `https://…`.
pub fn typed(text: &str) -> Option<Launch> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let (command, rest) = match t.strip_prefix('"') {
        Some(after) => match after.split_once('"') {
            Some((cmd, rest)) => (cmd.to_string(), rest.trim()),
            None => (after.to_string(), ""),
        },
        None => match t.split_once(char::is_whitespace) {
            Some((cmd, rest)) => (cmd.to_string(), rest.trim()),
            None => (t.to_string(), ""),
        },
    };
    Some(Launch {
        command,
        args: rest.to_string(),
        dir: String::new(),
        find: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(t: &str) -> Pin {
        match toml::from_str::<Pin>(t) {
            Ok(p) => p,
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn apps_and_web_apps() {
        let app =
            pin("kind='app'\nname='E3D'\ncommand='%AVEVA%\\evars.bat'\nargs='HIT'\ndir='C:\\p'");
        assert_eq!(
            launch_of(&app),
            Some(Launch {
                command: r"%AVEVA%\evars.bat".into(),
                args: "HIT".into(),
                dir: r"C:\p".into(),
                find: false
            })
        );
        let web = pin("kind='web'\nname='g'\nurl='https://grafana.example/d?a=1 b'");
        let l = launch_of(&web);
        assert_eq!(
            l.as_ref()
                .map(|l| (l.command.as_str(), l.args.as_str(), l.find)),
            Some((
                "msedge.exe",
                "--app=\"https://grafana.example/d?a=1 b\"",
                true
            ))
        );
        let brave =
            pin("kind='web'\nname='g'\nurl='https://x'\nbrowser='brave'\nprofile='Profile 2'");
        assert_eq!(
            launch_of(&brave).map(|l| (l.command, l.args)),
            Some((
                "brave.exe".to_string(),
                "--app=https://x --profile-directory=\"Profile 2\"".to_string()
            ))
        );
        let custom = pin("kind='web'\nname='g'\nurl='https://x'\nbrowser='C:\\b\\b.exe'");
        assert_eq!(
            launch_of(&custom).map(|l| (l.command, l.find)),
            Some((r"C:\b\b.exe".to_string(), false))
        );
        assert_eq!(
            launch_of(&pin("kind='folder'\nname='x'\npath='C:\\'")),
            None
        );
    }

    #[test]
    fn typed_text() {
        let t = |s: &str| typed(s).map(|l| (l.command, l.args));
        assert_eq!(
            t("notepad  x.txt"),
            Some(("notepad".into(), "x.txt".into()))
        );
        assert_eq!(
            t(r#""C:\My App\a.exe" -v --x"#),
            Some((r"C:\My App\a.exe".into(), "-v --x".into()))
        );
        assert_eq!(
            t("https://x.example"),
            Some(("https://x.example".into(), "".into()))
        );
        assert_eq!(t("   "), None);
    }
}
