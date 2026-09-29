use crate::{
    core::{Error, Session},
    protocol::{
        autoplay_context_request::AutoplayContextRequest, context::Context,
        transfer_state::TransferState,
    },
    state::{ConnectState, context::ContextType},
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

const LEXICON_PREFIX: &str = "hm://lexicon-session-provider/";

fn dj_context_url(context: &Context) -> Option<&str> {
    (context.metadata.get("lexicon_set_type").map(String::as_str) == Some("your_dj"))
        .then_some(context.url.as_deref())
        .flatten()
        .filter(|url| url.starts_with(LEXICON_PREFIX))
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
            Resolve::Context(ref ctx) => dj_context_url(ctx)
                .or_else(|| ConnectState::find_valid_uri(ctx.uri.as_deref(), ctx.pages.first())),
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
    dj_context_uri: Option<String>,
    dj_next_page_url: Option<String>,
}

// time after which an unavailable context is retried
const RETRY_UNAVAILABLE: Duration = Duration::from_secs(3600);

impl ContextResolver {
    pub fn pending(&self) -> VecDeque<ResolveContext> {
        self.queue.clone()
    }

    pub fn restore_pending(&mut self, pending: VecDeque<ResolveContext>) {
        self.queue = pending;
        self.unavailable_contexts.clear();
    }

    pub fn new(session: Session) -> Self {
        Self {
            session,
            queue: VecDeque::new(),
            unavailable_contexts: HashMap::new(),
            dj_context_uri: None,
            dj_next_page_url: None,
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
        self.queue = VecDeque::new();
        self.dj_context_uri = None;
        self.dj_next_page_url = None;
    }

    pub fn dj_active(&self) -> bool {
        self.dj_context_uri.is_some()
    }

    pub fn add_dj_page(&mut self) {
        if let Some(url) = self.dj_next_page_url.clone() {
            self.add(ResolveContext::append_context(url));
        }
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

        match next.update {
            ContextType::Default => {
                if resolve_uri.starts_with(LEXICON_PREFIX) {
                    if next.action == ContextAction::Append {
                        let page = self
                            .session
                            .spclient()
                            .get_context_page_url(resolve_uri)
                            .await?;
                        return Ok(Context {
                            uri: self.dj_context_uri.clone(),
                            pages: vec![page],
                            ..Default::default()
                        });
                    }
                    let mut ctx = self.session.spclient().get_context_url(resolve_uri).await?;
                    if let Resolve::Context(ref original) = next.resolve {
                        ctx.metadata.extend(original.metadata.clone());
                    }
                    ctx.uri = Some(next.context_uri().to_string());
                    ctx.url = Some(resolve_uri.to_string());
                    return Ok(ctx);
                }
                let mut ctx = self.session.spclient().get_context(resolve_uri).await?;
                if ctx.pages.iter().all(|page| page.tracks.is_empty())
                    && let Some(url) = ctx.metadata.get("lexicon_context_url").cloned()
                    && url.starts_with(LEXICON_PREFIX)
                {
                    let metadata = ctx.metadata;
                    ctx = self.session.spclient().get_context_url(&url).await?;
                    ctx.metadata.extend(metadata);
                    ctx.url = Some(url);
                } else {
                    ctx.url = Some(format!("context://{}", next.context_uri()));
                }
                ctx.uri = Some(next.context_uri().to_string());
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
        &mut self,
        state: &mut ConnectState,
        mut context: Context,
    ) -> Result<Option<Vec<ResolveContext>>, Error> {
        let (next, _, _) = self.find_next().ok_or(ContextResolverError::NoNext)?;

        let dj_replace = next.action == ContextAction::Replace
            && context.metadata.get("lexicon_set_type").map(String::as_str) == Some("your_dj");
        let dj_append = next.action == ContextAction::Append
            && next
                .resolve_uri()
                .is_some_and(|uri| uri.starts_with(LEXICON_PREFIX));
        let dj_context_uri = context.uri.clone();
        let dj_next_page_url = context
            .pages
            .last()
            .and_then(|page| page.next_page_url.as_deref())
            .filter(|url| url.starts_with(LEXICON_PREFIX))
            .map(str::to_owned);

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

        if dj_replace {
            self.dj_context_uri = dj_context_uri;
            self.dj_next_page_url = dj_next_page_url;
        } else if dj_append {
            self.dj_next_page_url = dj_next_page_url;
        } else if next.action == ContextAction::Replace {
            self.dj_context_uri = None;
            self.dj_next_page_url = None;
        }

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
mod dj_tests {
    use super::*;

    #[test]
    fn dj_transfer_resolves_its_lexicon_url_instead_of_the_empty_playlist() {
        let url = "hm://lexicon-session-provider/context-resolve/v2/session?contextUri=spotify:playlist:dj";
        let context = Context {
            uri: Some("spotify:playlist:dj".into()),
            url: Some(url.into()),
            metadata: [("lexicon_set_type".into(), "your_dj".into())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let resolve =
            ResolveContext::from_context(context, ContextType::Default, ContextAction::Replace);
        assert_eq!(resolve.resolve_uri(), Some(url));
        assert_eq!(resolve.context_uri(), "spotify:playlist:dj");
    }

    #[test]
    fn ordinary_playlist_does_not_use_lexicon_url() {
        let context = Context {
            uri: Some("spotify:playlist:ordinary".into()),
            url: Some("hm://lexicon-session-provider/irrelevant".into()),
            ..Default::default()
        };
        let resolve =
            ResolveContext::from_context(context, ContextType::Default, ContextAction::Replace);
        assert_eq!(resolve.resolve_uri(), Some("spotify:playlist:ordinary"));
    }
}
