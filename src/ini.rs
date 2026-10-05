use indexmap::IndexMap;

/// A parsed INI file with ordered sections and ordered key-value pairs.
/// Comments and blank lines are dropped on parse; the written output is clean.
#[derive(Debug, Clone, Default)]
pub struct IniFile {
    pub sections: IndexMap<String, IndexMap<String, String>>,
}

impl IniFile {
    pub fn parse(text: &str) -> Self {
        let mut ini = Self::default();
        let mut current = String::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
                continue;
            }
            if let Some(name) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                current = name.to_owned();
                ini.sections.entry(current.clone()).or_default();
            } else if let Some(eq_pos) = trimmed.find('=') {
                let key = trimmed[..eq_pos].trim().to_owned();
                let val = trimmed[eq_pos + 1..].trim().to_owned();
                if !key.is_empty() {
                    ini.sections
                        .entry(current.clone())
                        .or_default()
                        .insert(key, val);
                }
            }
        }
        ini
    }

    pub fn set(&mut self, section: &str, key: &str, value: impl Into<String>) {
        self.sections
            .entry(section.to_owned())
            .or_default()
            .insert(key.to_owned(), value.into());
    }

    #[allow(dead_code)]
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections.get(section)?.get(key).map(String::as_str)
    }

    /// Flatten to "Section.Key" → "Value" map.
    pub fn to_flat_map(&self) -> IndexMap<String, String> {
        let mut map = IndexMap::new();
        for (section, keys) in &self.sections {
            for (key, val) in keys {
                map.insert(format!("{}.{}", section, key), val.clone());
            }
        }
        map
    }

    /// Apply a flat "Section.Key" overlay onto this file.
    /// Splits on the *last* dot so section names that themselves contain dots
    /// (e.g. "ports.qt") round-trip correctly with `to_flat_map`.
    pub fn apply_overlay(&mut self, overlay: &IndexMap<String, String>) {
        for (flat_key, value) in overlay {
            if let Some(dot) = flat_key.rfind('.') {
                let section = &flat_key[..dot];
                let key = &flat_key[dot + 1..];
                self.set(section, key, value.clone());
            }
        }
    }
}

impl std::fmt::Display for IniFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (section, keys) in &self.sections {
            writeln!(f, "[{}]", section)?;
            for (k, v) in keys {
                writeln!(f, "{} = {}", k, v)?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_roundtrip() {
        let text =
            "[GPU]\nResolutionScale = 4\nTextureFilter = xBR-4x\n\n[CPU]\nOverclockEnable = true\n";
        let ini = IniFile::parse(text);
        assert_eq!(ini.get("GPU", "ResolutionScale"), Some("4"));
        assert_eq!(ini.get("CPU", "OverclockEnable"), Some("true"));
    }

    #[test]
    fn overlay_creates_sections() {
        let mut ini = IniFile::default();
        let mut overlay = IndexMap::new();
        overlay.insert("GPU.ResolutionScale".into(), "8".into());
        ini.apply_overlay(&overlay);
        assert_eq!(ini.get("GPU", "ResolutionScale"), Some("8"));
    }

    #[test]
    fn flat_map_round_trips() {
        let text = "[GPU]\nResolutionScale = 4\n";
        let ini = IniFile::parse(text);
        let flat = ini.to_flat_map();
        assert_eq!(
            flat.get("GPU.ResolutionScale").map(String::as_str),
            Some("4")
        );
    }

    #[test]
    fn dotted_section_name_round_trips() {
        // mGBA uses [ports.qt] — the section name itself contains a dot.
        // apply_overlay must split on the last dot, not the first.
        let text = "[ports.qt]\nwidth = 480\nheight = 320\n";
        let ini = IniFile::parse(text);
        let flat = ini.to_flat_map();
        assert_eq!(flat.get("ports.qt.width").map(String::as_str), Some("480"));

        let mut ini2 = IniFile::default();
        ini2.apply_overlay(&flat);
        assert_eq!(ini2.get("ports.qt", "width"), Some("480"));
        assert_eq!(ini2.get("ports.qt", "height"), Some("320"));
    }
}
