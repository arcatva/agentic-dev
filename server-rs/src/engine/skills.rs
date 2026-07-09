use std::path::Path;

#[derive(serde::Serialize, Clone, Debug)]
pub struct SkillInfo { pub name: String, pub description: String }

pub fn list_skills(skills_dir: &Path) -> Vec<SkillInfo> {
    let Ok(entries) = std::fs::read_dir(skills_dir) else { return vec![]; };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let md = e.path().join("SKILL.md");
        if !md.exists() { continue; }
        let Ok(text) = std::fs::read_to_string(&md) else { continue; };
        let entry_name = e.file_name().to_string_lossy().into_owned();
        let (mut name, mut description) = (entry_name.clone(), String::new());
        if let Some(fm) = text.strip_prefix("---\n").and_then(|rest| rest.split_once("\n---")) {
            // First match wins — a duplicate name:/ description: line does not override the first.
            let (mut name_set, mut desc_set) = (false, false);
            for line in fm.0.lines() {
                if let Some(v) = line.strip_prefix("name:") {
                    if !name_set { name = v.trim().to_string(); name_set = true; }
                } else if let Some(v) = line.strip_prefix("description:") {
                    if !desc_set { description = v.trim().to_string(); desc_set = true; }
                }
            }
            if name.is_empty() { name = entry_name; }
        }
        out.push(SkillInfo { name, description });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agentic-skills-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lists_skills_from_frontmatter_sorted() {
        let dir = tmp();
        let make = |name: &str, fm: &str| {
            let d = dir.join(name); std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("SKILL.md"), fm).unwrap();
        };
        make("zskill", "---\nname: zed\ndescription: last one\n---\nbody");
        make("askill", "---\nname: alpha\ndescription: first\n---\nbody");
        std::fs::create_dir_all(dir.join("nomd")).unwrap(); // no SKILL.md → skipped
        let out = list_skills(&dir);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "alpha");
        assert_eq!(out[0].description, "first");
        assert_eq!(out[1].name, "zed");
        assert!(list_skills(&dir.join("missing")).is_empty());
    }
}
