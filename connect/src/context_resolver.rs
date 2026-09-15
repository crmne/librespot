use crate::{
    core::{Error, Session},
    protocol::{
        autoplay_context_request::AutoplayContextRequest, context::Context,
        context_page::ContextPage, context_track::ContextTrack, transfer_state::TransferState,
    },
    state::{
        ConnectState, DJ_CONTEXT_METADATA_KEY, DJ_CONTEXT_METADATA_VALUE, context::ContextType,
    },
};
use std::{
    cmp::PartialEq,
    collections::{HashMap, VecDeque},
    fmt::{Display, Formatter},
    hash::Hash,
    time::Duration,
};
use thiserror::Error as ThisError;
use tokio::time::Instant;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum Resolve {
    Uri(String),
    Context(Context),
}

fn log_context_for_dj_debug(label: &str, ctx: &Context) {
    let volatile_context_id = ctx
        .metadata
        .get("playlist_volatile_context_id")
        .map(String::as_str)
        .unwrap_or("-");
    let transcript_id = ctx
        .metadata
        .get("home.card.dj.transcript_id")
        .map(String::as_str)
        .unwrap_or("-");
    let expiration = ctx
        .metadata
        .get("lexicon_expiration_time")
        .map(String::as_str)
        .unwrap_or("-");
    let correlation_id = ctx
        .metadata
        .get("correlation-id")
        .map(String::as_str)
        .unwrap_or("-");
    let first_tracks = ctx
        .pages
        .iter()
        .flat_map(|page| page.tracks.iter())
        .filter_map(context_track_uri)
        .take(5)
        .collect::<Vec<_>>();
    info!(
        "[DJDBG] {label}: uri={:?}, url={:?}, volatile_context_id={volatile_context_id:?}, transcript_id={transcript_id:?}, expires={expiration:?}, correlation_id={correlation_id:?}, first_tracks={first_tracks:?}, pages={}",
        ctx.uri,
        ctx.url,
        ctx.pages.len()
    );

    for (index, page) in ctx.pages.iter().enumerate() {
        info!(
            "[DJDBG] {label} page[{index}]: page_url={:?}, next_page_url={:?}, metadata={:?}, tracks={}",
            page.page_url,
            page.next_page_url,
            page.metadata,
            page.tracks.len()
        );
    }
}

fn find_hm_context_url(ctx: &Context) -> Option<(&str, &str)> {
    if let Some(url) = ctx
        .url
        .as_deref()
        .filter(|value| value.starts_with("hm://"))
    {
        return Some(("context.url", url));
    }

    ctx.metadata.iter().find_map(|(key, value)| {
        (key.contains("context") && key.contains("url") && value.starts_with("hm://"))
            .then_some((key.as_str(), value.as_str()))
    })
}

fn context_track_uri(track: &ContextTrack) -> Option<String> {
    track
        .uri
        .as_deref()
        .filter(|uri| !uri.is_empty())
        .or_else(|| {
            track
                .metadata
                .get("canonical_track_uri")
                .map(String::as_str)
                .filter(|uri| !uri.is_empty())
        })
        .map(ConnectState::normalize_unknown_track_uri)
}

