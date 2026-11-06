//! The pin query (CLAUDE.md section 3, "Tags and filtering"), in every picker and in the
//! Locations filter:
//!
//! - `#aveva` → pins tagged aveva (tags match by prefix: `#av` works)
//! - `#aveva #servers` → both (AND) · `#aveva,autodesk` → either (OR) · `!#archive` → not
//! - `is:folder|file|app|web|sync|backup|unc|local|unreachable|view` (and `!is:…`)|file|app|web|sync|backup|unc|local|unreachable` (and `!is:…`)
//! - `on:srv01` → pins on that server (mapped drives resolved by the caller)
//! - everything else is fuzzy text, matched by nucleo
//!
//! Filters narrow the set; they don't score. Pure: no I/O.

use crate::pins::PinKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Is {
    Kind(PinKind),
    Unc,
    Local,
    Unreachable,
    View,
}

/// The `is:` words, in the order completion offers them.
pub const IS_WORDS: &[&str] = &[
    "folder",
    "app",
    "web",
    "file",
    "sync",
    "backup",
    "unc",
    "local",
    "unreachable",
    "view",
];

impl Is {
    fn parse(word: &str) -> Option<Is> {
        Some(match word {
            "folder" => Is::Kind(PinKind::Folder),
            "file" => Is::Kind(PinKind::File),
            "app" => Is::Kind(PinKind::App),
            "web" => Is::Kind(PinKind::Web),
            "sync" => Is::Kind(PinKind::Sync),
            "backup" => Is::Kind(PinKind::Backup),
            "unc" => Is::Unc,
            "local" => Is::Local,
            "unreachable" => Is::Unreachable,
            "view" => Is::View,
            _ => return None,
        })
    }
}

/// What a filter can ask about a pin. The caller works these out (reachability comes
/// from the probes, the server of a mapped drive from the Locations listing).
#[derive(Debug, Clone, Default)]
pub struct Facts<'a> {
    pub kind: Option<PinKind>,
    /// A saved view (`is:view`).
    pub view: bool,
    pub tags: &'a [String],
    /// The path is a `\\server\share` path.
    pub unc: bool,
    /// On a local disk (not a share, not a mapped drive).
    pub local: bool,
    /// The last probe failed.
    pub unreachable: bool,
    /// The server it lives on, lowercase, for `on:`.
    pub server: Option<String>,
}

