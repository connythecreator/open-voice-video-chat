//! Turning `@name` text into roster ids.
//!
//! Mentions are stored as [`AuthorId`]s, never as the text the sender typed, so
//! display-name changes cannot redirect a mention that was already sent, and a
//! peer that joins later resolves the same ids against the same roster. This
//! module is the one parser both the CLI and the TUI use, so `@alice` means the
//! same thing in both.
//!
//! Resolution rules, in order:
//!
//! * `@display name` matches every member whose display name equals the text
//!   following the `@` (case-insensitive). The longest roster name wins, and
//!   the match must end at a word boundary, so a message like "mail me at
//!   @home" does not notify a member called "ho".
//! * `@hex-prefix` (4 to 64 hex digits) matches every member whose id starts
//!   with that prefix. This is how someone can be mentioned unambiguously when
//!   two people share a display name.
//!
//! Unresolvable tokens are left as literal text; they are not an error.

use crate::model::{AuthorId, MAX_MENTIONS, MemberInfo};

/// Parse the mentions in `text` against `members`.
///
/// The result preserves first-appearance order and is de-duplicated. It is
/// capped at [`MAX_MENTIONS`] distinct ids.
pub fn parse_mentions(text: &str, members: &[MemberInfo]) -> Vec<AuthorId> {
    let mut mentions = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        let after = &rest[at + 1..];
        if after.is_empty() {
            break;
        }

        let name_match = names_matching(after, members);
        let mut authors = name_match.authors;
        let mut consumed = name_match.consumed;
        if authors.is_empty() {
            let token_len = after
                .char_indices()
                .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.'))
                .map(|(index, _)| index)
                .unwrap_or(after.len());
            authors = ids_matching(&after[..token_len], members);
            consumed = token_len;
        }

        for author in authors {
            if !mentions.contains(&author) {
                mentions.push(author);
                if mentions.len() == MAX_MENTIONS {
                    return mentions;
                }
            }
        }
        // Always make progress past the `@`. Skipping one *character* (not one
        // byte) keeps non-ASCII text like "@é" from slicing mid-codepoint.
        let advance = if consumed > 0 {
            consumed
        } else {
            after.chars().next().map_or(1, char::len_utf8)
        };
        rest = &after[advance..];
    }
    mentions
}

/// Whether `text` contains an `@` token this parser could resolve. Used by
/// composer UIs to decide whether a mention lookup is worth doing.
pub fn has_mention_syntax(text: &str) -> bool {
    text.contains('@')
}

struct NameMatch {
    /// Bytes of the text following `@` consumed by the matched name.
    consumed: usize,
    authors: Vec<AuthorId>,
}

/// Longest roster display name that appears at the start of `after`, at a word
/// boundary, compared case-insensitively.
fn names_matching(after: &str, members: &[MemberInfo]) -> NameMatch {
    let lowered_after = after.to_lowercase();
    let mut best: Option<(usize, &str)> = None;
    for member in members {
        let name = member.display_name.trim();
        if name.is_empty() {
            continue;
        }
        let lowered = name.to_lowercase();
        if !lowered_after.starts_with(&lowered) {
            continue;
        }
        let boundary = lowered_after[lowered.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.'));
        if !boundary {
            continue;
        }
        if best.is_none_or(|(len, _)| lowered.len() > len) {
            best = Some((lowered.len(), name));
        }
    }
    let Some((consumed, name)) = best else {
        return NameMatch {
            consumed: 0,
            authors: Vec::new(),
        };
    };
    // Lowercasing can change byte length in exotic scripts; only consume when
    // the boundary is a real char boundary in the original text.
    if !after.is_char_boundary(consumed) {
        return NameMatch {
            consumed: 0,
            authors: Vec::new(),
        };
    }
    let lowered_name = name.to_lowercase();
    let mut authors: Vec<AuthorId> = Vec::new();
    for member in members {
        if member.display_name.trim().to_lowercase() == lowered_name
            && !authors.contains(&member.author)
        {
            authors.push(member.author);
        }
    }
    NameMatch { consumed, authors }
}

fn ids_matching(token: &str, members: &[MemberInfo]) -> Vec<AuthorId> {
    let lowered = token.to_lowercase();
    if !(4..=64).contains(&lowered.len()) || !lowered.chars().all(|c| c.is_ascii_hexdigit()) {
        return Vec::new();
    }
    let mut out: Vec<AuthorId> = Vec::new();
    for member in members {
        if member.author.to_hex().starts_with(&lowered) && !out.contains(&member.author) {
            out.push(member.author);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use crate::model::GuildId;

    fn member(identity: &Identity, name: &str) -> MemberInfo {
        MemberInfo {
            guild_id: GuildId([1; 32]),
            author: identity.author_id(),
            display_name: name.to_string(),
            joined_at: 0,
        }
    }

    #[test]
    fn names_resolve_case_insensitively_and_dedupe() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let members = vec![member(&alice, "Alice"), member(&bob, "bob")];
        let text = "hey @alice and @ALICE, cc @bob@alice";
        assert_eq!(
            parse_mentions(text, &members),
            vec![alice.author_id(), bob.author_id()]
        );
    }

    #[test]
    fn id_prefixes_resolve_when_names_are_ambiguous() {
        let alice = Identity::generate();
        let mallory = Identity::generate();
        let members = vec![member(&alice, "alex"), member(&mallory, "alex")];
        // A shared display name reaches both members.
        assert_eq!(
            parse_mentions("@alex", &members),
            vec![alice.author_id(), mallory.author_id()]
        );
        // The id prefix reaches exactly one.
        let prefix = &mallory.author_id().to_hex()[..6];
        assert_eq!(
            parse_mentions(&format!("@{prefix}"), &members),
            vec![mallory.author_id()]
        );
    }

    #[test]
    fn names_with_spaces_match_the_longest_roster_name() {
        let mary = Identity::generate();
        let mary_jane = Identity::generate();
        let members = vec![member(&mary, "Mary"), member(&mary_jane, "Mary Jane")];
        assert_eq!(
            parse_mentions("@Mary Jane hi", &members),
            vec![mary_jane.author_id()]
        );
        assert_eq!(parse_mentions("@Mary hi", &members), vec![mary.author_id()]);
    }

    #[test]
    fn unknown_tokens_are_left_alone() {
        let alice = Identity::generate();
        let ho = Identity::generate();
        let members = vec![member(&alice, "alice"), member(&ho, "ho")];
        assert!(parse_mentions("mail me at @home", &members).is_empty());
        assert!(parse_mentions("@", &members).is_empty());
        // Unresolved non-ASCII after `@` must advance by a whole character.
        assert!(parse_mentions("mail @é and @alice", &members) == vec![alice.author_id()]);
        // Too short to be an unambiguous id prefix.
        assert!(
            parse_mentions(&format!("@{}", &alice.author_id().to_hex()[..3]), &members).is_empty()
        );
    }

    #[test]
    fn mention_count_is_capped() {
        let members: Vec<MemberInfo> = (0..MAX_MENTIONS + 5)
            .map(|index| {
                let identity = Identity::from_seed([index as u8 + 1; 32]);
                member(&identity, &format!("user{index}"))
            })
            .collect();
        let text = members
            .iter()
            .map(|member| format!("@{}", member.display_name))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(parse_mentions(&text, &members).len(), MAX_MENTIONS);
    }
}