/// Merge the one-track transfer context into the session context returned by
/// the Lexicon endpoint. The transfer context carries the authoritative DJ
/// marker and current track, while the session response carries per-track
/// narration metadata and the continuation pages.
fn merge_direct_dj_context(mut resolved: Context, direct: &Context) -> Context {
    for (key, value) in &direct.metadata {
        resolved
            .metadata
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }

    let direct_track = direct
        .pages
        .iter()
        .flat_map(|page| page.tracks.iter())
        .find(|track| context_track_uri(track).is_some())
        .cloned();

    if let Some(direct_track) = direct_track {
        let direct_uri = context_track_uri(&direct_track);
        let mut merged_track = None;

        if let Some(direct_uri) = direct_uri.as_deref() {
            'pages: for page in &mut resolved.pages {
                if let Some(index) = page
                    .tracks
                    .iter()
                    .position(|track| context_track_uri(track).as_deref() == Some(direct_uri))
                {
                    let mut track = page.tracks.remove(index);
                    for (key, value) in &direct_track.metadata {
                        track
                            .metadata
                            .entry(key.clone())
                            .or_insert_with(|| value.clone());
                    }
                    if track.uid.is_none() {
                        track.uid = direct_track.uid.clone();
                    }
                    merged_track = Some(track);
                    break 'pages;
                }
            }
        }

        let merged_track = merged_track.unwrap_or(direct_track);
        if let Some(first_page) = resolved
            .pages
            .iter_mut()
            .find(|page| !page.tracks.is_empty())
        {
            // Keep the transferred song at index zero so finishing the transfer
            // does not switch playback to the first song returned by Lexicon.
            first_page.tracks.insert(0, merged_track);
        } else {
            resolved.pages.push(ContextPage {
                tracks: vec![merged_track],
                ..Default::default()
            });
        }
    }

    // Keep the Connect context identity used by the transfer. The HM URL is a
    // resolver hint, not a replacement for the session's Spotify context URI.
    if direct.uri.is_some() {
        resolved.uri = direct.uri.clone();
    }
    if direct.url.is_some() {
        resolved.url = direct.url.clone();
    }

    resolved
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(super) enum ContextAction {
    Append,
    Replace,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(super) struct ResolveContext {
    resolve: Resolve,
    fallback: Option<String>,
    update: ContextType,
    action: ContextAction,
}

impl ResolveContext {
    fn append_context(uri: impl Into<String>) -> Self {
        Self {
            resolve: Resolve::Uri(uri.into()),
            fallback: None,
            update: ContextType::Default,
            action: ContextAction::Append,
        }
    }

    pub fn from_uri(
        uri: impl Into<String>,
        fallback: impl Into<String>,
        update: ContextType,
        action: ContextAction,
    ) -> Self {
        let fallback_uri = fallback.into();
        Self {
            resolve: Resolve::Uri(uri.into()),
            fallback: (!fallback_uri.is_empty()).then_some(fallback_uri),
            update,
            action,
        }
    }

    pub fn from_context(context: Context, update: ContextType, action: ContextAction) -> Self {
        Self {
            resolve: Resolve::Context(context),
            fallback: None,
            update,
            action,
        }
    }

    /// the uri which should be used to resolve the context, might not be the context uri
    fn resolve_uri(&self) -> Option<&str> {
        // it's important to call this always, or at least for every ResolveContext
        // otherwise we might not even check if we need to fallback and just use the fallback uri
        match self.resolve {
            Resolve::Uri(ref uri) => ConnectState::valid_resolve_uri(uri),
            Resolve::Context(ref ctx) => {
                ConnectState::find_valid_uri(ctx.uri.as_deref(), ctx.pages.first())
            }
        }
        .or(self.fallback.as_deref())
    }

    /// the actual context uri
    fn context_uri(&self) -> &str {
        match self.resolve {
            Resolve::Uri(ref uri) => uri,
            Resolve::Context(ref ctx) => ctx.uri.as_deref().unwrap_or_default(),
        }
    }
}

impl Display for ResolveContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "resolve_uri: <{:?}>, context_uri: <{}>, update: <{:?}>",
            self.resolve_uri(),
            self.context_uri(),
            self.update,
        )
    }
}

#[derive(Debug, ThisError)]
enum ContextResolverError {
    #[error("no next context to resolve")]
    NoNext,
    #[error("tried appending context with {0} pages")]
    UnexpectedPagesSize(usize),
    #[error("tried resolving not allowed context: {0:?}")]
    NotAllowedContext(String),
}

impl From<ContextResolverError> for Error {
    fn from(value: ContextResolverError) -> Self {
        Error::failed_precondition(value)
    }
}

pub struct ContextResolver {
    session: Session,
    queue: VecDeque<ResolveContext>,
    unavailable_contexts: HashMap<ResolveContext, Instant>,
}

// time after which an unavailable context is retried
const RETRY_UNAVAILABLE: Duration = Duration::from_secs(3600);

