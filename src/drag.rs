//! Typed, session-scoped drag payloads and their state reducers.
//!
//! Pointer geometry stays in the UI. Validation and mutations live here so a
//! payload cannot become an untyped API request or let a filtered view corrupt
//! hidden ordering state.

use crate::util::{SpotifyUriKind, spotify_uri};

/// Identity of the navigation/auth state in which a drag began.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scope(u64);

impl Scope {
    pub(crate) const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// An editable playlist row's authoritative server position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistOrigin {
    playlist_id: String,
    index: u32,
}

impl PlaylistOrigin {
    /// Builds an origin only when the context URI and editable id describe
    /// the same validated playlist.
    pub fn from_context(context_uri: &str, editable_id: &str, index: usize) -> Option<Self> {
        let context = spotify_uri(context_uri)?;
        let index = u32::try_from(index).ok()?;
        (context.kind() == SpotifyUriKind::Playlist && context.id() == editable_id).then(|| Self {
            playlist_id: editable_id.to_owned(),
            index,
        })
    }

    pub const fn index(&self) -> u32 {
        self.index
    }

    pub fn playlist_id(&self) -> &str {
        &self.playlist_id
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
        (spotify_uri(&uri)?.kind() == SpotifyUriKind::Track).then_some(Self {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaylistMove {
    pub from: u32,
    pub insert_before: u32,
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
    let insert_before = u32::try_from(slot).ok()?;
    (insert_before != origin.index() && insert_before != origin.index().saturating_add(1))
        .then_some(PlaylistMove {
            from: origin.index(),
            insert_before,
        })
}

/// Orders one complete playlist library for the sidebar. Pins always lead;
/// without a custom order the remainder follows recency, while a custom
/// order keeps newly discovered playlists between pins and its saved rows.
pub fn playlist_sidebar_indices(
    uris: &[&str],
    recent: &[String],
    pinned: &[String],
    custom: &[String],
) -> Vec<usize> {
    let rank = |values: &[String], uri: &str| values.iter().position(|held| held == uri);
    let mut indices: Vec<usize> = (0..uris.len()).collect();
    indices.sort_by_key(|index| {
        let uri = uris[*index];
        if let Some(rank) = rank(pinned, uri) {
            (0, rank, 0)
        } else if custom.is_empty() {
            (1, rank(recent, uri).unwrap_or(usize::MAX), *index)
        } else if let Some(rank) = rank(custom, uri) {
            (2, rank, 0)
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
        let origin =
            PlaylistOrigin::from_context("spotify:playlist:p1", "p1", 2).expect("matching origin");
        assert!(PlaylistOrigin::from_context("spotify:album:p1", "p1", 2).is_none());
        assert!(PlaylistOrigin::from_context("spotify:playlist:p1", "p2", 2).is_none());
        let payload = track(Some(origin));
        assert_eq!(
            playlist_move(&payload, scope(), "p1", 0),
            Some(PlaylistMove {
                from: 2,
                insert_before: 0,
            })
        );
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
        let pinned = vec!["p2".into()];
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
