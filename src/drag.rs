//! Typed, session-scoped drag payloads and their state reducers.
//!
//! Pointer geometry stays in the UI. Validation and mutations live here so a
//! payload cannot become an untyped API request or let a filtered view corrupt
//! hidden ordering state.

use std::collections::HashMap;

use crate::util::{SpotifyUriKind, spotify_uri};

/// Identity of the navigation/auth state in which a drag began.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scope(u64);

impl Scope {
    pub(crate) const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// A complete playlist revision from which reorder origins may be captured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistAuthority {
    playlist_id: String,
    snapshot_id: String,
    loaded_extent: u32,
    revision: u64,
    mutation_generation: u64,
}

impl PlaylistAuthority {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        context_uri: &str,
        playlist_id: &str,
        snapshot_id: Option<&str>,
        loaded_extent: usize,
        total: Option<u32>,
        complete: bool,
        revision: u64,
        mutation_generation: u64,
    ) -> Option<Self> {
        let snapshot_id = snapshot_id.filter(|snapshot| !snapshot.is_empty())?;
        let loaded_extent = u32::try_from(loaded_extent).ok()?;
        (valid_playlist_identity(context_uri, playlist_id)
            && complete
            && total == Some(loaded_extent))
        .then(|| Self {
            playlist_id: playlist_id.to_owned(),
            snapshot_id: snapshot_id.to_owned(),
            loaded_extent,
            revision,
            mutation_generation,
        })
    }

    pub fn playlist_id(&self) -> &str {
        &self.playlist_id
    }
}

/// An editable playlist row's authoritative server occurrence at drag start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistOrigin {
    playlist_id: String,
    snapshot_id: String,
    source_uri: String,
    source_index: u32,
    source_occurrence: u32,
    loaded_extent: u32,
    revision: u64,
    mutation_generation: u64,
}

impl PlaylistOrigin {
    pub fn capture(
        authority: &PlaylistAuthority,
        source_uri: &str,
        source_index: usize,
        source_occurrence: usize,
    ) -> Option<Self> {
        if spotify_uri(source_uri)?.kind() != SpotifyUriKind::Track {
            return None;
        }
        let source_index = u32::try_from(source_index).ok()?;
        let source_occurrence = u32::try_from(source_occurrence).ok()?;
        (source_index < authority.loaded_extent).then(|| Self {
            playlist_id: authority.playlist_id.clone(),
            snapshot_id: authority.snapshot_id.clone(),
            source_uri: source_uri.to_owned(),
            source_index,
            source_occurrence,
            loaded_extent: authority.loaded_extent,
            revision: authority.revision,
            mutation_generation: authority.mutation_generation,
        })
    }

    pub const fn source_index(&self) -> u32 {
        self.source_index
    }

    pub fn playlist_id(&self) -> &str {
        &self.playlist_id
    }

    pub fn source_uri(&self) -> &str {
        &self.source_uri
    }
}

/// One validated music track in hand. Multi-item and mixed-content payloads
/// are intentionally not representable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackPayload {
    uri: String,
    title: String,
    image: Option<String>,
    origin: Option<PlaylistOrigin>,
    scope: Scope,
}

impl TrackPayload {
    pub fn new(
        uri: String,
        title: String,
        image: Option<String>,
        origin: Option<PlaylistOrigin>,
        scope: Scope,
    ) -> Option<Self> {
        (spotify_uri(&uri)?.kind() == SpotifyUriKind::Track
            && origin
                .as_ref()
                .is_none_or(|origin| origin.source_uri == uri))
        .then_some(Self {
            uri,
            title,
            image,
            origin,
            scope,
        })
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn image(&self) -> Option<&str> {
        self.image.as_deref()
    }

    pub fn origin(&self) -> Option<&PlaylistOrigin> {
        self.origin.as_ref()
    }

    pub(crate) fn belongs_to(&self, scope: Scope) -> bool {
        self.scope == scope
    }
}

/// One music context in hand on the library sidebar. Shows/podcasts are not
/// accepted, keeping the drag feature inside its audited music scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextPayload {
    uri: String,
    title: String,
    image: Option<String>,
    kind: SpotifyUriKind,
    scope: Scope,
}