impl ContextResolver {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            queue: VecDeque::new(),
            unavailable_contexts: HashMap::new(),
        }
    }

    pub fn add(&mut self, resolve: ResolveContext) {
        let last_try = self
            .unavailable_contexts
            .get(&resolve)
            .map(|i| i.duration_since(Instant::now()));

        let last_try = if matches!(last_try, Some(last_try) if last_try > RETRY_UNAVAILABLE) {
            let _ = self.unavailable_contexts.remove(&resolve);
            debug!(
                "context was requested {}s ago, trying again to resolve the requested context",
                last_try.expect("checked by condition").as_secs()
            );
            None
        } else {
            last_try
        };

        if last_try.is_some() {
            debug!("tried loading unavailable context: {resolve}");
            return;
        } else if self.queue.contains(&resolve) {
            debug!("update for {resolve} is already added");
            return;
        } else {
            trace!(
                "added {} to resolver queue",
                resolve.resolve_uri().unwrap_or(resolve.context_uri())
            )
        }

        self.queue.push_back(resolve)
    }

    pub fn add_list(&mut self, resolve: Vec<ResolveContext>) {
        for resolve in resolve {
            self.add(resolve)
        }
    }

    pub fn remove_used_and_invalid(&mut self) {
        if let Some((_, _, remove)) = self.find_next() {
            let _ = self.queue.drain(0..remove); // remove invalid
        }
        self.queue.pop_front(); // remove used
    }

    pub fn clear(&mut self) {
        self.queue = VecDeque::new()
    }

    fn find_next(&self) -> Option<(&ResolveContext, &str, usize)> {
        for idx in 0..self.queue.len() {
            let next = self.queue.get(idx)?;
            match next.resolve_uri() {
                None => {
                    warn!("skipped {idx} because of invalid resolve_uri: {next}");
                    continue;
                }
                Some(uri) => return Some((next, uri, idx)),
            }
        }
        None
    }

    pub fn has_next(&self) -> bool {
        self.find_next().is_some()
    }

    pub async fn get_next_context(
        &self,
        recent_track_uri: impl Fn() -> Vec<String>,
    ) -> Result<Context, Error> {
        let (next, resolve_uri, _) = self.find_next().ok_or(ContextResolverError::NoNext)?;

        match &next.resolve {
            Resolve::Uri(uri) => info!(
                "[DJDBG] Resolve::Uri: original_uri={uri:?}, context_uri={:?}, resolve_uri={resolve_uri:?}, update={:?}",
                next.context_uri(),
                next.update
            ),
            Resolve::Context(ctx) => {
                info!(
                    "[DJDBG] Resolve::Context: context_uri={:?}, resolve_uri={resolve_uri:?}, update={:?}",
                    next.context_uri(),
                    next.update
                );
                log_context_for_dj_debug("Resolve::Context input", ctx);
            }
        }

        // TransferState for Spotify DJ contains a one-track context. The page
        // is authoritative for the hand-off, but it usually omits the
        // per-track narration fields. When Spotify supplies its session HM
        // URL, resolve that URL in this same session and merge the transferred
        // track back into the response. If the session endpoint rejects the
        // request, retain the direct page so music playback still works.
        if let Resolve::Context(ctx) = &next.resolve {
            let is_dj = ctx
                .metadata
                .get(DJ_CONTEXT_METADATA_KEY)
                .is_some_and(|value| value == DJ_CONTEXT_METADATA_VALUE);
            if is_dj
                && !ctx.pages.is_empty()
                && ctx.pages.iter().all(|page| !page.tracks.is_empty())
            {
                if let Some((metadata_key, context_url)) = find_hm_context_url(ctx) {
                    info!(
                        "[DJDBG] resolving DJ session context through SpClient: metadata_key={metadata_key:?}, url={context_url:?}"
                    );

                    match self
                        .session
                        .spclient()
                        .get_context_from_hm_url(context_url)
                        .await
                    {
                        Ok(resolved)
                            if resolved.pages.iter().any(|page| !page.tracks.is_empty()) =>
                        {
                            let resolved = merge_direct_dj_context(resolved, ctx);
                            log_context_for_dj_debug("merged DJ session context", &resolved);
                            return Ok(resolved);
                        }
                        Ok(_) => warn!(
                            "[DJDBG] DJ session context returned no tracks; using transfer context"
                        ),
                        Err(err) => warn!(
                            "[DJDBG] DJ session context resolution failed; using transfer context: {err:#?}"
                        ),
                    }
                }

                log_context_for_dj_debug("using direct context", ctx);
                return Ok(ctx.clone());
            }
        }

        match next.update {
            ContextType::Default => {
                // Pagination cursors returned by Lexicon are opaque `hm://`
                // URLs. They are already the complete resolver endpoint and
                // must not be sent through the regular Spotify-context URI
                // endpoint (which would treat the HM URL as a malformed
                // Spotify URI and lose the session cursor).
                let mut ctx = if resolve_uri.starts_with("hm://") {
                    info!("[DJDBG] resolving HM pagination cursor: {resolve_uri:?}");
                    if matches!(next.action, ContextAction::Append) {
                        let page = self
                            .session
                            .spclient()
                            .get_context_page_from_hm_url(resolve_uri)
                            .await?;
                        Context {
                            pages: vec![page],
                            ..Default::default()
                        }
                    } else {
                        self.session
                            .spclient()
                            .get_context_from_hm_url(resolve_uri)
                            .await?
                    }
                } else {
                    self.session.spclient().get_context(resolve_uri).await?
                };
                log_context_for_dj_debug("get_context response before rewrite", &ctx);

                if !resolve_uri.starts_with("hm://")
                    && let Some((metadata_key, context_url)) = find_hm_context_url(&ctx)
                {
                    info!(
                        "[DJDBG] resolving hm context through SpClient: metadata_key={metadata_key:?}, url={context_url:?}"
                    );

                    let original_metadata = ctx.metadata.clone();
                    let mut resolved = self
                        .session
                        .spclient()
                        .get_context_from_hm_url(context_url)
                        .await
                        .map_err(|err| {
                            warn!(
                                "[DJDBG] hm context resolution failed: url={context_url:?}, error={err:#?}"
                            );
                            err
                        })?;

                    // The wrapper response carries DJ presentation metadata that
                    // may not be repeated by the Lexicon response.
                    for (key, value) in original_metadata {
                        resolved.metadata.entry(key).or_insert(value);
                    }

                    log_context_for_dj_debug("hm context response", &resolved);
                    ctx = resolved;
                }

                // A pagination cursor is not a context identity. Keep the
                // URI supplied by the Lexicon response (if any) instead of
                // exposing the opaque HM token as a track context.
                if !resolve_uri.starts_with("hm://") {
                    ctx.uri = Some(next.context_uri().to_string());
                    ctx.url = ctx.uri.as_ref().map(|s| format!("context://{s}"));
                }
                Ok(ctx)
            }
            ContextType::Autoplay => {
                if resolve_uri.contains("spotify:show:") || resolve_uri.contains("spotify:episode:")
                {
                    // autoplay is not supported for podcasts
                    Err(ContextResolverError::NotAllowedContext(
                        resolve_uri.to_string(),
                    ))?
                }

                let request = AutoplayContextRequest {
                    context_uri: Some(resolve_uri.to_string()),
                    recent_track_uri: recent_track_uri(),
                    ..Default::default()
                };
                self.session.spclient().get_autoplay_context(&request).await
            }
        }
    }

    pub fn mark_next_unavailable(&mut self) {
        if let Some((next, _, _)) = self.find_next() {
            self.unavailable_contexts
                .insert(next.clone(), Instant::now());
        }
    }

    pub fn apply_next_context(
        &self,
        state: &mut ConnectState,
        mut context: Context,
    ) -> Result<Option<Vec<ResolveContext>>, Error> {
        let (next, _, _) = self.find_next().ok_or(ContextResolverError::NoNext)?;

        // DJ session pages are dynamic queue batches, not ordinary context
        // pages. Keep them out of `next_tracks` so the session-specific queue
        // (and its narration metadata) remains authoritative; the caller
        // merges these tracks into `dj_next_tracks` after the current context
        // transition has finished. Still enqueue each page cursor so the
        // following batch is resolved in the same Lexicon session.
        let is_dj_append = matches!(next.action, ContextAction::Append)
            && (state.is_dj_context()
                || context
                    .metadata
                    .get(DJ_CONTEXT_METADATA_KEY)
                    .is_some_and(|value| value == DJ_CONTEXT_METADATA_VALUE));
        if is_dj_append {
            let continuations = context
                .pages
                .iter()
                .filter_map(|page| page.next_page_url.as_deref())
                .filter(|url| !url.is_empty())
                .map(ResolveContext::append_context)
                .collect::<Vec<_>>();
            return Ok((!continuations.is_empty()).then_some(continuations));
        }

        let remaining = match next.action {
            ContextAction::Append if context.pages.len() == 1 => state
                .fill_context_from_page(context.pages.remove(0))
                .map(|_| None),
            ContextAction::Replace => {
                let remaining = state.update_context(context, next.update);
                if let Resolve::Context(ref ctx) = next.resolve {
                    state.merge_context(ctx.pages.clone().pop());
                }

                remaining
            }
            ContextAction::Append => {
                warn!("unexpected page size: {context:#?}");
                Err(ContextResolverError::UnexpectedPagesSize(context.pages.len()).into())
            }
        }?;

        Ok(remaining.map(|remaining| {
            remaining
                .into_iter()
                .map(ResolveContext::append_context)
                .collect::<Vec<_>>()
        }))
    }

    pub fn try_finish(
        &self,
        state: &mut ConnectState,
        transfer_state: &mut Option<TransferState>,
    ) -> bool {
        let (next, _, _) = match self.find_next() {
            None => return false,
            Some(next) => next,
        };

        // when there is only one update type, we are the last of our kind, so we should update the state
        if self
            .queue
            .iter()
            .filter(|resolve| resolve.update == next.update)
            .count()
            != 1
        {
            return false;
        }

        match (next.update, state.active_context) {
            (ContextType::Default, ContextType::Default) | (ContextType::Autoplay, _) => {
                debug!(
                    "last item of type <{:?}>, finishing state setup",
                    next.update
                );
            }
            (ContextType::Default, _) => {
                debug!("skipped finishing default, because it isn't the active context");
                return false;
            }
        }

        let active_ctx = state.get_context(state.active_context);
        let res = if let Some(transfer_state) = transfer_state.take() {
            state.finish_transfer(transfer_state)
        } else if state.shuffling_context() && next.update == ContextType::Default {
            state.shuffle_new()
        } else if matches!(active_ctx, Ok(ctx) if ctx.index.track == 0) {
            // has context, and context is not touched
            // when the index is not zero, the next index was already evaluated elsewhere
            let ctx = active_ctx.expect("checked by precondition");
            let idx = ConnectState::find_index_in_context(ctx, |t| {
                state.current_track(|c| t.uri == c.uri)
            })
            .ok();

            state.reset_playback_to_position(idx)
        } else {
            state.fill_up_next_tracks()
        };

        if let Err(why) = res {
            error!("setup of state failed: {why}, last used resolve {next:#?}")
        }

        state.update_restrictions();
        state.update_queue_revision();

        true
    }
}