impl Facts<'_> {
    fn is(&self, is: Is) -> bool {
        match is {
            Is::Kind(k) => self.kind == Some(k),
            Is::Unc => self.unc,
            Is::Local => self.local,
            Is::Unreachable => self.unreachable,
            Is::View => self.view,
        }
    }

    fn has_tag(&self, prefix: &str) -> bool {
        self.tags.iter().any(|t| t.starts_with(prefix))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PinQuery {
    /// Every group must match; a group matches when any of its tags does.
    pub tags: Vec<Vec<String>>,
    pub not_tags: Vec<String>,
    pub is: Vec<Is>,
    pub not_is: Vec<Is>,
    /// Any of these servers.
    pub on: Vec<String>,
    /// The fuzzy part, for nucleo.
    pub text: String,
    /// Words that look like filters but aren't (`is:nope`), to show under the prompt.
    pub problems: Vec<String>,
}

impl PinQuery {
    pub fn parse(query: &str) -> PinQuery {
        let mut q = PinQuery::default();
        let mut text: Vec<&str> = Vec::new();
        for word in query.split_whitespace() {
            if let Some(rest) = word.strip_prefix("!#") {
                q.not_tags.extend(tag_list(rest));
            } else if let Some(rest) = word.strip_prefix('#') {
                let group = tag_list(rest);
                // A bare `#` (still typing) filters nothing.
                if !group.is_empty() {
                    q.tags.push(group);
                }
            } else if let Some((neg, rest)) = word
                .strip_prefix("!is:")
                .map(|r| (true, r))
                .or_else(|| word.strip_prefix("is:").map(|r| (false, r)))
            {
                match Is::parse(&rest.to_lowercase()) {
                    Some(is) if neg => q.not_is.push(is),
                    Some(is) => q.is.push(is),
                    None if rest.is_empty() => {}
                    None => q.problems.push(format!("unknown filter is:{rest}")),
                }
            } else if let Some(rest) = word.strip_prefix("on:") {
                let server = rest.trim_start_matches('\\').to_lowercase();
                if !server.is_empty() {
                    q.on.push(server);
                }
            } else {
                text.push(word);
            }
        }
        q.text = text.join(" ");
        q
    }

    pub fn matches(&self, f: &Facts) -> bool {
        self.tags.iter().all(|g| g.iter().any(|t| f.has_tag(t)))
            && !self.not_tags.iter().any(|t| f.has_tag(t))
            && self.is.iter().all(|i| f.is(*i))
            && !self.not_is.iter().any(|i| f.is(*i))
            && (self.on.is_empty()
                || f.server
                    .as_deref()
                    .is_some_and(|s| self.on.iter().any(|o| s.starts_with(o.as_str()))))
    }

    /// The single tag filter `]t` / `[t` cycle through, if that's all the query has.
    pub fn single_tag(&self) -> Option<&str> {
        match self.tags.as_slice() {
            [g] if g.len() == 1 => Some(&g[0]),
            _ => None,
        }
    }
}

/// `aveva,#autodesk` → `["aveva", "autodesk"]`.
fn tag_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(|t| t.trim().trim_start_matches('#').to_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// The completion for the word being typed, if it's a filter word: what's typed of it
/// (`#av`), and the candidates as (completed word, count).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The text before the part being completed (`… #aveva,`).
    pub head: String,
    pub candidates: Vec<(String, usize)>,
}

/// Completes the last word of `query` against tags, `is:` words and servers, each given
/// with a count. Already-used tags aren't offered again.
pub fn complete(
    query: &str,
    tags: &[(String, usize)],
    is_counts: &[(String, usize)],
    servers: &[(String, usize)],
) -> Option<Completion> {
    if query.ends_with(char::is_whitespace) {
        return None;
    }
    let word = query.rsplit(char::is_whitespace).next().unwrap_or_default();
    let before = &query[..query.len() - word.len()];
    let used = PinQuery::parse(before);
    let (prefix_len, typed, pool): (usize, &str, &[(String, usize)]) =
        if let Some(rest) = word.strip_prefix("!#").or_else(|| word.strip_prefix('#')) {
            // In `#a,b` only the part after the last comma is being typed.
            let piece = rest.rsplit(',').next().unwrap_or_default();
            (word.len() - piece.len(), piece, tags)
        } else if let Some(rest) = word
            .strip_prefix("!is:")
            .or_else(|| word.strip_prefix("is:"))
        {
            (word.len() - rest.len(), rest, is_counts)
        } else if let Some(rest) = word.strip_prefix("on:") {
            (word.len() - rest.len(), rest, servers)
        } else {
            return None;
        };
    let typed = typed.to_lowercase();
    let in_word: Vec<String> = tag_list(word.trim_start_matches(['!', '#']));
    let candidates: Vec<(String, usize)> = pool
        .iter()
        .filter(|(name, _)| name.starts_with(&typed) && *name != typed)
        .filter(|(name, _)| {
            !(used.tags.iter().flatten().any(|t| t == name)
                || used.not_tags.contains(name)
                || in_word[..in_word.len().saturating_sub(1)].contains(name))
        })
        .map(|(name, n)| (name.clone(), *n))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    Some(Completion {
        head: format!("{before}{}", &word[..prefix_len]),
        candidates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(list: &[&str]) -> Vec<String> {
        list.iter().map(|t| t.to_string()).collect()
    }

    fn facts<'a>(kind: PinKind, tags: &'a [String], server: Option<&str>) -> Facts<'a> {
        Facts {
            kind: Some(kind),
            view: false,
            tags,
            unc: server.is_some(),
            local: server.is_none(),
            unreachable: false,
            server: server.map(str::to_string),
        }
    }

    #[test]
    fn parse_table() {
        let q = PinQuery::parse(
            "hit #aveva #Servers,acme !#archive is:folder !is:web on:\\\\SRV01 is:nope #",
        );
        assert_eq!(q.text, "hit");
        assert_eq!(q.tags, vec![tags(&["aveva"]), tags(&["servers", "acme"])]);
        assert_eq!(q.not_tags, tags(&["archive"]));
        assert_eq!(q.is, [Is::Kind(PinKind::Folder)]);
        assert_eq!(q.not_is, [Is::Kind(PinKind::Web)]);
        assert_eq!(q.on, ["srv01"]);
        assert_eq!(q.problems, ["unknown filter is:nope"]);

        let plain = PinQuery::parse("  b1001   iso ");
        assert_eq!(plain.text, "b1001 iso");
        assert!(PinQuery::parse("is:").problems.is_empty(), "still typing");
    }

    #[test]
    fn matches_table() {
        let av = tags(&["aveva", "servers"]);
        let auto = tags(&["autodesk"]);
        let hit_server = facts(PinKind::Folder, &av, Some("srv01"));
        let cad_local = facts(PinKind::App, &auto, None);
        let cases: &[(&str, bool, bool)] = &[
            // query, matches hit_server, matches cad_local
            ("", true, true),
            ("#aveva", true, false),
            ("#av", true, false),
            ("#a", true, true),
            ("#aveva #servers", true, false),
            ("#aveva #autodesk", false, false),
            ("#aveva,autodesk", true, true),
            ("!#aveva", false, true),
            ("is:folder", true, false),
            ("is:app", false, true),
            ("!is:folder", false, true),
            ("is:unc", true, false),
            ("is:local", false, true),
            ("is:unreachable", false, false),
            ("on:srv01", true, false),
            ("on:srv0", true, false),
            ("on:other", false, false),
            ("#aveva is:folder on:srv01", true, false),
        ];
        for (q, a, b) in cases {
            let q2 = PinQuery::parse(q);
            assert_eq!(q2.matches(&hit_server), *a, "{q} vs hit_server");
            assert_eq!(q2.matches(&cad_local), *b, "{q} vs cad_local");
        }
    }

    #[test]
    fn single_tag_for_cycling() {
        assert_eq!(PinQuery::parse("x #aveva").single_tag(), Some("aveva"));
        assert_eq!(PinQuery::parse("#a #b").single_tag(), None);
        assert_eq!(PinQuery::parse("#a,b").single_tag(), None);
        assert_eq!(PinQuery::parse("x").single_tag(), None);
    }

    #[test]
    fn completion_table() {
        let t = vec![
            ("aveva".to_string(), 23),
            ("acme".to_string(), 40),
            ("autodesk".to_string(), 11),
        ];
        let is: Vec<(String, usize)> = vec![("folder".into(), 9), ("file".into(), 2)];
        let on = vec![("srv01".to_string(), 30)];
        let names = |q: &str| -> Option<(String, Vec<String>)> {
            complete(q, &t, &is, &on)
                .map(|c| (c.head, c.candidates.into_iter().map(|(n, _)| n).collect()))
        };
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            names("hit #a"),
            Some(("hit #".into(), s(&["aveva", "acme", "autodesk"])))
        );
        assert_eq!(names("#au"), Some(("#".into(), s(&["autodesk"]))));
        assert_eq!(names("!#av"), Some(("!#".into(), s(&["aveva"]))));
        assert_eq!(
            names("#aveva,a"),
            Some(("#aveva,".into(), s(&["acme", "autodesk"])))
        );
        assert_eq!(
            names("#aveva #a"),
            Some(("#aveva #".into(), s(&["acme", "autodesk"])))
        );
        assert_eq!(names("is:f"), Some(("is:".into(), s(&["folder", "file"]))));
        assert_eq!(names("on:srv"), Some(("on:".into(), s(&["srv01"]))));
        assert_eq!(names("#aveva"), None, "complete already");
        assert_eq!(names("hit"), None, "not a filter word");
        assert_eq!(names("#a "), None, "word finished");
    }
}