impl ContextPayload {
    pub fn new(uri: String, title: String, image: Option<String>, scope: Scope) -> Option<Self> {
        let kind = spotify_uri(&uri)?.kind();
        kind.is_sidebar_music().then_some(Self {
            uri,
            title,
            image,
            kind,
            scope,
        })
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn image(&self) -> Option<&str> {
        self.image.as_deref()
    }

    pub const fn kind(&self) -> SpotifyUriKind {
        self.kind
    }

    pub(crate) fn belongs_to(&self, scope: Scope) -> bool {
        self.scope == scope
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistMove {
    origin: PlaylistOrigin,
    insert_before: u32,
}

impl PlaylistMove {
    pub fn from_origin(origin: PlaylistOrigin, insert_before: usize) -> Option<Self> {
        let insert_before = u32::try_from(insert_before).ok()?;
        (insert_before <= origin.loaded_extent
            && insert_before != origin.source_index
            && insert_before != origin.source_index.saturating_add(1))
        .then_some(Self {
            origin,
            insert_before,
        })
    }

    pub fn playlist_id(&self) -> &str {
        self.origin.playlist_id()
    }

    pub const fn from(&self) -> u32 {
        self.origin.source_index
    }

    pub const fn insert_before(&self) -> u32 {
        self.insert_before
    }

    pub fn snapshot_id(&self) -> &str {
        &self.origin.snapshot_id
    }

    pub fn source_uri(&self) -> &str {
        self.origin.source_uri()
    }

    pub fn matches_current(
        &self,
        authority: &PlaylistAuthority,
        source_uri: &str,
        source_occurrence: usize,
    ) -> bool {
        PlaylistOrigin::capture(
            authority,
            source_uri,
            self.origin.source_index as usize,
            source_occurrence,
        )
        .as_ref()
            == Some(&self.origin)
            && self.insert_before <= authority.loaded_extent
    }
}

/// Resolves an in-playlist drop to Spotify's insert-before coordinates.
/// Cross-playlist, stale, oversized, and own-edge drops are no-ops.
pub fn playlist_move(
    payload: &TrackPayload,
    scope: Scope,
    playlist_id: &str,
    slot: usize,
) -> Option<PlaylistMove> {
    if !payload.belongs_to(scope) {
        return None;
    }
    let origin = payload.origin()?;
    if origin.playlist_id() != playlist_id {
        return None;
    }
    PlaylistMove::from_origin(origin.clone(), slot)
}

/// A playlist row is a mutation target only when both API identifiers agree.
pub fn valid_playlist_identity(uri: &str, playlist_id: &str) -> bool {
    spotify_uri(uri).is_some_and(|parsed| {
        parsed.kind() == SpotifyUriKind::Playlist && parsed.id() == playlist_id
    })
}

/// Orders one complete playlist library for the sidebar. Pins always lead;
/// without a custom order the remainder follows recency, while a custom
/// order keeps newly discovered playlists between pins and its saved rows.
/// The caller supplies the frame's precomputed first-occurrence pin ranks.
pub fn playlist_sidebar_indices(
    uris: &[&str],
    recent: &[String],
    pinned: &HashMap<&str, usize>,
    custom: &[String],
) -> Vec<usize> {
    fn first_ranks(values: &[String]) -> HashMap<&str, usize> {
        let mut ranks = HashMap::with_capacity(values.len());
        for (index, value) in values.iter().enumerate() {
            ranks.entry(value.as_str()).or_insert(index);
        }
        ranks
    }
    let recent = first_ranks(recent);
    let custom = first_ranks(custom);
    let has_custom_order = !custom.is_empty();
    let mut indices: Vec<usize> = (0..uris.len()).collect();
    indices.sort_by_key(|index| {
        let uri = uris[*index];
        if let Some(rank) = pinned.get(uri) {
            (0, *rank, 0)
        } else if !has_custom_order {
            (1, recent.get(uri).copied().unwrap_or(usize::MAX), *index)
        } else if let Some(rank) = custom.get(uri) {
            (2, *rank, 0)
        } else {
            (1, *index, 0)
        }
    });
    indices
}

fn valid_layout(
    visible: &[String],
    liked_rows: usize,
    pinned_rows: usize,
    slot: usize,
    uri: &str,
) -> bool {
    liked_rows.saturating_add(pinned_rows) <= visible.len()
        && slot <= visible.len()
        && visible.iter().any(|held| held == uri)
}

fn pin_at(
    pinned: &mut Vec<String>,
    visible: &[String],
    liked_rows: usize,
    section_end: usize,
    slot: usize,
    uri: &str,
) {
    let anchor = visible[liked_rows..section_end]
        .iter()
        .skip(slot.saturating_sub(liked_rows))
        .find(|held| held.as_str() != uri)
        .cloned();
    pinned.retain(|held| held != uri);
    let at = anchor
        .and_then(|anchor| pinned.iter().position(|held| *held == anchor))
        .unwrap_or(pinned.len());
    pinned.insert(at, uri.to_owned());
}

/// Pins, reorders, or unpins an album/artist row while preserving pins from
/// the other shelves around the visible anchors.
pub fn drop_pinned_context(
    pinned: &mut Vec<String>,
    visible: &[String],
    liked_rows: usize,
    pinned_rows: usize,
    slot: usize,
    uri: &str,
) -> bool {
    if !valid_layout(visible, liked_rows, pinned_rows, slot, uri) {
        return false;
    }
    let before = pinned.clone();
    let section_end = liked_rows + pinned_rows;
    if slot <= section_end {
        pin_at(pinned, visible, liked_rows, section_end, slot, uri);
    } else {
        pinned.retain(|held| held != uri);
    }
    *pinned != before
}

/// Applies the playlist shelf's two-level ordering: pins stay in their own
/// block, while a drop below that block establishes or updates the custom
/// order of the remaining complete library. `full_order` must include hidden
/// rows, so filtering cannot discard them.
#[expect(clippy::too_many_arguments)]
pub fn drop_playlist_context(
    pinned: &mut Vec<String>,
    custom_order: &mut Vec<String>,
    visible: &[String],
    full_order: &[String],
    liked_rows: usize,
    pinned_rows: usize,
    slot: usize,
    uri: &str,
) -> bool {
    if spotify_uri(uri).is_none_or(|parsed| parsed.kind() != SpotifyUriKind::Playlist)
        || !valid_layout(visible, liked_rows, pinned_rows, slot, uri)
    {
        return false;
    }
    let before_pinned = pinned.clone();
    let before_order = custom_order.clone();
    let section_end = liked_rows + pinned_rows;
    if pinned_rows > 0 && slot < section_end {
        pin_at(pinned, visible, liked_rows, section_end, slot, uri);
        custom_order.retain(|held| held != uri);
        return *pinned != before_pinned || *custom_order != before_order;
    }

    let was_pinned = pinned.iter().any(|held| held == uri);
    if was_pinned {
        pinned.retain(|held| held != uri);
        if custom_order.is_empty() {
            return true;
        }
    }

    let anchor = visible
        .iter()
        .skip(slot)
        .filter(|held| !held.is_empty())
        .find(|held| held.as_str() != uri)
        .cloned();
    let mut order = full_order.to_vec();
    order.retain(|held| held != uri);
    let at = anchor
        .and_then(|anchor| order.iter().position(|held| *held == anchor))
        .unwrap_or(order.len());
    order.insert(at, uri.to_owned());
    *custom_order = order;
    *pinned != before_pinned || *custom_order != before_order
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope::default()
    }

    fn track(origin: Option<PlaylistOrigin>) -> TrackPayload {
        TrackPayload::new(
            "spotify:track:t1".into(),
            "One".into(),
            None,
            origin,
            scope(),
        )
        .expect("valid track")
    }

    #[test]
    fn payload_types_reject_invalid_and_non_music_content() {
        assert!(TrackPayload::new("".into(), "Empty".into(), None, None, scope()).is_none());
        assert!(
            TrackPayload::new(
                "spotify:episode:e1".into(),
                "Episode".into(),
                None,
                None,
                scope(),
            )
            .is_none()
        );
        assert!(
            ContextPayload::new("spotify:show:s1".into(), "Show".into(), None, scope()).is_none()
        );
        assert_eq!(
            ContextPayload::new("spotify:album:a1".into(), "Album".into(), None, scope(),)
                .expect("music context")
                .kind(),
            SpotifyUriKind::Album
        );
    }

    #[test]
    fn playlist_moves_require_current_matching_origin_and_skip_own_edges() {
        let authority = PlaylistAuthority::new(
            "spotify:playlist:p1",
            "p1",
            Some("snapshot"),
            4,
            Some(4),
            true,
            7,
            3,
        )
        .expect("complete playlist authority");
        let origin =
            PlaylistOrigin::capture(&authority, "spotify:track:t1", 2, 0).expect("matching origin");
        assert!(
            PlaylistAuthority::new(
                "spotify:album:p1",
                "p1",
                Some("snapshot"),
                4,
                Some(4),
                true,
                7,
                3,
            )
            .is_none()
        );
        assert!(
            PlaylistAuthority::new(
                "spotify:playlist:p1",
                "p2",
                Some("snapshot"),
                4,
                Some(4),
                true,
                7,
                3,
            )
            .is_none()
        );
        let payload = track(Some(origin));
        let movement = playlist_move(&payload, scope(), "p1", 0).expect("valid move");
        assert_eq!(movement.from(), 2);
        assert_eq!(movement.insert_before(), 0);
        assert_eq!(playlist_move(&payload, scope(), "p1", 2), None);
        assert_eq!(playlist_move(&payload, scope(), "p1", 3), None);
        assert_eq!(playlist_move(&payload, scope(), "p2", 0), None);
        assert_eq!(playlist_move(&payload, scope().next(), "p1", 0), None);
    }

    #[test]
    fn pin_reordering_preserves_other_shelves_and_unpins_below_the_block() {
        let visible = vec![
            "spotify:album:a1".into(),
            "spotify:album:a2".into(),
            "spotify:album:a3".into(),
        ];
        let mut pinned = vec!["spotify:artist:r1".into(), "spotify:album:a1".into()];
        assert!(drop_pinned_context(
            &mut pinned,
            &visible,
            0,
            1,
            0,
            "spotify:album:a2",
        ));
        assert_eq!(
            pinned,
            vec![
                "spotify:artist:r1".to_string(),
                "spotify:album:a2".to_string(),
                "spotify:album:a1".to_string(),
            ]
        );
        assert!(drop_pinned_context(
            &mut pinned,
            &visible,
            0,
            2,
            3,
            "spotify:album:a2",
        ));
        assert!(!pinned.contains(&"spotify:album:a2".to_string()));
    }

    #[test]
    fn playlist_sidebar_order_has_one_pin_recency_and_custom_policy() {
        let uris = ["p1", "p2", "p3", "new"];
        let recent = vec!["p3".into(), "p1".into()];
        let pinned = HashMap::from([("p2", 0)]);
        assert_eq!(
            playlist_sidebar_indices(&uris, &recent, &pinned, &[]),
            vec![1, 2, 0, 3]
        );

        let custom = vec!["p1".into(), "p3".into()];
        assert_eq!(
            playlist_sidebar_indices(&uris, &recent, &pinned, &custom),
            vec![1, 3, 0, 2]
        );
    }

    #[test]
    fn playlist_target_identity_is_strict() {
        assert!(valid_playlist_identity("spotify:playlist:mix", "mix"));
        for (uri, id) in [
            ("", "mix"),
            ("spotify:playlist:", "mix"),
            ("spotify:album:mix", "mix"),
            ("spotify:playlist:other", "mix"),
            ("spotify:playlist:mix:extra", "mix"),
            ("spotify:playlist:mix", ""),
        ] {
            assert!(!valid_playlist_identity(uri, id), "accepted {uri:?}/{id:?}");
        }
    }

    #[test]
    fn playlist_authority_requires_a_complete_exact_snapshot() {
        let authority = |snapshot, extent, total, complete| {
            PlaylistAuthority::new(
                "spotify:playlist:mix",
                "mix",
                snapshot,
                extent,
                total,
                complete,
                4,
                2,
            )
        };
        assert!(authority(Some("snap"), 3, Some(3), true).is_some());
        assert!(authority(None, 3, Some(3), true).is_none());
        assert!(authority(Some(""), 3, Some(3), true).is_none());
        assert!(authority(Some("snap"), 2, Some(3), true).is_none());
        assert!(authority(Some("snap"), 3, Some(3), false).is_none());
    }

    #[test]
    fn sidebar_ranking_scales_by_precomputed_first_occurrences() {
        let uris: Vec<String> = (0..4_000).map(|index| format!("p{index}")).collect();
        let refs: Vec<&str> = uris.iter().map(String::as_str).collect();
        let recent = vec!["p3000".into(), "p3000".into(), "deleted".into()];
        let pinned = HashMap::from([("p3999", 0), ("missing", 2)]);
        let custom = vec![
            "unknown".into(),
            "p2000".into(),
            "p2000".into(),
            "p1000".into(),
        ];
        let ranked = playlist_sidebar_indices(&refs, &recent, &pinned, &custom);
        let mut expected = vec![3999];
        expected.extend((0..4_000).filter(|index| !matches!(index, 1000 | 2000 | 3999)));
        expected.extend([2000, 1000]);
        assert_eq!(ranked, expected);

        let without_custom = playlist_sidebar_indices(&refs, &recent, &pinned, &[]);
        assert_eq!(&without_custom[..3], &[3999, 3000, 0]);
    }

    #[test]
    fn playlist_order_keeps_pins_and_hidden_rows_separate() {
        let p = |id: &str| format!("spotify:playlist:{id}");
        let visible = vec![String::new(), p("pin"), p("three"), p("one")];
        let full = vec![p("one"), p("hidden"), p("two"), p("three")];
        let mut pinned = vec![p("pin")];
        let mut custom = Vec::new();
        assert!(drop_playlist_context(
            &mut pinned,
            &mut custom,
            &visible,
            &full,
            1,
            1,
            3,
            &p("three"),
        ));
        assert_eq!(pinned, vec![p("pin")]);
        assert_eq!(custom, vec![p("three"), p("one"), p("hidden"), p("two")]);

        assert!(drop_playlist_context(
            &mut pinned,
            &mut custom,
            &visible,
            &full,
            1,
            1,
            1,
            &p("three"),
        ));
        assert_eq!(pinned, vec![p("three"), p("pin")]);
        assert!(!custom.contains(&p("three")));
    }
}