#[cfg(test)]
mod tests {
    use super::merge_direct_dj_context;
    use crate::protocol::{
        context::Context, context_page::ContextPage, context_track::ContextTrack,
    };
    use std::collections::HashMap;

    #[test]
    fn merges_transfer_track_and_keeps_it_first() {
        let direct = Context {
            uri: Some("spotify:playlist:dj".to_string()),
            url: Some("context://spotify:playlist:dj".to_string()),
            metadata: HashMap::from([("lexicon_set_type".to_string(), "your_dj".to_string())]),
            pages: vec![ContextPage {
                tracks: vec![ContextTrack {
                    uri: Some("spotify:unknown:current".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let resolved = Context {
            uri: Some("spotify:playlist:other".to_string()),
            pages: vec![ContextPage {
                tracks: vec![
                    ContextTrack {
                        uri: Some("spotify:track:next".to_string()),
                        ..Default::default()
                    },
                    ContextTrack {
                        uri: Some("spotify:track:current".to_string()),
                        metadata: HashMap::from([(
                            "narration.intro.ssml".to_string(),
                            "<speak>Welcome</speak>".to_string(),
                        )]),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        let merged = merge_direct_dj_context(resolved, &direct);
        let page = &merged.pages[0];
        assert_eq!(page.tracks[0].uri.as_deref(), Some("spotify:track:current"));
        assert_eq!(page.tracks[1].uri.as_deref(), Some("spotify:track:next"));
        assert_eq!(merged.uri.as_deref(), Some("spotify:playlist:dj"));
        assert_eq!(
            merged.metadata.get("lexicon_set_type").map(String::as_str),
            Some("your_dj")
        );
    }
}
