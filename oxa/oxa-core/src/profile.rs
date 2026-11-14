//! `oxa.profile` in the core (OXA-LOCAL-FIRST sec. 6, T1): what an epoch declares under each
//! profile, and how the profile is stored with the epoch.
//!
//! The profile is read once per prefix epoch (`epoch.rs`, from the settings and `OXA_PROFILE`)
//! and kept in `Stats::profile`. Under `local` a `custom` entry [`PROFILE_ENTRY`] follows the
//! epoch's `Prefix` entry (never in context); under `pi` nothing is written, so a `pi` session
//! file is byte for byte what it was before profiles existed. Resume replays the stored prefix
//! (W3) and takes the profile from the same branch ([`profile_of_branch`]): a session keeps the
//! profile it was created with until a new epoch opens.
//!
//! A profile never adds or removes a capability (O-167): `local` changes how the `edit` tool is
//! declared (T1) and the default thinking policy (T5), nothing else.
use oxa_proto::{Entry, EntryBody, Profile, Source, ToolSpec, Value};
use oxa_tools::descriptions;

/// The `custom` entry type that records a non-`pi` profile of an epoch. `data` = the name.
pub const PROFILE_ENTRY: &str = "oxa.profile";

/// The specs an epoch of `profile` declares. `pi`: unchanged. `local`: every built-in is declared
/// with its profile `local` text ([`descriptions::local_specs::by_name`]: integer parameters
/// (T3); `edit` flat, [`descriptions::EDIT_FLAT`]). Each keeps its exposure. Extension and MCP
/// tools are never touched.
pub fn declared_specs(mut specs: Vec<ToolSpec>, profile: Profile) -> Vec<ToolSpec> {
    if profile == Profile::Local {
        for s in specs.iter_mut() {
            if s.source != Source::Builtin {
                continue;
            }
            let Some(text) = descriptions::local_specs::by_name(&s.name) else {
                continue;
            };
            let exposure = s.exposure;
            *s = oxa_tools::spec_of(text);
            s.exposure = exposure;
        }
    }
    specs
}

/// The entry that stores `profile` with a new epoch; `None` under `pi`.
pub fn profile_entry(profile: Profile) -> Option<EntryBody> {
    (profile != Profile::Pi).then(|| EntryBody::Custom {
        custom_type: PROFILE_ENTRY.to_string(),
        data: Value::from(profile.as_str()),
    })
}

/// The profile of the epoch in force at the end of `branch`: a [`PROFILE_ENTRY`] after the last
/// `Prefix` entry, else `pi` (also for files written before profiles existed).
pub fn profile_of_branch(branch: &[Entry]) -> Profile {
    let start = branch
        .iter()
        .rposition(|e| matches!(e.body, EntryBody::Prefix(_)))
        .map_or(0, |i| i + 1);
    branch[start..]
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            EntryBody::Custom { custom_type, data } if custom_type == PROFILE_ENTRY => {
                data.as_str().and_then(Profile::parse)
            }
            _ => None,
        })
        .unwrap_or(Profile::Pi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::{Exposure, FrozenPrefix};

    fn entry(id: &str, body: EntryBody) -> Entry {
        Entry {
            id: id.to_string(),
            parent: None,
            ts_ms: 0,
            body,
        }
    }
    fn prefix() -> EntryBody {
        EntryBody::Prefix(FrozenPrefix {
            sections: Vec::new(),
            tools_json: String::from("[]"),
            sha256: [0; 32],
        })
    }

    #[test]
    fn pi_declares_pi_edit_and_local_declares_it_flat() {
        let specs: Vec<ToolSpec> = descriptions::ALL.iter().map(|t| oxa_tools::spec_of(t)).collect();
        assert_eq!(declared_specs(specs.clone(), Profile::Pi), specs);
        let local = declared_specs(specs.clone(), Profile::Local);
        for (a, b) in specs.iter().zip(&local) {
            let text = descriptions::local_specs::by_name(&a.name).expect("a built-in");
            assert_eq!(b.name, a.name);
            assert_eq!(b.schema_json, text.schema_json, "{}", a.name);
            assert_eq!(b.description, text.description, "{}", a.name);
            assert_eq!(b.exposure, a.exposure);
            assert_eq!(b.class, a.class);
            assert_eq!(b.source, a.source);
        }
        let edit = local.iter().find(|s| s.name == "edit").unwrap();
        assert_eq!(edit.schema_json, descriptions::EDIT_FLAT.schema_json);
        assert_eq!(edit.description, descriptions::EDIT_FLAT.description);
        // An extension tool named `edit` is not a built-in and stays as declared.
        let mut ext = oxa_tools::spec_of(&descriptions::EDIT);
        ext.source = Source::Extension("x.js".to_string());
        ext.exposure = Exposure::Direct;
        assert_eq!(declared_specs(vec![ext.clone()], Profile::Local), vec![ext]);
    }

    #[test]
    fn the_flat_schema_has_three_required_strings_and_no_array() {
        let v: Value = serde_json::from_str(descriptions::EDIT_FLAT.schema_json).unwrap();
        let props = v["properties"].as_object().unwrap();
        assert_eq!(props.len(), 3);
        for p in ["path", "oldText", "newText"] {
            assert_eq!(props[p]["type"], "string", "{p}");
        }
        assert_eq!(v["required"], serde_json::json!(["path", "oldText", "newText"]));
        assert!(!descriptions::EDIT_FLAT.schema_json.contains("array"));
        // Only the parameter part differs from pi's text: same first sentence, same path text.
        let first = |d: &str| d.split(". ").next().unwrap().to_string();
        assert_eq!(
            first(descriptions::EDIT_FLAT.description),
            first(descriptions::EDIT.description)
        );
        let pi: Value = serde_json::from_str(descriptions::EDIT.schema_json).unwrap();
        assert_eq!(props["path"], pi["properties"]["path"]);
    }

    #[test]
    fn the_profile_is_read_from_the_last_epoch() {
        let local = profile_entry(Profile::Local).unwrap();
        assert_eq!(profile_entry(Profile::Pi), None);
        assert_eq!(profile_of_branch(&[]), Profile::Pi);
        assert_eq!(profile_of_branch(&[entry("a", prefix())]), Profile::Pi);
        let b = [entry("a", prefix()), entry("b", local.clone())];
        assert_eq!(profile_of_branch(&b), Profile::Local);
        // A later epoch without the marker is a `pi` epoch.
        let b = [entry("a", prefix()), entry("b", local), entry("c", prefix())];
        assert_eq!(profile_of_branch(&b), Profile::Pi);
    }
}
