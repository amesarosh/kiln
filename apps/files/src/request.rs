//! What the portal service asks the picker window for, and what it answers. They are separate
//! processes (a window per request), so both travel as lines of text: one `key value` per
//! line, values escaped so they stay on one line.

use std::path::PathBuf;

use kiln_files::fs::Filter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Pick one or more files (or folders) to open.
    #[default]
    Open,
    /// Pick a name to save one file as.
    Save,
    /// Pick a folder to save several files into.
    SaveFiles,
}

/// An app's choice shown under the files: a list of options, or a checkbox when it has none.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Choice {
    pub id: String,
    pub label: String,
    /// (id, label)
    pub options: Vec<(String, String)>,
    /// The option picked, or "true"/"false" for a checkbox.
    pub selected: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Request {
    pub mode: Mode,
    pub title: String,
    /// Text of the button that accepts ("Open", "Upload"...); empty for the default.
    pub accept: String,
    pub multiple: bool,
    /// Pick folders rather than files.
    pub directory: bool,
    pub folder: Option<PathBuf>,
    /// For Save: the suggested name.
    pub name: String,
    pub filters: Vec<Filter>,
    /// Index into `filters` of the one to start with.
    pub current_filter: Option<usize>,
    pub choices: Vec<Choice>,
    /// For SaveFiles: the names of the files being saved.
    pub files: Vec<String>,
}

/// What the user picked.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Answer {
    pub paths: Vec<PathBuf>,
    pub filter: Option<usize>,
    /// (choice id, value)
    pub choices: Vec<(String, String)>,
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n").replace('\t', "\\t")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

/// Fields of one line: split on tabs, each unescaped.
fn fields(rest: &str) -> Vec<String> {
    rest.split('\t').map(unescape).collect()
}

fn join(parts: &[&str]) -> String {
    parts.iter().map(|p| escape(p)).collect::<Vec<_>>().join("\t")
}

impl Request {
    pub fn encode(&self) -> String {
        let mut out = String::new();
        let mut line = |k: &str, v: String| {
            out.push_str(k);
            out.push(' ');
            out.push_str(&v);
            out.push('\n');
        };
        line(
            "mode",
            match self.mode {
                Mode::Open => "open",
                Mode::Save => "save",
                Mode::SaveFiles => "save-files",
            }
            .into(),
        );
        line("title", escape(&self.title));
        line("accept", escape(&self.accept));
        line("multiple", self.multiple.to_string());
        line("directory", self.directory.to_string());
        if let Some(f) = &self.folder {
            line("folder", escape(&f.to_string_lossy()));
        }
        line("name", escape(&self.name));
        for f in &self.filters {
            let mut parts = vec![f.name.clone()];
            for (kind, p) in &f.patterns {
                parts.push(format!("{kind}:{p}"));
            }
            line("filter", join(&parts.iter().map(String::as_str).collect::<Vec<_>>()));
        }
        if let Some(i) = self.current_filter {
            line("current-filter", i.to_string());
        }
        for c in &self.choices {
            let mut parts = vec![c.id.clone(), c.label.clone(), c.selected.clone()];
            for (id, label) in &c.options {
                parts.push(format!("{id}={label}"));
            }
            line("choice", join(&parts.iter().map(String::as_str).collect::<Vec<_>>()));
        }
        for f in &self.files {
            line("file", escape(f));
        }
        out
    }

    pub fn decode(text: &str) -> Self {
        let mut r = Self::default();
        for l in text.lines() {
            let (key, rest) = l.split_once(' ').unwrap_or((l, ""));
            match key {
                "mode" => {
                    r.mode = match rest {
                        "save" => Mode::Save,
                        "save-files" => Mode::SaveFiles,
                        _ => Mode::Open,
                    }
                }
                "title" => r.title = unescape(rest),
                "accept" => r.accept = unescape(rest),
                "multiple" => r.multiple = rest == "true",
                "directory" => r.directory = rest == "true",
                "folder" => r.folder = Some(PathBuf::from(unescape(rest))),
                "name" => r.name = unescape(rest),
                "filter" => {
                    let f = fields(rest);
                    let patterns = f[1..]
                        .iter()
                        .filter_map(|p| {
                            let (k, p) = p.split_once(':')?;
                            Some((k.parse().ok()?, p.to_string()))
                        })
                        .collect();
                    r.filters.push(Filter {
                        name: f[0].clone(),
                        patterns,
                    });
                }
                "current-filter" => r.current_filter = rest.parse().ok(),
                "choice" => {
                    let f = fields(rest);
                    if f.len() < 3 {
                        continue;
                    }
                    r.choices.push(Choice {
                        id: f[0].clone(),
                        label: f[1].clone(),
                        selected: f[2].clone(),
                        options: f[3..]
                            .iter()
                            .filter_map(|o| o.split_once('=').map(|(a, b)| (a.to_string(), b.to_string())))
                            .collect(),
                    });
                }
                "file" => r.files.push(unescape(rest)),
                _ => {}
            }
        }
        r
    }
}

impl Answer {
    pub fn encode(&self) -> String {
        let mut out = String::new();
        for p in &self.paths {
            out.push_str(&format!("path {}\n", escape(&p.to_string_lossy())));
        }
        if let Some(i) = self.filter {
            out.push_str(&format!("filter {i}\n"));
        }
        for (id, v) in &self.choices {
            out.push_str(&format!("choice {}\n", join(&[id, v])));
        }
        out
    }

    pub fn decode(text: &str) -> Self {
        let mut a = Self::default();
        for l in text.lines() {
            let (key, rest) = l.split_once(' ').unwrap_or((l, ""));
            match key {
                "path" => a.paths.push(PathBuf::from(unescape(rest))),
                "filter" => a.filter = rest.parse().ok(),
                "choice" => {
                    let f = fields(rest);
                    if f.len() == 2 {
                        a.choices.push((f[0].clone(), f[1].clone()));
                    }
                }
                _ => {}
            }
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let r = Request {
            mode: Mode::Save,
            title: "Save\tit\nnow".into(),
            accept: "Export".into(),
            multiple: true,
            directory: false,
            folder: Some("/tmp/a b".into()),
            name: "x.png".into(),
            filters: vec![Filter {
                name: "Images".into(),
                patterns: vec![(0, "*.png".into()), (1, "image/jpeg".into())],
            }],
            current_filter: Some(0),
            choices: vec![Choice {
                id: "enc".into(),
                label: "Encoding".into(),
                options: vec![("utf8".into(), "UTF-8".into())],
                selected: "utf8".into(),
            }],
            files: vec!["a.txt".into()],
        };
        assert_eq!(Request::decode(&r.encode()), r);
        let a = Answer {
            paths: vec!["/tmp/x\ty".into()],
            filter: Some(1),
            choices: vec![("enc".into(), "utf8".into())],
        };
        assert_eq!(Answer::decode(&a.encode()), a);
    }
}
