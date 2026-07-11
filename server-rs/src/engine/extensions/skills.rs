use std::io;
use std::path::Path;

#[derive(serde::Serialize, Clone, Debug)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
}

/// Create `<skills_dir>/<name>/SKILL.md` with YAML frontmatter and the skill's
/// INSTRUCTIONS as the markdown body. An empty `instructions` writes frontmatter
/// only — but note that such a skill is an empty shell: the body is what the agent
/// actually loads and follows, the description only decides WHEN to load it.
/// Returns `AlreadyExists` if the directory already exists.
///
/// Defense-in-depth: asserts `name` is a single normal path component
/// (rejects `..`, `.`, `/abs`, `a/b`, etc.) before creating anything.
/// This blocks traversal attacks even if the API-layer name validator
/// were bypassed.  (Note: checking the parent of the joined path does
/// NOT work for `..` — `Path::parent` strips the `..` component, so the
/// comparison would always pass.  Component-level inspection avoids that.)
pub fn add_skill(
    skills_dir: &Path,
    name: &str,
    description: &str,
    instructions: &str,
) -> io::Result<()> {
    // Belt-and-suspenders traversal guard: verify that `name` is a single,
    // normal path component — no `..`, no `.`, no separators, no absolute
    // path.  We check the components of the *name* itself (before joining)
    // so that tricks like "..", "../x", "/abs", or "a/b" are all rejected,
    // regardless of whether the resulting path happens to exist.
    //
    // Note: checking the lexical parent of `skills_dir.join("..")` does NOT
    // work because Path::parent strips the ".." component and returns
    // `skills_dir` itself, so the comparison always passes for "..".
    {
        use std::path::Component;
        let mut components = std::path::Path::new(name).components();
        let single = components.next();
        let is_single_normal =
            matches!(single, Some(Component::Normal(_))) && components.next().is_none();
        if !is_single_normal {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refusing to create skill '{name}': name must be a single path component"),
            ));
        }
    }

    let skill_dir = skills_dir.join(name);
    if skill_dir.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("skill '{name}' already exists"),
        ));
    }
    std::fs::create_dir_all(&skill_dir)?;
    let mut content = format!("---\nname: {name}\ndescription: {description}\n---\n");
    let body = instructions.trim();
    if !body.is_empty() {
        content.push('\n');
        content.push_str(body);
        content.push('\n');
    }
    std::fs::write(skill_dir.join("SKILL.md"), content)?;
    Ok(())
}

/// Remove `<skills_dir>/<name>` recursively.
/// Returns `false` if absent, `true` on success.
/// Defense-in-depth: canonicalizes both paths and asserts the target is a direct
/// child of `skills_dir` before calling `remove_dir_all` — blocks path-traversal
/// attacks even if the name validator in the API layer were bypassed.
pub fn delete_skill(skills_dir: &Path, name: &str) -> io::Result<bool> {
    let target = skills_dir.join(name);
    if !target.exists() {
        return Ok(false);
    }

    let canon_skills = skills_dir.canonicalize()?;
    let canon_target = target.canonicalize()?;
    let canon_parent = canon_target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "target has no parent"))?;
    if canon_parent != canon_skills {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to delete '{}': not a direct child of skills_dir",
                canon_target.display()
            ),
        ));
    }
    std::fs::remove_dir_all(&canon_target)?;
    Ok(true)
}

pub fn list_skills(skills_dir: &Path) -> Vec<SkillInfo> {
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return vec![];
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let md = e.path().join("SKILL.md");
        if !md.exists() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&md) else {
            continue;
        };
        let entry_name = e.file_name().to_string_lossy().into_owned();
        let (mut name, mut description) = (entry_name.clone(), String::new());
        if let Some(fm) = text
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---"))
        {
            // First match wins — a duplicate name:/ description: line does not override the first.
            let (mut name_set, mut desc_set) = (false, false);
            for line in fm.0.lines() {
                if let Some(v) = line.strip_prefix("name:") {
                    if !name_set {
                        name = v.trim().to_string();
                        name_set = true;
                    }
                } else if let Some(v) = line.strip_prefix("description:") {
                    if !desc_set {
                        description = v.trim().to_string();
                        desc_set = true;
                    }
                }
            }
            if name.is_empty() {
                name = entry_name;
            }
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
        let d = std::env::temp_dir().join(format!(
            "agentic-skills-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lists_skills_from_frontmatter_sorted() {
        let dir = tmp();
        let make = |name: &str, fm: &str| {
            let d = dir.join(name);
            std::fs::create_dir_all(&d).unwrap();
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

    #[test]
    fn add_skill_creates_dir_and_skill_md() {
        let dir = tmp();
        add_skill(&dir, "my-skill", "does things", "").unwrap();
        let md = dir.join("my-skill").join("SKILL.md");
        assert!(md.exists());
        let text = std::fs::read_to_string(&md).unwrap();
        assert_eq!(text, "---\nname: my-skill\ndescription: does things\n---\n");
    }

    #[test]
    fn add_skill_writes_instructions_as_body() {
        let dir = tmp();
        add_skill(
            &dir,
            "real-skill",
            "does things",
            "## Steps\n1. do the thing\n",
        )
        .unwrap();
        let text = std::fs::read_to_string(dir.join("real-skill").join("SKILL.md")).unwrap();
        assert_eq!(
            text,
            "---\nname: real-skill\ndescription: does things\n---\n\n## Steps\n1. do the thing\n",
        );
        // And list_skills still parses the frontmatter with a body present.
        let listed = list_skills(&dir);
        assert!(listed
            .iter()
            .any(|s| s.name == "real-skill" && s.description == "does things"));
    }

    #[test]
    fn add_skill_errors_if_already_exists() {
        let dir = tmp();
        add_skill(&dir, "dup", "d", "").unwrap();
        let err = add_skill(&dir, "dup", "d2", "").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn delete_skill_removes_dir_and_returns_true() {
        let dir = tmp();
        add_skill(&dir, "to-remove", "desc", "").unwrap();
        assert!(delete_skill(&dir, "to-remove").unwrap());
        assert!(!dir.join("to-remove").exists());
    }

    #[test]
    fn delete_skill_absent_returns_false() {
        let dir = tmp();
        assert!(!delete_skill(&dir, "no-such").unwrap());
    }

    #[test]
    fn add_skill_rejects_path_traversal() {
        let dir = tmp();
        // ".." resolves to the parent of skills_dir — must be refused BEFORE any directory
        // or file is created; in particular, <skills_dir>/../SKILL.md must NOT appear.
        let err = add_skill(&dir, "..", "should be refused", "").unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::PermissionDenied,
            "traversal must be refused with PermissionDenied, got: {err}"
        );
        // Verify nothing was created above the skills dir.
        let outside_skill_md = dir.parent().unwrap().join("SKILL.md");
        assert!(
            !outside_skill_md.exists(),
            "SKILL.md must NOT have been created outside skills_dir"
        );
    }

    #[test]
    fn delete_skill_rejects_path_traversal() {
        let dir = tmp();
        // Create a directory OUTSIDE skills_dir to try to delete via traversal.
        let outside = dir
            .parent()
            .unwrap()
            .join(format!("outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        // The name "../outside-<pid>" would resolve to outside dir — must be refused.
        let name = format!("../outside-{}", std::process::id());
        let err = delete_skill(&dir, &name).unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied,
            "traversal must be refused with PermissionDenied, got: {err}"
        );
        // The outside dir must still exist (not deleted).
        assert!(outside.exists());
        std::fs::remove_dir_all(&outside).ok();
    }
}
