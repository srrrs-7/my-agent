//! `package.json` and `package-lock.json` / `npm-shrinkwrap.json`.

use serde_json::{Map, Value};

use super::{Entries, insert};

/// Sections of `package.json` whose keys are packages to install.
const DEPENDENCY_SECTIONS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
];

/// Sections that redirect what other entries resolve to.
const OVERRIDE_SECTIONS: [&str; 2] = ["overrides", "resolutions"];

/// Lists of packages shipped inside this one's tarball.
const BUNDLE_SECTIONS: [&str; 2] = ["bundleDependencies", "bundledDependencies"];

/// Scripts that npm runs by itself during an install. Adding one is how a
/// package gets code executed without anyone calling it, so it is listed next
/// to the dependencies even though it is not one.
const INSTALL_SCRIPTS: [&str; 5] = [
    "preinstall",
    "install",
    "postinstall",
    "prepare",
    "prepublish",
];

/// The registry whose tarball URLs go unmentioned.
const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org/";

/// Characters of an integrity hash shown.
const INTEGRITY_CHARS: usize = 20;

pub(super) fn manifest(text: &str) -> Result<Entries, String> {
    let root = object(text)?;
    let mut entries = Entries::new();

    for section in DEPENDENCY_SECTIONS.iter().chain(&OVERRIDE_SECTIONS) {
        let Some(value) = root.get(*section) else {
            continue;
        };
        let Value::Object(packages) = value else {
            // Not a shape npm accepts, but whatever it is, show it.
            insert(&mut entries, format!("({section})"), value.to_string());
            continue;
        };
        for (name, spec) in packages {
            let name = if *section == "dependencies" {
                name.clone()
            } else {
                format!("{name} ({section})")
            };
            insert(&mut entries, name, spec_text(spec));
        }
    }

    for section in BUNDLE_SECTIONS {
        if let Some(value) = root.get(section) {
            insert(&mut entries, format!("({section})"), value.to_string());
        }
    }

    if let Some(Value::Object(scripts)) = root.get("scripts") {
        for script in INSTALL_SCRIPTS {
            if let Some(command) = scripts.get(script) {
                insert(
                    &mut entries,
                    format!("{script} (scripts)"),
                    spec_text(command),
                );
            }
        }
    }
    Ok(entries)
}

/// Everything in a `package.json` that [`manifest`] does not read as an
/// entry, flattened to `key.path = value` - `scripts.test = "..."`,
/// `bin.tool = "./x.js"`. Objects are walked so that one changed script is
/// one changed line, not the whole `scripts` object; arrays stay whole.
pub(super) fn other(text: &str) -> Result<Vec<String>, String> {
    let root = object(text)?;
    let mut lines = Vec::new();
    for (key, value) in &root {
        let is_entry = DEPENDENCY_SECTIONS
            .iter()
            .chain(&OVERRIDE_SECTIONS)
            .chain(&BUNDLE_SECTIONS)
            .any(|section| *section == key.as_str());
        if is_entry {
            continue;
        }
        if key == "scripts"
            && let Value::Object(scripts) = value
        {
            for (script, command) in scripts {
                if !INSTALL_SCRIPTS.contains(&script.as_str()) {
                    flatten(&format!("scripts.{script}"), command, &mut lines);
                }
            }
            continue;
        }
        flatten(key, value, &mut lines);
    }
    Ok(lines)
}

fn flatten(path: &str, value: &Value, lines: &mut Vec<String>) {
    match value {
        Value::Object(fields) if !fields.is_empty() => {
            for (key, value) in fields {
                flatten(&format!("{path}.{key}"), value, lines);
            }
        }
        other => lines.push(format!("{path} = {other}")),
    }
}

/// The installed packages a lockfile pins. Lockfile v2/v3 list them under
/// `packages` by install path; v1 only has the nested `dependencies` tree.
pub(super) fn lockfile(text: &str) -> Result<Entries, String> {
    let root = object(text)?;
    let mut entries = Entries::new();

    if let Some(packages) = root.get("packages") {
        let Value::Object(packages) = packages else {
            return Err("`packages` is not an object".to_string());
        };
        for (path, package) in packages {
            // The empty path is the project itself.
            if path.is_empty() {
                continue;
            }
            let name = path.strip_prefix("node_modules/").unwrap_or(path);
            insert(&mut entries, name.to_string(), locked_spec(package));
        }
        return Ok(entries);
    }

    if let Some(dependencies) = root.get("dependencies") {
        walk_v1(dependencies, "", &mut entries)?;
    }
    Ok(entries)
}

fn walk_v1(dependencies: &Value, parent: &str, entries: &mut Entries) -> Result<(), String> {
    let Value::Object(dependencies) = dependencies else {
        return Err("`dependencies` is not an object".to_string());
    };
    for (name, package) in dependencies {
        let path = if parent.is_empty() {
            name.clone()
        } else {
            format!("{parent} > {name}")
        };
        insert(entries, path.clone(), locked_spec(package));
        if let Some(nested) = package.get("dependencies") {
            walk_v1(nested, &path, entries)?;
        }
    }
    Ok(())
}

