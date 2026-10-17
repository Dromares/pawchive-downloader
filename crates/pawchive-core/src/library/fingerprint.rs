//! The change fingerprint of a post: SHA-256 over its title, text, embed, file list and
//! availability flags. `edited` alone misses posts that carry none (all 629 posts of one test
//! profile) and flag changes that leave it alone, so a post counts as updated when either differs.

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::model::Post;

#[derive(Serialize)]
struct Input<'a> {
    title: &'a str,
    content: &'a str,
    embed: Option<[&'a str; 3]>,
    /// `file` then `attachments`, de-duplicated by path, as `[name, path, preview_only]`.
    files: Vec<(&'a str, &'a str, bool)>,
    has_full: Option<bool>,
    preview_state: Option<&'a str>,
}

/// Lower-case hex SHA-256 of the post's canonical JSON. Field order is fixed by the struct above.
pub fn fingerprint(post: &Post) -> String {
    let mut seen = std::collections::HashSet::new();
    let files = post
        .file
        .iter()
        .chain(post.attachments.iter())
        .filter_map(|f| {
            let path = f.path.as_deref().filter(|p| !p.is_empty())?;
            seen.insert(path).then(|| (f.name.as_deref().unwrap_or(""), path, f.preview_only.unwrap_or(false)))
        })
        .collect();
    let input = Input {
        title: &post.title,
        content: &post.content,
        embed: post.embed.as_ref().map(|e| [e.url.as_str(), e.subject.as_str(), e.description.as_str()]),
        files,
        has_full: post.has_full,
        preview_state: post.preview_state.as_deref(),
    };
    // Serialising plain strings and bools cannot fail.
    let json = serde_json::to_vec(&input).unwrap_or_default();
    Sha256::digest(&json).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::parse_post;

    fn post(extra: &str) -> Post {
        parse_post(
            format!(
                r#"{{"id":"1","title":"t","content":"<p>x</p>","embed":{{}},"edited":null,
                "file":{{"name":"a.png","path":"/aa/a.png"}},
                "attachments":[{{"name":"a.png","path":"/aa/a.png"}},{{"name":"b.png","path":"/bb/b.png"}}]{extra}}}"#
            )
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn same_post_same_fingerprint_and_every_input_moves_it() {
        let base = fingerprint(&post(""));
        assert_eq!(base.len(), 64);
        assert_eq!(base, fingerprint(&post("")));
        let mut changed = post("");
        changed.title = "t2".into();
        assert_ne!(base, fingerprint(&changed));
        let mut changed = post("");
        changed.content.push('!');
        assert_ne!(base, fingerprint(&changed));
        let mut changed = post("");
        changed.attachments.pop();
        assert_ne!(base, fingerprint(&changed));
        let mut changed = post("");
        changed.file.as_mut().unwrap().preview_only = Some(true);
        assert_ne!(base, fingerprint(&changed));
        assert_ne!(base, fingerprint(&post(r#","has_full":false"#)));
        assert_ne!(base, fingerprint(&post(r#","preview_state":"pending""#)));
        let mut changed = post("");
        changed.embed = Some(crate::model::RawEmbed { url: "https://x.test".into(), ..Default::default() });
        assert_ne!(base, fingerprint(&changed));
    }

    #[test]
    fn edited_added_and_a_repeated_attachment_do_not_move_it() {
        let base = fingerprint(&post(""));
        let mut p = post("");
        p.edited = Some("2026-01-01T00:00:00".into());
        p.added = Some("2026-01-02T00:00:00".into());
        assert_eq!(base, fingerprint(&p));
    }
}
