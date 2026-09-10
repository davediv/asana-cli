//! Project lookup for `project view`: a gid or URL is used as is, anything
//! else is a name matched against the workspace's projects.
//!
//! This is `resolve::resolve_project` with one more tolerance: Asana users
//! put an emoji in front of project names (`⚙️ UTL`, `👨‍⚖️ LEGAL`), and
//! nobody wants to type one, so a name is also matched with its leading
//! symbols stripped (`UTL` finds `⚙️ UTL`). Exact matches win over prefix
//! matches, and the full name wins over the bare one, so adding a project
//! called `UTL` next to `⚙️ UTL` never changes what `⚙️ UTL` resolves to.

use anyhow::{bail, Result};

use crate::api::Named;
use crate::ctx::Ctx;
use crate::resolve::{parse_project_gid, IdKind, InvalidIdentifier};

/// The gid of the project `input` names; a network round trip only for a
/// name.
pub async fn resolve_gid(ctx: &Ctx, input: &str) -> Result<String> {
    if let Some(gid) = parse_project_gid(input) {
        return Ok(gid);
    }
    let name = input.trim();
    if name.is_empty() {
        return Err(InvalidIdentifier::new(IdKind::Project, input).into());
    }
    let ws = ctx.workspace().await?;
    let projects: Vec<Named> = ctx
        .api
        .get_all(
            "/projects",
            &[("workspace", ws.gid.as_str()), ("opt_fields", "name")],
            None,
        )
        .await?;
    match pick(&projects, name) {
        Match::One(p) => Ok(p.gid.clone()),
        Match::None => bail!("no project named \"{name}\" in {}", ws.display()),
        Match::Many(many) => {
            let mut msg = format!(
                "\"{name}\" matches {} projects in {}:",
                many.len(),
                ws.display()
            );
            for p in many {
                msg.push_str(&format!("\n  {} ({})", p.display(), p.gid));
            }
            msg.push_str("\nUse the gid to pick one.");
            bail!(msg)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Match<'a> {
    One(&'a Named),
    Many(Vec<&'a Named>),
    None,
}

/// `name` without whatever decorates its front: emoji, their variation
/// selectors and joiners, punctuation, spaces. Empty when nothing but
/// decoration is left (`🚀`), so a bare match is never a match on nothing.
fn bare(name: &str) -> &str {
    name.trim_start_matches(|c: char| !c.is_alphanumeric())
}

/// Case-insensitive lookup, in tiers, stopping at the first tier that has
/// any candidate: exact name, exact bare name, name prefix, bare-name
/// prefix. A tier with several candidates is ambiguous.
pub fn pick<'a>(projects: &'a [Named], query: &str) -> Match<'a> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Match::None;
    }
    let named: Vec<(&Named, String)> = projects
        .iter()
        .filter_map(|p| p.name.as_deref().map(|n| (p, n.to_lowercase())))
        .collect();
    let tiers: [&dyn Fn(&str) -> bool; 4] = [
        &|n: &str| n == query,
        &|n: &str| {
            let b = bare(n);
            !b.is_empty() && b == query
        },
        &|n: &str| n.starts_with(&query),
        &|n: &str| {
            let b = bare(n);
            !b.is_empty() && b.starts_with(&query)
        },
    ];
    for tier in tiers {
        let hits: Vec<&Named> = named
            .iter()
            .filter(|(_, lower)| tier(lower))
            .map(|(p, _)| *p)
            .collect();
        match hits.as_slice() {
            [] => continue,
            [one] => return Match::One(one),
            _ => return Match::Many(hits),
        }
    }
    Match::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(gid: &str, name: &str) -> Named {
        Named {
            gid: gid.into(),
            name: Some(name.into()),
            email: None,
        }
    }

    fn projects() -> Vec<Named> {
        vec![
            named("1", "PR"),
            named("2", "⚙️ UTL"),
            named("3", "👾 FUN"),
            named("4", "👨\u{200d}⚖️ LEGAL"),
            named("5", "HCK-RC"),
            named("6", "HCK-AG"),
            named("7", "Roadmap"),
            named("8", "🚀"),
            Named {
                gid: "9".into(),
                name: None,
                email: None,
            },
        ]
    }

    fn gid_of(m: Match<'_>) -> Option<String> {
        match m {
            Match::One(p) => Some(p.gid.clone()),
            _ => None,
        }
    }

    #[test]
    fn bare_strips_leading_decoration_only() {
        assert_eq!(bare("⚙️ UTL"), "UTL");
        assert_eq!(bare("👨\u{200d}⚖️ LEGAL"), "LEGAL");
        assert_eq!(bare("#1 Priority"), "1 Priority");
        assert_eq!(bare("Roadmap 🚀"), "Roadmap 🚀");
        assert_eq!(bare("🚀"), "");
        assert_eq!(bare("  spaced"), "spaced");
    }

    #[test]
    fn exact_names_with_or_without_the_emoji() {
        let p = projects();
        assert_eq!(gid_of(pick(&p, "⚙️ UTL")).as_deref(), Some("2"));
        assert_eq!(gid_of(pick(&p, "UTL")).as_deref(), Some("2"));
        assert_eq!(gid_of(pick(&p, "utl")).as_deref(), Some("2"));
        assert_eq!(gid_of(pick(&p, " legal ")).as_deref(), Some("4"));
        assert_eq!(gid_of(pick(&p, "👨\u{200d}⚖️ LEGAL")).as_deref(), Some("4"));
        assert_eq!(gid_of(pick(&p, "roadmap")).as_deref(), Some("7"));
        assert_eq!(gid_of(pick(&p, "🚀")).as_deref(), Some("8"));
    }

    #[test]
    fn prefixes_are_unique_or_ambiguous() {
        let p = projects();
        assert_eq!(gid_of(pick(&p, "road")).as_deref(), Some("7"));
        assert_eq!(gid_of(pick(&p, "fu")).as_deref(), Some("3"));
        assert_eq!(gid_of(pick(&p, "leg")).as_deref(), Some("4"));
        match pick(&p, "hck") {
            Match::Many(many) => assert_eq!(many.len(), 2),
            other => panic!("expected two HCK projects, got {other:?}"),
        }
        assert_eq!(pick(&p, "no-such-project-zzz"), Match::None);
        assert_eq!(pick(&p, ""), Match::None);
    }

    #[test]
    fn the_full_name_beats_the_bare_one() {
        let mut p = projects();
        p.push(named("10", "UTL"));
        assert_eq!(gid_of(pick(&p, "UTL")).as_deref(), Some("10"));
        assert_eq!(gid_of(pick(&p, "⚙️ UTL")).as_deref(), Some("2"));
        // An exact bare match beats a plain prefix match.
        p.push(named("11", "PRIMARY"));
        assert_eq!(gid_of(pick(&p, "pr")).as_deref(), Some("1"));
        p.push(named("12", "🔥 PR"));
        match pick(&p, "pr") {
            Match::One(one) => assert_eq!(one.gid, "1"),
            other => panic!("exact name should win, got {other:?}"),
        }
    }
}