fn locked_spec(package: &Value) -> String {
    let field = |key: &str| package.get(key).and_then(Value::as_str);
    let mut spec = field("version").unwrap_or("?").to_string();
    if package.get("link").and_then(Value::as_bool) == Some(true) {
        spec.push_str(" (link)");
    }
    if let Some(resolved) = field("resolved").filter(|url| !url.starts_with(DEFAULT_REGISTRY)) {
        spec.push_str(&format!(" ({resolved})"));
    }
    if let Some(integrity) = field("integrity") {
        let short: String = integrity.chars().take(INTEGRITY_CHARS).collect();
        spec.push_str(&format!(" {short}"));
    }
    if let Some(Value::Bool(true)) = package.get("hasInstallScript") {
        spec.push_str(" [install script]");
    }
    spec
}

fn object(text: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(root)) => Ok(root),
        Ok(_) => Err("the top level is not a JSON object".to_string()),
        Err(error) => Err(format!("invalid JSON: {error}")),
    }
}

fn spec_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::dependency::DependencyChange;

    fn changes(
        parse: fn(&str) -> Result<Entries, String>,
        before: &str,
        after: &str,
    ) -> Vec<String> {
        DependencyChange::between(&parse(before).unwrap(), &parse(after).unwrap())
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    const PACKAGE: &str = r#"{
  "name": "demo",
  "dependencies": { "react": "^18.2.0", "lodash": "^4.17.21" },
  "devDependencies": { "vitest": "^1.0.0" },
  "scripts": { "test": "vitest" }
}"#;

    #[test]
    fn package_json_adds_removes_and_changes_are_listed() {
        let after = r#"{
  "name": "demo",
  "dependencies": { "react": "^19.0.0", "left-pad": "github:someone/left-pad" },
  "devDependencies": { "vitest": "^1.0.0" },
  "scripts": { "test": "vitest", "postinstall": "node steal.js" }
}"#;
        assert_eq!(
            changes(manifest, PACKAGE, after),
            [
                "+ left-pad github:someone/left-pad",
                "- lodash ^4.17.21",
                "+ postinstall (scripts) node steal.js",
                "~ react ^18.2.0 -> ^19.0.0",
            ]
        );
    }

    #[test]
    fn overrides_count_as_dependency_changes() {
        let after = PACKAGE.replacen('{', r#"{ "overrides": { "lodash": "npm:evil@1" },"#, 1);
        assert_eq!(
            changes(manifest, PACKAGE, &after),
            ["+ lodash (overrides) npm:evil@1"]
        );
    }

    #[test]
    fn unrelated_fields_change_no_entry() {
        let after = PACKAGE.replace("\"test\": \"vitest\"", "\"test\": \"vitest --run\"");
        let after = after.replace("\"name\": \"demo\"", "\"name\": \"demo2\"");
        assert!(changes(manifest, PACKAGE, &after).is_empty());
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(manifest("{\"dependencies\": ").is_err());
        assert!(manifest("[]").is_err());
    }

    const LOCK: &str = r#"{
  "lockfileVersion": 3,
  "packages": {
    "": { "name": "demo" },
    "node_modules/react": { "version": "18.2.0", "resolved": "https://registry.npmjs.org/react/-/react-18.2.0.tgz", "integrity": "sha512-aaaaaaaaaaaaaaaaaaaaaaaa" }
  }
}"#;

    #[test]
    fn a_lockfile_lists_installed_versions_and_unusual_sources() {
        let after = r#"{
  "lockfileVersion": 3,
  "packages": {
    "": { "name": "demo" },
    "node_modules/react": { "version": "18.3.0", "integrity": "sha512-bbbbbbbbbbbbbbbbbbbbbb" },
    "node_modules/evil": { "version": "1.0.0", "resolved": "https://evil.example/evil.tgz", "hasInstallScript": true }
  }
}"#;
        assert_eq!(
            changes(lockfile, LOCK, after),
            [
                "+ evil 1.0.0 (https://evil.example/evil.tgz) [install script]",
                "~ react 18.2.0 sha512-aaaaaaaaaaaaa -> 18.3.0 sha512-bbbbbbbbbbbbb",
            ]
        );
    }

    #[test]
    fn a_version_1_lockfile_is_read_through_its_nested_tree() {
        let before = r#"{"lockfileVersion": 1, "dependencies": {"a": {"version": "1.0.0"}}}"#;
        let after = r#"{"lockfileVersion": 1, "dependencies": {"a": {"version": "1.0.0", "dependencies": {"b": {"version": "2.0.0"}}}}}"#;
        assert_eq!(changes(lockfile, before, after), ["+ a > b 2.0.0"]);
    }
}
