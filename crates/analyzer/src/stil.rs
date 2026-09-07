use crate::header::SubtuneIndex;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct StilPath(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StilFieldKind {
    Name,
    Title,
    Artist,
    Author,
    Comment,
    Bug,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[must_use]
pub struct StilField {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtune: Option<SubtuneIndex>,
    pub kind: StilFieldKind,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[must_use]
pub struct StilEntry {
    pub path: StilPath,
    pub fields: Vec<StilField>,
}

#[derive(Debug, Default)]
pub struct StilDatabase {
    entries: HashMap<String, StilEntry>,
}

impl StilDatabase {
    pub fn load(path: &Path) -> io::Result<Self> {
        fs::read_to_string(path).map(|contents| Self::parse(&contents))
    }

    #[must_use]
    pub fn parse(contents: &str) -> Self {
        let mut entries = HashMap::new();
        let mut current_path: Option<String> = None;
        let mut current_subtune = None;
        let mut fields = Vec::new();

        for raw_line in contents.lines() {
            let line = raw_line.trim_end();
            if line.starts_with('/') {
                if let Some(path) = current_path.replace(normalize_path(line)) {
                    insert_entry(&mut entries, path, std::mem::take(&mut fields));
                }
                current_subtune = None;
                continue;
            }
            if current_path.is_none() || line.trim_start().starts_with('#') {
                continue;
            }

            let trimmed = line.trim();
            if let Some(subtune) = parse_subtune_marker(trimmed) {
                current_subtune = Some(subtune);
                continue;
            }
            if let Some((kind, value)) = parse_field(trimmed) {
                fields.push(StilField {
                    subtune: current_subtune,
                    kind,
                    value: value.to_owned(),
                });
            } else if !trimmed.is_empty()
                && let Some(field) = fields.last_mut()
            {
                field.value.push('\n');
                field.value.push_str(trimmed);
            }
        }
        if let Some(path) = current_path {
            insert_entry(&mut entries, path, fields);
        }
        Self { entries }
    }

    #[must_use]
    pub fn lookup(&self, path: &Path) -> Option<&StilEntry> {
        let normalized = normalize_path(&path.to_string_lossy());
        if let Some(entry) = self.entries.get(&normalized) {
            return Some(entry);
        }

        let file_name = path.file_name()?.to_string_lossy();
        let suffix = format!("/{file_name}");
        let mut matches = self
            .entries
            .iter()
            .filter(|(candidate, _)| candidate.ends_with(&suffix));
        let (_, entry) = matches.next()?;
        matches.next().is_none().then_some(entry)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn insert_entry(entries: &mut HashMap<String, StilEntry>, path: String, fields: Vec<StilField>) {
    entries.insert(
        path.clone(),
        StilEntry {
            path: StilPath(path),
            fields,
        },
    );
}

fn normalize_path(path: &str) -> String {
    let normalized = path.trim().replace('\\', "/");
    if normalized.starts_with('/') {
        normalized
    } else {
        format!("/{normalized}")
    }
}

fn parse_subtune_marker(line: &str) -> Option<SubtuneIndex> {
    line.strip_prefix("(#")?
        .strip_suffix(')')?
        .parse::<u16>()
        .ok()
        .filter(|value| *value > 0)
        .map(SubtuneIndex)
}

fn parse_field(line: &str) -> Option<(StilFieldKind, &str)> {
    [
        ("NAME:", StilFieldKind::Name),
        ("TITLE:", StilFieldKind::Title),
        ("ARTIST:", StilFieldKind::Artist),
        ("AUTHOR:", StilFieldKind::Author),
        ("COMMENT:", StilFieldKind::Comment),
        ("BUG:", StilFieldKind::Bug),
    ]
    .into_iter()
    .find_map(|(prefix, kind)| {
        line.strip_prefix(prefix)
            .map(|value| (kind, value.trim_start()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "; STIL sample\n\
/MUSICIANS/H/Hubbard_Rob/Nemesis.sid\n\
   COMMENT: Global note\n\
            continued\n\
       BUG: Playback clicks on real hardware\n\
  (#1)\n\
   NAME: Main Theme\n\
  TITLE: Nemesis the Warlock\n\
 ARTIST: Rob Hubbard\n\
  (#2)\n\
  AUTHOR: Unknown\n\
\n\
/DEMOS/A/Nemesis.sid\n\
 COMMENT: Ambiguous filename\n";

    #[test]
    fn parses_fields_subtunes_and_continuations() {
        let database = StilDatabase::parse(SAMPLE);
        assert_eq!(database.len(), 2);
        let entry = database
            .lookup(Path::new("MUSICIANS/H/Hubbard_Rob/Nemesis.sid"))
            .unwrap();
        assert_eq!(entry.fields.len(), 6);
        assert_eq!(entry.fields[0].value, "Global note\ncontinued");
        assert_eq!(entry.fields[1].kind, StilFieldKind::Bug);
        assert_eq!(entry.fields[1].value, "Playback clicks on real hardware");
        assert_eq!(entry.fields[2].subtune, Some(SubtuneIndex(1)));
        assert_eq!(entry.fields[5].subtune, Some(SubtuneIndex(2)));
    }

    #[test]
    fn filename_fallback_requires_a_unique_match() {
        let database = StilDatabase::parse(SAMPLE);
        assert!(database.lookup(Path::new("Nemesis.sid")).is_none());

        let unique =
            StilDatabase::parse("/MUSICIANS/H/Hubbard_Rob/Nemesis.sid\n COMMENT: One match\n");
        assert!(
            unique
                .lookup(Path::new("assets/music/Nemesis.sid"))
                .is_some()
        );
    }
}
