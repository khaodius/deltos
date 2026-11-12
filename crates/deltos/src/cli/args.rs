//! A small argument reader: positional arguments, `--name value`, `--name=value`, short
//! `-x value`, and flags. Options may come before or after the positionals.

use std::collections::HashMap;

/// Options that take a value; anything else starting with `-` is a flag.
const VALUED: [&str; 10] = [
    "execute",
    "file",
    "format",
    "output",
    "max-rows",
    "namespace",
    "into",
    "delimiter",
    "null",
    "where",
];

/// Short names and the long option each stands for.
const SHORT: [(&str, &str); 6] = [
    ("e", "execute"),
    ("f", "file"),
    ("o", "output"),
    ("n", "namespace"),
    ("y", "yes"),
    ("h", "help"),
];

#[derive(Debug, Default, PartialEq)]
pub struct Args {
    pub positional: Vec<String>,
    values: HashMap<String, Vec<String>>,
    flags: Vec<String>,
}

impl Args {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut out = Args::default();
        let mut it = args.into_iter();
        let mut only_positional = false;
        while let Some(arg) = it.next() {
            if only_positional || arg == "-" || !arg.starts_with('-') {
                out.positional.push(arg);
                continue;
            }
            if arg == "--" {
                only_positional = true;
                continue;
            }
            let (name, inline) = match arg.strip_prefix("--") {
                Some(long) => match long.split_once('=') {
                    Some((n, v)) => (n.to_owned(), Some(v.to_owned())),
                    None => (long.to_owned(), None),
                },
                None => {
                    let short = &arg[1..];
                    let long = SHORT
                        .iter()
                        .find(|(s, _)| *s == short)
                        .map(|(_, l)| *l)
                        .ok_or_else(|| format!("unknown option {arg}"))?;
                    (long.to_owned(), None)
                }
            };
            if VALUED.contains(&name.as_str()) {
                let value = match inline {
                    Some(v) => v,
                    None => it.next().ok_or_else(|| format!("--{name} needs a value"))?,
                };
                out.values.entry(name).or_default().push(value);
            } else if inline.is_some() {
                return Err(format!("--{name} takes no value"));
            } else {
                out.flags.push(name);
            }
        }
        Ok(out)
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    /// The last value given for `name`.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }

    /// Every value given for `name`, in order.
    pub fn values(&self, name: &str) -> &[String] {
        self.values.get(name).map_or(&[], Vec::as_slice)
    }

    /// Fails on a flag this command does not know, so a typo is not silently ignored.
    pub fn only_flags(&self, known: &[&str]) -> Result<(), String> {
        match self
            .flags
            .iter()
            .find(|f| !known.contains(&f.as_str()) && f.as_str() != "help")
        {
            Some(f) => Err(format!("unknown option --{f}")),
            None => Ok(()),
        }
    }

    /// Fails on an option with a value this command does not take.
    pub fn only_values(&self, known: &[&str]) -> Result<(), String> {
        match self.values.keys().find(|k| !known.contains(&k.as_str())) {
            Some(k) => Err(format!("--{k} does not apply to this command")),
            None => Ok(()),
        }
    }
}

/// `a.b.c` as a namespace path. Empty parts are dropped.
pub fn path(text: &str) -> Vec<String> {
    text.split('.')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        Args::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn reads_options_anywhere() {
        let a = parse(&[
            "prod",
            "-e",
            "select 1",
            "--format=json",
            "--yes",
            "-e",
            "x",
        ])
        .unwrap();
        assert_eq!(a.positional, ["prod"]);
        assert_eq!(a.values("execute"), ["select 1", "x"]);
        assert_eq!(a.value("format"), Some("json"));
        assert!(a.flag("yes"));
        assert!(!a.flag("json"));
    }

    #[test]
    fn refuses_what_it_does_not_know() {
        assert!(parse(&["-z"]).is_err());
        assert!(parse(&["--format"]).is_err());
        assert!(parse(&["--yes=1"]).is_err());
        let a = parse(&["--jsn"]).unwrap();
        assert!(a.only_flags(&["json"]).is_err());
        let a = parse(&["--", "-e"]).unwrap();
        assert_eq!(a.positional, ["-e"]);
    }

    #[test]
    fn splits_namespace_paths() {
        assert_eq!(path("app.public"), ["app", "public"]);
        assert_eq!(path(" main "), ["main"]);
        assert!(path("").is_empty());
    }
}
